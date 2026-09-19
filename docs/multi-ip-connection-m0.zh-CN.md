# 多 IP 建连 M0 原型结论（libcurl CONNECT_TO）

日期：2026-09-19。状态：M0 实验完成，首选架构（同一 Multi + `CURLOPT_CONNECT_TO`）成立；
driver 侧按 IP 的空闲记账尚未实现，仍需在 M1 里补验收实验。

本文记录 [多 IP 建连与连接复用收敛计划](multi-ip-connection-plan.zh-CN.md) §8 中
M0 门槛的实验结果。计划 §2–§4 的设计依据是 libcurl 源码与官方文档；本文用真实
本地服务器把它们测出来，并给出“协议与回收是否可信”的可复现结论。
相关文档：[连接池语义实测](libcurl-pool-semantics.zh-CN.md)、
[libcurl 迁移计划](libcurl-migration-plan.zh-CN.md)。

## 1. 实验环境与方法

- 依赖：`curl` `0.4.50`，`curl-sys` `0.4.90+curl-8.21.0`，特性 `static-curl`。
- 运行时实测特性（`curl::runtime`，本次记录机器）：

  ```text
  libcurl 8.21.0-DEV (vendored=true) host=unknown tls=OpenSSL/3.0.13 zlib=1.3 http2=false http3=false https_proxy=true ipv6=true async_dns=true
  ```

- 测量代码：`src/network/curl/pool_semantics.rs`（M0 部分）。服务器
  （`DualAddressServer`）在 **同一个端口** 上同时监听 `127.0.0.1` 与 `127.0.0.2`：

  - 同一端口是关键：`CONNECT_TO` 以 URL 的 `host:port` 匹配，两个候选目标因此
    只差一个地址，正是策略要做的选择；
  - URL 使用无法解析的 `dual.test`，请求只能通过 `CONNECT_TO` 到达监听者，
    条目一旦失效会表现为 DNS 失败而不是静默连到别处；
  - 每个地址独立记录“接受的连接数”“每条连接上的请求数”“同时打开的峰值连接数”，
    响应体为 `{地址标签}{连接序号}`（如 `a0`、`b1`），据此分辨“哪个地址”与“哪条 socket”；
  - 客户端侧同时读取 `CURLINFO_NUM_CONNECTS`、`CURLINFO_PRIMARY_IP` 与
    `CURLINFO_CONN_ID`（`curl-sys` 未绑定，按公开头文件补最小 getinfo FFI）。

- `pool_semantics` 收尾处新增 `ConnectToEntry`（`src/network/curl/driver/mod.rs`）
  负责渲染 `host:port:connect-to-host:connect-to-port`，M1 的 driver 直接复用，
  原型与计划不各写一份。
- 复现命令：

  ```text
  cargo test --features curl-backend curl::pool_semantics
  cargo test --features curl-backend curl::pool_semantics -- --ignored   # 含 TLS 两项
  ```

## 2. 实测结果

| # | 实验 | 设置 | 观测结果 | 对方案的含义 |
| --- | --- | --- | --- | --- |
| 1 | `CONNECT_TO` 定向与复用 | 同 origin 顺序请求 A → B → A | 三次请求分别落在 `a0`、`b0`、`a0`；`NUM_CONNECTS` = 1、1、0；A 的 socket 承载 2 个请求，B 承载 1 个；第 1、3 次 `CONN_ID` 相同且与第 2 次不同；`PRIMARY_IP` 分别为 `127.0.0.1`、`127.0.0.2` | §3 首选方案成立：定向是“每个 easy handle 独立”的，换回同一地址会复用该地址的连接，不需要 fresh-connect |
| 2 | 连接身份的作用域 | 请求 → 销毁 Multi → 重建 Multi 再请求同一地址 | 第二次是**新的** socket（服务器观察到 2 条连接、响应体 `a0`/`a1`），但两次 `CONN_ID` 数值相同 | `CONN_ID` 只在单个连接缓存内唯一；跨 Multi 重建必须换代，计划 §4 的 `(driver_id, pool_generation, conn_id)` 是必需的，不是保险 |
| 3 | 并发不串目标 | 两个 handle 同时挂同一 Multi，一个指向 A、一个指向 B | 响应分别来自 `a0`、`b0`；两个地址各自峰值并发 1、各接受 1 条连接 | `RESOLVE` 无法做到这一点（共享 DNS 缓存只能有一个值，见实验 6 与旧池实验）；`CONNECT_TO` 可以 |
| 4 | 请求本身不变 | 指向 B 的请求 | 请求行为 `GET /origin-name`，请求头为 `host: dual.test:<port>` | 定向只改连接目标：`Host`（以及 TLS 的 SNI/证书校验，见实验 9/10）始终来自 URL |
| 5 | 缓存上界按 origin 共享 | `CURLMOPT_MAXCONNECTS = 1`，先 A 后 B，两次空闲时刻相隔 150 ms | 第一次传输后 A 的 socket 仍缓存（A 存活 1）；第二次传输完成（B 变为空闲）时被淘汰，最终只有 B 存活 | 上界整池共享，**不会**按候选数量放大成“每个 IP 各 k 条”，符合计划 §6 的口径 |
| 6 | `pool_max_idle_per_host = 0` | `fresh_connect + forbid_reuse` 指向同一地址的两次请求 | `NUM_CONNECTS` = 1、1；响应体 `a0`、`a1`；服务器观察到 2 条连接，传输结束后 0 条存活 | `k = 0` 的“不复用”契约在定向路径上同样成立 |
| 7 | 缓存上界 ≠ 并发上界 | `MAXCONNECTS = 1`，3 个并发请求全部指向 A | 三次 `NUM_CONNECTS` 均为 1，A 的峰值并发 3，接受 3 条连接 | 与池语义实验 3b 一致：上界只在“连接变为空闲”时收缩，不排队、不串行化 |
| 8 | IPv6 目标 | `::1` 上的监听者，URL 仍是无法解析的名字 | 条目渲染为 `dual.test:<port>:[::1]:<port>`，请求成功（响应体 `v0`，`PRIMARY_IP` = `::1`） | 方括号形式正确且真的可达，双栈候选可用同一机制 |
| 9 | TLS 基于 URL 域名（正向） | 证书签发给 `localhost`，连接打到 `127.0.0.1` | 握手与证书校验通过（额外信任测试 CA），请求成功 | 定向到某个 IP 不会把校验对象换成该 IP |
| 10 | TLS 基于 URL 域名（反向） | 证书只签发给 `127.0.0.1`，URL 仍是 `localhost` 并定向到 `127.0.0.1` | 校验失败（curl 60），详细错误指向 `localhost` | 连接目标不会成为校验名：**不能**靠换 IP 绕过证书错误（计划 §6） |

