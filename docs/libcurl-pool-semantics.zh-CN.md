# libcurl 连接池语义实测与方案（P0 门槛）

日期：2026-09-16；P3 更新：2026-09-12（按本机时钟）。状态：P0 实验完成，P3 已按本文方案实现，
P3 验收复审的 4 项修正见第 6 节。

本文记录 `docs/libcurl-migration-plan.zh-CN.md` §4.4 要求的“可运行的池语义验证”：
把 `curl`/`Multi` 的真实行为测出来，再决定 `pool_max_idle_per_host` 与
`pool_idle_timeout` 如何落到 libcurl 选项上，避免静默近似旧 API 契约。
第 5 节记录 P3 的实际实现与新增验证，第 6 节记录复审修正后的最终语义。

## 1. 实验环境与方法

- 依赖：`curl` `0.4.50`，`curl-sys` `0.4.90+curl-8.21.0`，特性 `static-curl`
  （vendored libcurl）。
- 运行时实测特性（`curl::runtime`）：

  ```text
  libcurl 8.21.0-DEV (vendored=true) host=unknown tls=Schannel zlib=1.3.2 http2=false http3=false https_proxy=true ipv6=true async_dns=true
  ```

- 测量代码：`src/network/curl/pool_semantics.rs`。每个实验连接本地 HTTP/1.1
  计数服务器，记录“接受的连接数”“每条连接上的请求数”“当前仍打开的连接数”
  “同时打开的峰值连接数”，并用 `CURLINFO_NUM_CONNECTS` 观察客户端是否真的新建连接。
- 复现命令：

  ```text
  cargo test --features curl-backend curl::pool_semantics
  cargo test --features curl-backend curl::pool_semantics -- --nocapture   # 含时间采样输出
  ```

## 2. 实测结果

| # | 实验 | 设置 | 观测结果 | 对映射的含义 |
| --- | --- | --- | --- | --- |
| 1 | 顺序请求复用 | 默认选项，3 次顺序请求 | 接受连接 1 个，该连接承载 3 个请求；第 2、3 次 `NUM_CONNECTS=0` | Multi 默认缓存空闲连接，可选择复用 |
| 2 | 关闭池 | `fresh_connect + forbid_reuse` | 每次请求新建连接（2 次请求 2 条连接），传输结束后连接被关闭 | 可用于实现 `pool_max_idle_per_host = 0` |
| 3 | `CURLMOPT_MAXCONNECTS = 1` | 两个不同 origin 各一次顺序请求，两次空闲时刻相隔 150 ms | 第一次传输后第一条连接仍缓存；第二次传输完成（连接变为空闲）时，**最久空闲**的那条被关闭，最终缓存 1 条 | 上界在“连接变为空闲”这一刻生效，且淘汰对象是空闲连接 |
| 3b | `CURLMOPT_MAXCONNECTS = 1` + 3 个并发请求 | 同 origin，响应延迟 150 ms | 峰值同时连接数 3、三次 `NUM_CONNECTS=1`（上界不参与并发控制）；全部完成后缓存收缩到 1 条 | 缓存上界不影响并发语义，收缩发生在完成之后 |
| 3c | 仅下调 `CURLMOPT_MAXCONNECTS` | 两条已缓存空闲连接，下调到 1 并等待 300 ms | 两条 socket 都保持打开；直到下一次“连接变为空闲”才淘汰一条 | 单靠该选项无法实现 `pool_idle_timeout` 的主动回收 |
| 4 | `CURLMOPT_MAX_HOST_CONNECTIONS = 1` | 同 origin 并发 3 次请求，响应 150 ms | 峰值同时连接数 1，接受连接 1 个，总耗时 ≥ 400 ms（3 次串行）；后续请求 `NUM_CONNECTS=0` | 该选项限制“同一主机同时连接数”，会改变并发语义，**不能**当作空闲池上限 |
| 4b | `CURLMOPT_MAX_TOTAL_CONNECTIONS = 1` | 同 origin 并发 8 次请求 | 只建立 1 条连接，其余请求排队串行 | 同上：这是并发上限而不是缓存上界，也不是可行的空闲池上限 |
| 5 | `CURLOPT_MAXAGE_CONN = 1 s` | 第一次请求后等待 1.5 s | 旧连接不被复用（第 2 次 `NUM_CONNECTS=1`，接受连接 2 个）；等待 1.5 s 期间旧 socket 仍处于打开状态 | 只在“尝试复用”时检查年龄，不主动关闭空闲连接；无法直接实现 `pool_idle_timeout` 的主动回收 |
| 6 | `CURLOPT_RESOLVE` 重复注入 | 同一 origin 连续两次请求，均注入同一地址 | 第二次请求复用第一次的连接（`NUM_CONNECTS=0`） | 已有缓存连接不会按新的 RESOLVE 条目重建，多 IP/固定 IP 实验必须隔离 pool |
| 7 | 销毁 `Multi` | 请求完成后 `drop(multi)` | 空闲连接被关闭（服务器观察为 0 条打开连接） | “完全空闲的池可直接销毁”这一回收路径可行 |
| 8 | `CURLMOPT_NETWORK_CHANGED \| CURLMNWC_CLEAR_CONNS` | 一条连接正在滴流传输（96 KiB），另一条已空闲 | 空闲连接立刻被关闭；**进行中的传输继续使用原连接并完整传完 96 KiB** | 这是唯一能在“池仍在运行”时主动关闭其空闲 socket 的公开 API（libcurl 8.21+），`pool_idle_timeout` 的活跃池回收依赖它 |

