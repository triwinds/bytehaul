# 多 IP 建连与连接复用收敛计划（libcurl）

状态：重新设计，尚未实现多 IP 策略。更新：2026-09-17。

本文以当前 libcurl 生产后端为基线，替代原先“按 IP 分组的 Hyper client”方案。
旧原型及其测试、覆盖率仅作为[历史记录](multi-ip-connection-prototype.zh-CN.md)，不代表本方案已验收。

相关文档：[下载器调研](multi-ip-downloader-research.zh-CN.md)、
[libcurl 迁移计划](libcurl-migration-plan.zh-CN.md)、
[连接池语义实测](libcurl-pool-semantics.zh-CN.md)。

## 1. 结论与范围

首选 **保留每个 origin 的 Multi，在每个 easy handle 上用单目标
`CURLOPT_CONNECT_TO` 指向选中的 IP**。Hickory 提供候选和 TTL，
策略决定请求使用哪个 IP，libcurl 管理该目标的连接建立和复用。

不需要为每个 IP 创建 driver 或 Multi，也不会把 `pool_max_idle_per_host`
按候选数量复制。先做池语义原型，确认真实行为和空闲回收后再接入下载；
“origin + IP 独立 Multi”保留为原型失败后的备选。

目标仍是利用正常下载请求分散尝试候选，再根据实际传输表现，让后续请求主要使用少数快且稳定的 IP。
不发独立测速请求，不新增测速分片，不修改分片划分算法，不迁移正在持续下载的大请求。
小文件可能在取得可靠样本前已经完成，不能保证受益。

首版默认关闭，仅处理直连 HTTP/HTTPS、现有 HTTP/1.1 路径。
代理和 IP 字面量 URL 走原路径；已有慢速恢复负责在途恢复，IP 策略只选择后续请求目标。

## 2. 当前代码基线与改动位置

| 位置 | 已有能力 | 需要补充 |
| --- | --- | --- |
| `src/network/dns.rs` | Hickory、IPv4/IPv6 开关、带有效期的 `DnsAnswer` | 候选代次与失效处理 |
| `src/network/curl/transport.rs` | DNS、代理路由、剩余建连/响应头预算、`RequestOptions` | 提交候选快照与任务上下文；启用时绕过源站 RESOLVE 注入 |
| `src/network/curl/driver/mod.rs` | 一个客户端一个 driver；`PoolKey { origin, proxy }` 对应 Multi；HTTP/1.1，关闭自动重定向 | 选路、CONNECT_TO、真实连接归属、统计事件 |
| 同上 `complete_transfer/cancel_transfer` | `primary_ip`、`num_connects`、移除 handle 和正文终态 | 连接 ID、采样、取消确认与额度归还 |
| 同上 `BodySink` | 有界队列、pause/resume、实际接受字节数 | 背压区间及共享统计上下文 |
| `src/http/worker.rs` | 请求 ID、手动重定向、响应扩展 | 下传请求/任务标识，每跳重新选路 |
| `src/session/single.rs`、`src/session/multi/worker/` | 消费、限速、写入及恢复 | 本地约束标记和消费终态反馈 |
| `src/network.rs`、Python 绑定 | 网络配置与客户端缓存 | 默认关闭的开关、缓存键及绑定传播 |

一个 driver 属于一个 `ClientNetworkConfig`。不同代理、TLS、DNS 配置不得共享策略历史。
当前尚无 IP 排名或完整传输上下文；已有 worker/lease 数也不能直接作为全部物理 socket 的计数。

## 3. 定向连接与复用

### 首选：同一 Multi + CONNECT_TO

URL 保留为 `https://download.example/file.zip`，选择 `192.0.2.10` 时，
设置 `download.example:443:192.0.2.10:443`；IPv6 目标加方括号。