实验 9/10 与既有 driver HTTPS 测试一样标了 `#[ignore]`（需要 TLS 可用的运行环境），
本次在 Linux/OpenSSL 上用 `--ignored` 实跑通过。

## 3. 结论

1. **首选架构确认**：同一 origin 保留一个 Multi，每个 easy handle 用单目标
   `CONNECT_TO` 定向，可以同时满足“定向准确”“换回同一 IP 复用该 IP 的连接”
   “并发不串目标”“请求与 TLS 语义不变”四项要求。备选方案（`PoolKey + selected_ip`
   独立 Multi）在 M0 不需要启用。
2. **连接身份**：`CURLINFO_CONN_ID` 可用，但只在单个连接缓存内唯一；同一条 socket
   由不同请求读到相同值，可用于“同一连接”的判定，跨 Multi 必须配代次。它与
   `NUM_CONNECTS`（估算复用/新建）互补：前者回答“是不是同一条连接”，后者回答
   “这次有没有新建连接”。
3. **缓存与回收口径**：`MAXCONNECTS` 是整池的缓存上界，多候选共享，不需要按候选
   复制；`k = 0` 与“上界不限制并发”两条既有语义在定向路径上都不变。计划 §6 的
   预算表格可以按现有实现口径执行，无需新增每 IP 配额。
4. **地址与协议语义**：`Host`、SNI、证书校验全部基于 URL，定向只改变连接目标；
   IPv6 目标用方括号渲染即可，无需特殊分支。

## 4. 尚未通过的门槛

计划 §8 要求“空闲记账或预算未过关，不进入策略启用阶段”。M0 只证明了机制可用，
以下两项仍**未**验证，M1 必须补做：

1. **driver 侧按 IP 的空闲记账**。当前 `Pool::idle` 是每池一个时间戳堆，
   `claim_idle_connection` 领取最久空闲的一条，用 `NUM_CONNECTS` 估算复用；
   它没有 IP 维度。定向之后，A 的请求可能领走 B 的空闲记录，从而推迟 B 的回收期限。
   实验 1/2 说明改用连接身份是可行的（`(generation, conn_id)` 能区分连接，
   而 `CONN_ID` 跨缓存会重号），但“按连接身份记账”本身还没有实现与验收。
   计划 §4 要求的两个用例——“A 持续忙、B 空闲到期”与“同 IP 多条空闲连接只复用一条，
   未使用的不得被别的请求延后回收”——必须在 driver 实验里跑通。
2. **活动预算与真实 socket 峰值**。计划 §6 要求区分“任务活动预算 / 地址尝试 /
   origin 缓存 / 物理连接”，并验证提交失败、取消确认不会泄漏额度。M0 未涉及
   driver 的 slot 与预算路径。

因此 M1 的范围应包含：开关与配置传播、候选快照与 TTL/代次、driver 原子选路与
`CONNECT_TO` 注入、有界串行回退，**以及**上面两项记账/预算的补测。策略启用
（排名、迟滞、探索，M3）在记账与预算过关之前不开始。

## 5. 附带修正

M0 新增的正向 TLS 实验最初失败，定位到既有测试夹具本身的问题：
`driver::tests::TlsFixture` 与 M0 夹具都用 rcgen 的默认可分辨名称，CA 与叶证书的
subject 完全相同，叶证书的 issuer 等于自身 subject，OpenSSL 因而把它当作
self-signed，无法串到测试 CA 上（`verify result: self-signed certificate (18)`）。
原先只有负向用例（预期失败）覆盖这条路径，所以问题一直没暴露；
`local_https_range_requests_reuse_one_verified_connection` 处于 `#[ignore]`，
没有在默认门禁里跑到。

修正方式：给 CA 一个独立的 `CN`（`bytehaul-test-ca` / `bytehaul-m0-ca`）。
修正后 driver 的 3 个 HTTPS 测试与 M0 的 2 个 TLS 实验全部通过。