## 3. 结论（P0 需要明确的方案）

采用计划 §4.4 的**方案 1**：在同一 driver 内按“origin + 代理路由 + 网络配置”分池，
保持现有 API 契约，不用 libcurl 选项直接顶替旧参数。

1. `pool_max_idle_per_host = 0`
   - 对该池的传输设置 `CURLOPT_FRESH_CONNECT` 与 `CURLOPT_FORBID_REUSE`，实测每个
     请求新建连接且用后关闭（实验 2）。
2. `pool_max_idle_per_host = k > 0`
   - **不**使用 `CURLMOPT_MAX_HOST_CONNECTIONS`（实验 4 证明它会串行化并发请求，
     等于偷偷改变 `max_connections` 行为），也不使用 `CURLMOPT_MAX_TOTAL_CONNECTIONS`
     （实验 4b 证明它同样限制并发，而不是缓存）。
   - 用 `CURLMOPT_MAXCONNECTS = k` 作为该 multi 的连接缓存上界：libcurl 在“连接变为空闲”
     时检查 `缓存条数 > 上界` 并关闭最久空闲的连接（实验 3/3b），因此在“一个池 = 一个 origin”
     的前提下它正好表达“最多缓存 k 条空闲连接”，且不会关闭 in-use 连接、不影响并发。
   - 关闭池（k = 0）之外不改变并发上限；并发仍由调度器与 lease 控制。
3. `pool_idle_timeout`
   - 不能只依赖 `CURLOPT_MAXAGE_CONN`：它只在复用（或新建连接触发死连接清理）时检查年龄
     （实验 5）。
   - 池**完全空闲**后启动回收计时，超过 `pool_idle_timeout` 就销毁该池的 `Multi`
     （实验 7 证明销毁会关闭空闲 socket）。
   - 池**仍在忙**（例如同源一个长传输）时不能销毁，改用
     `CURLMOPT_NETWORK_CHANGED | CURLMNWC_CLEAR_CONNS` 关闭其中已空闲的连接（实验 8），
     计时同样从“某条连接变为空闲”开始（复审修正 4）。
   - 复用时仍设置 `MAXAGE_CONN`（取 `min(pool_idle_timeout, 默认值)` 的秒数）作为
     第二道防线，避免极端情况下复用到超龄连接；注意该选项按秒截断，小于 1 s 会被
     截成 0。
4. 分池键
   - 池键至少包含 origin、代理路由（直连/HTTP 代理/HTTPS 代理及其端点）与
     `ClientNetworkConfig` 中影响连接的字段，避免跨配置复用连接。
   - RESOLVE 注入不能用于“给已有池换地址”（实验 6），固定 IP / 多 IP 调度需要独立
     pool 或独立 driver，属于后续设计，不作为 P0 结论。
5. 语义差异需要写入文档
   - libcurl 的池上限是“Multi 连接缓存条数”，旧配置是“每主机空闲连接上限”。当
     一个池对应一个 origin 时两者基本等价（空闲条数不超过 k），但 **k 小于该 origin 的并发
     连接数时，连接会随空闲被关闭并在下次请求时重建**：需要复用并发连接的场景应让
     `pool_max_idle_per_host ≥ max_connections`（默认 4 对 4）。该要求已写入
     `ClientNetworkConfig::http_idle_pool` 文档，不能静默近似。

## 4. 待 P3 验证的后续问题

- `CURLMOPT_MAXCONNECTS` 动态更新在有活动传输时的安全性与生效时机：已由实验 3/3b/3c
  与 `max_connects_can_shrink_while_transfers_are_active` 回答（见第 5/6 节）。
- 空闲回收的时间粒度（driver 等待分片为 20 ms，回收判定可按需降频）。
- 代理模式下池键的粒度（按代理解析目标域名时，origin 与代理端点的组合）。
- 多 IP 调度需要的新池模型（结合 `docs/multi-ip-connection-plan.zh-CN.md`）。

## 5. P3 实现与新增验证（2026-09-12）