CONNECT_TO 只改变连接目标，Host、TLS SNI 和证书域名验证仍基于 URL。
它不修改共享 DNS 缓存，各 easy handle 可携带不同目标。
见 [CONNECT_TO 官方说明](https://curl.se/libcurl/c/CURLOPT_CONNECT_TO.html)。

当前锁定的 `curl 0.4.50` 已提供 `Easy2::connect_to(List)`，并持有列表生命周期。
本地 `curl-sys 0.4.90+curl-8.21.0` 所带 `curl/lib/url.c` 的
`url_match_destination` 拒绝不同 `via_peer` 的复用，
`url_match_connect_config` 还区分有无 connect-to。
这是选择该方案的源码依据；**同 IP 复用、跨 IP 隔离仍须用真实 socket 原型验收**，
升级依赖后保留回归测试。

启用策略的域名直连请求始终携带一个明确目标，包括只剩一个候选时。
不能选中 IP 后又把全部地址交给 libcurl 自行回退，否则归属可能失真。
该路径绕过源站 `ResolvePlan`，避免地址变化触发 `fresh_connect`、破坏复用；
关闭策略继续现有 RESOLVE 路径。

### 为什么不只修改 RESOLVE

RESOLVE 写入 host:port 的 DNS 缓存，后续同键条目会覆盖旧值，
不是每个请求的独立路由选择器。见 [官方说明](https://curl.se/libcurl/c/CURLOPT_RESOLVE.html)。

现有池实验及 `a_changed_resolve_answer_opens_a_new_connection` 已覆盖地址变化后的旧连接问题。
每次换 IP 强制 fresh-connect 无法自然回到该 IP 的缓存连接；
并发改写共享 DNS 条目还需额外隔离，因此不以“轮换 RESOLVE + 强制新建”实现收敛。

### 备选与淘汰边界

只有首选原型不能满足隔离或回收契约时，才评估 `PoolKey + selected_ip` 独立 Multi。
备选必须有 origin 级缓存配额分配、候选池数量和寿命限制，不能每个 IP 各缓存 k 条。

首选不承诺立即定向关闭某个 IP 的任意空闲 socket。排名下降或 DNS 移除时，
停止新分配，旧连接由缓存上限和空闲超时回收，在途请求完成。
`CURLMOPT_NETWORK_CHANGED / CURLMNWC_CLEAR_CONNS` 作用于整个 Multi，
不是“关闭一个差 IP”；进行中的传输可以继续。
见 [官方说明](https://curl.se/libcurl/c/CURLMOPT_NETWORK_CHANGED.html)。

不因排名小幅变化清空整个池，也不提前对所有请求设置 forbid-reuse。

## 4. 前置门槛：空闲回收必须重新验证

当前 `Pool::idle` 是时间戳堆，提交时 `claim_idle_connection` 领取最老记录，
完成后用 `NUM_CONNECTS` 估算复用或新建，没有 IP 或真实连接 ID。

定向后 A 请求不能领取 B 的空闲记录，否则会推迟 B 的回收期限。
即使按 IP 分组，`NUM_CONNECTS = 0` 也不能证明具体复用了哪条连接或它仍在缓存。
原来的估算不能直接作为多 IP 精确池状态。

原型需用真实连接身份关联空闲时间，或采用可证明不推迟超时的保守回收机制：

- 身份使用 `(driver_id, pool_generation, conn_id)`。`CURLINFO_CONN_ID`
  仅在同一连接缓存内唯一，无连接为 -1；Multi 重建必须换代。
  见 [官方说明](https://curl.se/libcurl/c/CURLINFO_CONN_ID.html)。
- 当前 wrapper 如无包装，在 driver 封装最小 getinfo FFI，使用 `curl_off_t` 并检查返回码。
  请求 ID、IP、fd 都不能替代物理连接 ID。
- conn ID 不等于完整缓存枚举。服务端关闭、libcurl 淘汰、透明重连、清池失效仍需处理；
  必要时结合 socket 生命周期事件验证。
- 测试“A 持续忙、B 空闲到期”和“同 IP 多条空闲连接只复用一条”，
  未使用的连接不得因为别的请求而延后回收。

保留 origin 的 `MAXCONNECTS = k`，不将它解释为活动连接限制。
k = 0 保持 fresh-connect + forbid-reuse。并发超过 k 可能降低复用率，沿用当前配置语义。
回收能力以实际 vendored 版本、运行时返回值和实验为准；
历史文档的版本号不作为最新 API 可用性判断。

## 5. 候选状态与请求流程

建议新增内部 `src/network/curl/ip_policy.rs`，由 driver 线程维护状态，
避免多个 Tokio 请求同时选中同一个“尚未覆盖”候选：

1. transport 判定本跳是否适用，在现有连接期限内取得 DNS 答案。
2. 随提交命令传入 origin、候选、绝对有效期、请求/任务标识及绝对 deadline。
   driver 再检查有效期，不能使用排队期间已经过期的快照开始新选路。
3. driver 原子选择 IP 并预留占用，构建 CONNECT_TO；提交失败、完成、取消各有一次性结算。
4. 记录实际 IP/连接身份；实际与期望不符时记录异常，不强行归给期望 IP。
5. 后续请求使用更新排名。内部地址回退用独立 attempt 标识，仍归属原请求和原任务预算。

每个候选记录 DNS 代次、最近使用、在途尝试数、连接失败冷却、有效窗口字节/时长/数量、
近期速率及结果分类。候选表和 origin 历史设容量、过期限制，不持久化；
活跃状态不能为满足容量直接删除。

TTL 更新保留仍存在 IP 的短期历史；移除的 IP 不再接新请求。
旧代次完成事件只结算原占用，不能重新激活已移除候选。
稳定候选不必仅因 TTL 刷新重建连接。
没有可靠网络变化通知时，以客户端重建和短期历史过期作为边界。

## 6. 预算、回退与兼容性

“任务最多 8 个连接”不能变成“每个 IP 各 8 个”。区分以下口径：

| 项目 | 约束 |
| --- | --- |
| 任务活动预算 | 初始探测 GET、普通/Range、恢复/hedge、重定向本跳及候选回退共用任务准入 |
| 地址尝试 | 首版一次连接一个字面量 IP，不额外并行竞速或预热 |
| origin 缓存 | 多 IP 共用同一 Multi 的 k；共享空闲连接不归属单一任务 |
| 物理连接 | 服务器计数/socket 事件验证峰值，含透明重连，不能只数 easy handle |
| 多任务 | 保留各自活动预算，不互相覆盖配置；可共享近期 IP 历史 |

沿用或补齐 session slot 机制，将预算上下文传到 worker/transport。
已持有 slot 的请求复用凭证，避免底层再次获取造成自锁；
恢复请求按现有规则取得额度，不能因为换 IP 绕过预算。
严格物理连接峰值未经原型验证不能宣称达标。

收到响应头不归还活动额度。丢弃 body 只是提交取消，
driver 完成 remove/结算后才归还传输额度。
libcurl 收完正文后连接可能已能复用，上层仍在消费缓冲；
应用 lease 可以更晚结束，不能将其误当物理连接仍在忙。

DNS、driver 排队、选路和串行回退共用本次调用的原有响应头 deadline。
连接阶段使用 `min(connect_timeout, head_budget)` 的剩余量。
可给单候选有界尝试时间，为其他候选留机会，但总期限不重置。
手动重定向沿用现有每跳超时语义，不擅自改成整个链共用一个期限。

首版只在**能确认尚未发送 HTTP 请求的建连失败**后内部串行回退，
次数受候选数、剩余时间及内部上限共同约束。
“响应头未发布”不能作为安全重试依据，请求可能已经发送；
其他错误交回现有应用重试规则，不能再新增一层下载重试循环。
全部候选失败返回有分类错误，不退回无预算的全地址竞争。

连接拒绝/不可达可短期冷却；429/503、Retry-After、TLS 验证失败、
正文错误、用户取消分别处理，不轮换 IP 绕过限流或忽略证书错误。
代理仍由代理解析源站，不设置源站 CONNECT_TO，不把代理 IP 用于源站评分。
重定向每跳按新 origin 重新选路，Host/TLS 基于原域名。
IPv6 开关继续由 Hickory 过滤候选，串行回退的双栈可用性必须测试。

客户端级开关进入 `ClientNetworkConfig` 缓存相等性/哈希及 Python 绑定。
关闭时保持现有行为；未来 HTTP/2/3 的流与连接关系另行设计。

## 7. 被动统计和收敛

### 两层终态与归属

共享上下文关联任务 ID、worker 请求 ID、driver transfer/attempt ID、origin、
候选代次、选中/实际 IP 和连接身份。
worker ID 通过请求扩展下传，transport 再将上下文关联到响应扩展和 body。

响应头回调发布时连接信息未必已被外层读取，上下文允许稍后补全。
在 driver 正常执行点读取 getinfo，remove handle 前保存最终信息；
不得从 Tokio 线程操作 easy handle，也不在回调中重入不允许的 libcurl 操作。

网络终态（收完/失败/取消）与消费终态（完整接受/提前丢弃/Range 或校验失败）
通过幂等事件合并。libcurl DONE 不等于文件写入或完整性校验成功。
取消、hedge 败者、正文未消费完不作完整成功样本；
任务级校验失败不能凭空归罪某个 IP。

### 采样窗口

用单调时钟记录正常正文窗口，包含等待远端数据的时间。
`BodySink::write` 只统计实际接受的字节，pause 后重投的数据不能重复计数。
补充 pause/resume 区间，由 session 标记限速、内存等待及写入背压；
只有 pause 次数不足以辨别污染。

首版丢弃明显受本地约束污染的窗口，不通过大量扣除时间制造虚高速度，
也不直接用 `CURLINFO_SPEED_DOWNLOAD_T` 或整次下载平均速率作为 IP 能力。
健康持续窗口可作暂定观测，完整成功统计等消费终态确认；
失败窗口与此前健康窗口分开，避免误记成功。

先按请求/连接计算有效速率，再平滑聚合到 IP，并记录并发占用。
不按 IP 总字节排名，否则更多连接会带来虚假优势。
建连及响应头耗时辅助判断，空连接没有吞吐成绩。

### 选择规则

1. 过滤 DNS 失效和连接失败冷却候选，无健康候选时做有界回退。
2. 正常请求先覆盖未尝试候选，再按在途占用分散，不额外发请求。
3. 达到最小有效时长、字节数和多个稳定样本后，以近期有效吞吐为主排序。
4. 后续主要选择前两个 IP；单候选自然退化，不为等待首选空闲饿死其他正常请求。
5. 用差距阈值、最小驻留时间、历史衰减抑制反复切换。
6. 样本过期、首选退化或低频探索条件满足时，让正常请求尝试其他候选。

前两个 IP、最小样本、探索频率先作为内部参数，由受控实验确定。
“8 个活动名额、3 个候选”应能初期覆盖三个 IP，随后主要使用稳定快的两个。
在途慢请求正常完成，持续卡顿仍交给现有恢复模块。

## 8. 实施步骤与验收

### M0：定向与生命周期原型

在 `src/network/curl/pool_semantics.rs` 和 driver 测试增加真实本地服务器实验：

- 同域名、同端口、两个 loopback IP：A → B → A，最后一次复用 A，
  不使用 fresh-connect；并发 A/B 不串目标。
- HTTPS 原域名 SNI/Host 正确，证书域名不匹配失败，IPv6 目标格式正确。
- 真实 IP/连接 ID 可关联；同连接上请求 ID 不同、conn ID 相同；
  Multi 重建后的同号不误关联。
- 多 IP 共用 k，k = 0 不缓存，活动并发不被 k 串行化。
- 多 IP/同 IP 空闲期限、服务端关闭、透明重连、取消、超时、
  body 丢弃和 DONE 后尚有缓冲数据的生命周期。
- 所有任务路径的活动预算和真实 socket 峰值；提交失败/取消确认不泄漏额度。

输出可复现结论，再确认首选架构。空闲记账或预算未过关，不进入策略启用阶段。

### M1：开关、候选分散和回退

实现默认关闭配置、TTL/代次、driver 原子选路、冷却、有界回退。
验证代理、重定向、单 IP、双栈、多任务、取消/暂停恢复。
所有内部回退延续原 deadline 和任务额度。

### M2：只观察，不排名

单连接、多连接和恢复统一反馈。验证 pause 重投不重复计字节、
限速/背压不产生虚假排名、网络与消费完成不重复结算。
算法测试用可控时钟，真实网络测试归属和生命周期。

### M3：启用收敛

加入排名、迟滞、衰减、低频探索。受控快/慢/失败路径验证初始分散和后续收敛；
中途交换快慢后能重新收敛且切换有界。
不新增在途抢占，不因排名变化清空池。

### M4：兼容性与公网复测

覆盖 DNS 更新、429/Retry-After、TLS/正文错误、校验、多任务、取消、代理、策略关闭。
Rust 实现变更执行项目相关测试、fmt/clippy 和 Linux 95% 覆盖率门禁。
本次只更新设计文档，未执行这些实现验收。

公网沿用指定 Citron ZIP：先固定 IP 分别测量，再交替比较开关，
固定连接数、文件和日志级别并重复运行，验证 SHA-256/ZIP CRC。
记录每 IP 后续请求分布、失败率、中位耗时、慢尾；
公网波动时如实报告不确定性，不把公网吞吐作为 CI 断言。

## 9. 可观测性与完成标准

TRACE 记录候选/TTL、选择理由、实际 IP/连接身份、回退、窗口污染、
冷却及预算事件，避免逐帧日志。
任务结束汇总每 IP 请求数、新建连接数、有效样本/速率、失败分类、占用峰值及切换次数。
driver 累计统计另行保留，不冒充单任务值；日志不输出代理凭据。

验收核心：没有独立测速流量；任务并发和 origin 缓存不随 IP 数膨胀；
归属与样本可信，本地限速/背压不误导排名；
受控环境能够收敛并适应变化；原有超时、重试、取消、代理、TLS 和校验不回归。
公网提速是实验结果，不预先承诺固定比例。