实现位置：`src/network/curl/driver/mod.rs`（`Pool`、`PoolKey`、`DriverConfig`）与
`src/network/curl/transport.rs`（`DriverConfig` 由 `ClientNetworkConfig` 构造）。

1. **分池键**：`PoolKey { origin: "scheme://host:port", proxy: Option<proxy URL> }`。
   路径不参与（同源共享连接），scheme、端口与代理端点参与。一个 driver 只属于一个
   `ClientNetworkConfig`，因此影响连接的配置差异天然落在不同 driver 中。
2. **`pool_max_idle_per_host = k`**：每个池创建时对 `Multi` 调用
   `CURLMOPT_MAXCONNECTS = k`（复审修正 2 改用正确的 API 与取值）；不使用
   `CURLMOPT_MAX_HOST_CONNECTIONS`，也不使用 `CURLMOPT_MAX_TOTAL_CONNECTIONS`。
   `k = 0` 时改为对每个传输设置 `CURLOPT_FRESH_CONNECT` + `CURLOPT_FORBID_REUSE`，
   并在池空闲时立即销毁该池（保持旧的“不复用连接”契约）。
3. **`pool_idle_timeout`**：池内最后一个传输结束后记录 `idle_since`，driver 在等待
   命令时以“最近的回收时刻”为上限阻塞（无传输时不会忙轮询），到点即销毁该池的
   `Multi`，从而关闭其空闲 socket。池仍在忙时按**连接**记录空闲起始时刻（`Pool::idle`
   小顶堆，见 §6 的记账规则），到点调用
   `CURLMOPT_NETWORK_CHANGED | CURLMNWC_CLEAR_CONNS` 关闭其空闲连接（复审修正 4、5）。
   `CURLOPT_MAXAGE_CONN` 取 `min(pool_idle_timeout, 118 s)` 且仅在 ≥ 1 s 时设置
   （该选项按秒截断），作为复用前的第二道防线。
4. **等待与回收粒度**：命令通道改为 `Mutex<VecDeque<Command>> + Condvar`（`CommandQueue`），
   因为 driver 需要“带截止时间的阻塞等待”，而 tokio mpsc 只能无限阻塞或忙轮询。
   有传输时只在**有活动传输的池**上按 `min(libcurl timer, 20 ms)` 调用 `Multi::poll`
   （P5：无描述符时也按超时有界等待；空闲池没有可等待描述符，选中它会让循环空转）。
   命令入队同时通过当前池的 wakeup socket（`Multi::waker`）打断阻塞中的 `poll`，
   登记 waker 后重查命令队列及关闭状态，有待处理状态就跳过等待，避免登记前的唤醒丢失。
   排队延迟不再受 20 ms 分片限制；无传输时等待命令或最近的回收时刻。
   命令排队延迟记入 `DriverStats::max_command_latency`。
5. **RESOLVE 与池的交互**：注入条目按 `host:port` 记在池内，TTL 见
   `src/network/dns.rs` 的 `DnsAnswer`。TTL 到期或地址变化时先发 `-host:port` 再发新条目；
   地址变化额外对该传输设置 `CURLOPT_FRESH_CONNECT`，避免复用指向旧地址的连接
   （实验 6 的结论由此落实为可测试行为）。`CURLMOPT_MAXCONNECTS` 的动态更新、代理池键
   与回收粒度都在第 5 节末尾的测试中覆盖。

新增/强化的可执行验证（`cargo test --features curl-backend curl::`）：

- `pool_keys_separate_origins_and_proxy_routes`：同源不同路径同池；端口、scheme、
  代理端点分别分池。
- `resolve_plans_inject_once_and_refresh_stale_or_changed_answers`：首次注入、TTL 内不重复
  注入、TTL 到期先删后加、地址变化要求新建连接。
- `an_uncommitted_plan_does_not_claim_the_answer_was_injected`：`add2` 失败时不留幻影缓存。
- `a_changed_resolve_answer_opens_a_new_connection`：端到端（127.0.0.1 → 127.0.0.2 同一端口）
  验证新地址不会复用旧连接，旧监听只接受一次连接。
- `idle_pools_close_their_sockets_after_the_idle_timeout`：没有后续请求时，服务器也能观察到
  空闲 socket 被关闭（证明回收来自池销毁，而不是 `MAXAGE_CONN`）。
- `a_pool_that_may_not_reuse_connections_keeps_none_idle`：`k = 0` 时池立即释放。
- `the_connection_cache_keeps_at_most_the_configured_idle_connections`：`k = 1` 时 8 个并发
  请求各自建连（并发不受限），全部结束后服务器只观察到 1 条连接（复审修正 2）。
- `a_busy_pool_closes_connections_left_idle_for_the_idle_timeout`：同源长传输仍在进行时，
  另一条空闲连接在 `pool_idle_timeout` 后被关闭，长传输的 body 完整（复审修正 4），
  `DriverStats::idle_clears` 记为 1。
- `a_request_does_not_postpone_the_deadline_of_connections_it_does_not_use`：同池一个长传输 +
  两条空闲连接，新请求只复用其中一条时，另一条仍按**自己的**到期时间被关闭（复审修正 5）；
  `reusing_one_connection_keeps_the_other_idle_deadlines` 用确定性单测固定这套记账规则
  （领取/退回/按连接到期/清理后作废）；
  `fresh_connections_preserve_the_deadline_of_unused_cached_connections` 覆盖强制新建连接的
  两条触发路径（`forbid_reuse` 与 DNS 答案变化）。
- `driver_stats_separate_requests_from_connections`：两次请求只建连一次，
  `submitted = 2`、`connections = 1`。
- `pools_are_counted_per_origin`：两个 origin 各自成池。
- `pool_semantics::max_connects_can_shrink_while_transfers_are_active`：在两个传输
  同时进行时把 `CURLMOPT_MAXCONNECTS` 调到 1，两个 body 仍完整、峰值并发仍为 2，
  说明动态更新不会中断在途传输。

仍未覆盖：代理模式下“由代理解析目标域名”时池键的粒度只做到了“按代理端点分池”，
更细的按目标域名分池与多 IP 调度仍属后续设计；Linux/macOS 上的池行为需要 CI 矩阵复验。

## 6. 复审修正后的最终语义（2026-09-12）

复审（`docs/libcurl-migration-plan.zh-CN.md` §5「P3 验收复审与修正」）先后发现并修正了 5 项问题，
其中两项改变本文第 3/5 节的结论，此处汇总最终生效的语义：

1. **`pool_max_idle_per_host = k` 的语义**：`CURLMOPT_MAXCONNECTS = k`（不是
   `CURLMOPT_MAX_TOTAL_CONNECTIONS`，也不是 `活跃 + k`）。
   - 生效时刻：某条连接变为空闲（即一次传输结束）时，若缓存条数 > k，libcurl 关闭**最久空闲**
     的那条；in-use 连接永不被关闭，因此不会限制并发（实验 3/3b）。
   - 仅下调 k 不会关闭已经空闲的连接（实验 3c）——所以主动回收必须另有机制。
   - 两条连接在同一毫秒内变为空闲时，谁是“最久空闲”由内部迭代顺序决定；driver 只依赖
     “最终空闲条数 ≤ k”，不依赖具体淘汰对象。
   - 代价：同一 origin 的并发连接数大于 k 时，连接会随空闲被关闭、下次请求重建。默认配置
     `pool_max_idle_per_host = 4` ≥ `max_connections = 4`，不受影响；
     `ClientNetworkConfig::http_idle_pool` 的文档写明了这一关系。
2. **`pool_idle_timeout` 的语义**：
   - 池无传输：记录 `idle_since`，到期销毁池（关闭其全部 socket）。
   - 池有传输：**按连接**记录空闲起始时刻（`Pool::idle`，一个小顶堆），到期时对最久空闲的那条
     调用 `CURLMOPT_NETWORK_CHANGED | CURLMNWC_CLEAR_CONNS` 关闭空闲连接，进行中的传输不受影响
     （实验 8）。
   - libcurl 只报告「本次传输新建了几条连接」（`CURLINFO_NUM_CONNECTS`），所以 driver 自己记账：
     传输加入池时**领取**一条空闲记录（libcurl 优先复用最久空闲的连接），完成时若 `NUM_CONNECTS = 0`
     表示确实复用了它——该连接带着**新的**空闲时刻回到集合，其余连接的到期时间不变；若 > 0 表示它
     自己新建了连接，则把领取的记录**原样退回**；失败的传输会关掉自己的连接（记录作废），被取消的
     传输则退回尚未使用的领取记录。清理成功后整批记录清空并递增 `clear_generation`，清理时正在
     运行的传输因此不会声称自己的连接被缓存了。
   - 因此**提交或完成一个新请求不会推迟其他连接的回收到期时间**：只有真正被复用的那条连接会更新
     自己的时间（复审修正 5）。
   - 兼容性：该选项要求 libcurl ≥ 8.21；老版本返回 `CURLM_UNKNOWN_OPTION`，driver 记录一次并退回
     “池空闲才销毁 + `MAXAGE_CONN`”的行为。本仓库 vendored libcurl 为 `8.21.0-DEV`，实验 8
     即在该版本上测出。
3. **连接阶段预算**：`min(connect_timeout, 响应头 deadline)` 同时覆盖 Hickory 解析与 libcurl
   建连，解析耗时从预算中扣除（复审修正 3，详见计划 §5）。
