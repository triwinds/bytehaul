# libcurl 传输后端迁移计划

日期：2026-09-12；最近更新：2026-09-13。状态：P0–P6 已完成。P4 的构建/分发门禁、P5 的默认
切换和 P6 的旧实现清理均已接入仓库；生产传输现在只有 libcurl。

## 1. 目标与结论

本计划将原 Hyper HTTP/TLS 传输栈替换为 `curl` crate 驱动的 libcurl，采用专用线程运行
`Multi + Easy2`，向 Tokio 下载会话提供异步响应头和有界流式 body。保留 bytehaul 的
分片调度、动态拆分、连续 Range 请求、前缀保留、断点续传、重试预算、限速和存储提交逻辑。

这不是仅替换 TCP connector：libcurl 同时管理 HTTP、TLS 和连接复用，需要把当前暴露给
会话层的 Hyper 响应体一并抽离。迁移期间曾保留 Hyper 作为同条件对照和构建时回退；P6
完成后已移除这条旧生产路径，生产默认与显式 curl-only 构建均使用 libcurl。

依据是 [libcurl 公网对照实验](libcurl-comparison.zh-CN.md)。其中单连接中位速度更高，
但计时、日志和 IP 条件未完全对齐，8 路实验也有重置与超时。它支持投入迁移验证，
不支持承诺速度提升倍数，也不能把“8 个线程各跑一个 Easy”的实验直接搬进生产。

首版继续使用 HTTP/1.1。HTTP/2、HTTP/3、多 IP 竞速和分片策略调优分别评估，避免同时改变
协议、连接数含义和调度行为。多 IP 相关文档属于后续设计参考，不作为本次切换的前置实现。

## 2. 当前代码边界

| 位置 | 当前职责与耦合 | 计划改动 |
| --- | --- | --- |
| `src/network.rs` | 共享的 `ClientNetworkConfig`、代理解析以及唯一的 libcurl client 入口 | 保留配置入口；resolver 与 libcurl 实现已拆出（P1/P2/P3/P6 已完成） |
| `src/network/dns.rs` | Hickory 解析器：多 DNS、多 DoH、IPv6 开关、TTL 缓存；`resolve()` 返回地址 + TTL 截止时间 | 稳定接口；多 IP 调度另立设计 |
| `src/network/curl/` | `driver/`：专用线程 + 按 origin/代理分池的多 `Multi`、头部状态机、有界流、终态与 drop 取消、RESOLVE 记账、错误表与 `DriverStats`；`transport.rs`：中立请求 → 驱动选项、DNS 注入、代理路由、响应头 → 中立响应 | P4/P5/P6 已完成；继续维护 libcurl 边界 |
| `src/http/mod.rs` | 中立 `HttpBody`（P1）、自有 `HttpRequestBody`、逐帧读取、read timeout、`BodyBudget` 请求扩展（P3） | 保留调用语义；P4 观察背压参数 |
| `src/http/request.rs`、`response.rs` | GET/Range 构造、HTTP 元数据解析 | 已完成（直接依赖 `http` crate） |
| `src/http/worker.rs` | 重定向、最终 URL 缓存及失效刷新、状态码、响应头超时、请求诊断、按会话预算下发每传输 body 预算（P3） | 保留应用层行为 |
| `src/session/single.rs`、`multi.rs` | 消费响应体、限速、写盘、恢复；`MemoryBudget` 派生传输队列预算（P3） | 调度与提交职责不动 |
| `src/session/multi/adaptive.rs` | `RequestStream` 保存中立 `HttpBody` | 已完成 |
| `src/manager.rs` | 按 `ClientNetworkConfig` 缓存与共享唯一的 libcurl client | 缓存可克隆的 driver 入口；`CurlTransport` 释放时记录 `DriverStats` |
| `src/error.rs` | 公开 `TransportErrorKind` 与重试判断 | curl 错误表见 `driver::classify_curl_failure`（确定性失败不可重试） |
| `bindings/python/`、`.github/workflows/` | Python API、wheel 构建与发布 | P4 补 native 依赖和分发验证 |

`scheduler`、`storage`、`rate_limiter` 不因换库重写。传输回调收到字节不代表分片已提交；
进度、piece map 和控制文件仍以现有会话/存储规则为准。

当前实际边界：响应类型是 `http::Response<HttpBody>`，`HttpBody`
（`src/http/body.rs`）包装驱动 `driver::BodyStream`（迁移期间它是 `Curl` 单变体枚举，
后续在[简化与优化计划](simplification-and-optimization-plan.zh-CN.md)的 P6 中收敛为
具体类型）；libcurl 响应头和 body 在
`src/network/curl/transport.rs` 处转换后，不再向 worker、session 或 adaptive 暴露具体驱动
类型。请求体是自有 ZST `HttpRequestBody`。解析器位于 `src/network/dns.rs`，通过
`CURLOPT_RESOLVE` 注入 libcurl；`enable_ipv6`、自定义 DNS、DoH 与 TTL 缓存在同一条路径生效。
worker、session、scheduler、storage 与 rate_limiter 的调用形态没有变化。

## 3. 推荐实现

### 3.1 中立 HTTP 接口

优先沿用 `http::Request`、`http::Response`、`HeaderMap`、`StatusCode` 和 extensions，
只引入内部 `HttpBody`，避免额外创造整套 HTTP 对象。GET 请求体使用自有的
`HttpRequestBody`。`HttpBody` 提供异步分块读取和 drop 取消行为。

接口必须满足：

- 当前这一跳的最终响应头完整后即可返回 response，不等待 body 下载完毕。
- body 依序返回 `Bytes`；结束必须区分成功 EOF 与传输错误。
- 响应头之前失败交给 request future；之后失败交给 body，原始原因不能退化成普通 EOF。
- 已排队的有效字节先交付，随后返回终态错误，供现有前缀恢复逻辑处理。
- request future 被丢弃或超时、response/body 被丢弃，都能取消对应传输。
- 保留 `RequestDiagnostics` extensions；请求调用次数与真实建连次数分开统计。

实现使用内部 `HttpBody` 包装驱动 body，无需引入公开后端 trait 或让用户接触 libcurl
handle。P6 完成后 `HttpBody` 只保留 libcurl 变体，`next_chunk`/`next_data_chunk` 的签名
与调用点不变；也不再提供后端选择字段或测试用的 `with_backend` 入口。后续简化计划的 P6
把单变体枚举与 `BytehaulClient` 一并收敛为包装具体传输的类型，这一 seam 与签名保持不变。

### 3.2 Multi driver 与线程归属

`curl::multi::Multi` 不实现 `Send/Sync`，在拥有它的专用线程内创建、驱动和销毁。
Tokio 侧只持有命令入口与响应通道；所有 `add2/remove2`、恢复接收和 handle 操作都在
driver 线程执行。[Multi 文档](https://docs.rs/curl/latest/curl/multi/struct.Multi.html)

建议先让每个缓存的网络 client 持有一个 driver，再按连接池兼容方案在该线程内管理
一个或多个 Multi。client clone 共享 driver；不为每个分片创建线程。大量不同配置的
client 需要设置资源预算，并验证缓存释放；若线程数成为瓶颈，再改成固定 driver 分片。

driver 循环处理提交/取消/恢复命令、执行 `perform`、消费完成消息，并按 curl timer
及有上限的等待时间调用 `wait`。首版采用有界等待，例如最大 20 ms，记录命令处理延迟；
无 active transfer 时阻塞等待命令，不能忙轮询。高并发优化再评估 socket/timer API
与可唤醒 poll，不直接假设 Rust binding 暴露所有 C API。

每个传输使用独立 ID 和终态记录。取消控制使用不会被 body 队列堵住的通路；重复取消、
完成与取消竞争、恢复过期 ID 均须幂等。driver 意外退出时，让所有等待者得到明确错误。
最后一个 client/传输引用释放后关闭 driver；避免循环引用，Tokio 工作线程不做阻塞 join。

### 3.3 回调、背压和内存

`Easy2` 的 header callback 解析响应头，write callback 将字节送入有界队列。
回调不能等待 Tokio、写磁盘或等待限速令牌；这些操作继续由会话执行。

按字节而非仅按消息数控制队列预算，计入每个任务及全局内存预算。预算耗尽时，write callback
返回 pause，不能部分入队后再 pause。curl 会在恢复时重新交付这批数据，否则会产生重复字节。
消费者释放空间后通知 driver 恢复；恢复操作可能同步重入 write callback，不能持有队列锁。
[write callback](https://curl.se/libcurl/c/CURLOPT_WRITEFUNCTION.html)、
[pause 语义](https://curl.se/libcurl/c/curl_easy_pause.html)

队列上限之外还要测量 libcurl、TLS、socket 缓冲和暂停缓存的额外占用，不能把通道大小当成
总 RSS 上限。首版不启用 HTTP/2 多路复用和自动内容解压，减少隐藏缓存及 Range 偏移变化。
至少容纳一次完整 body callback，避免单块大于预算导致永远无法恢复。

用户暂停下载仍走现有保存/退出/续传流程；curl pause 仅是瞬时背压机制。限速等待和写盘
停顿不计为网络低速；终态通知不依赖已满的 body 队列，避免取消或失败时死锁。

## 4. 行为兼容清单

### 4.1 HTTP 与数据正确性

- 明确只允许 HTTP/HTTPS，固定 HTTP/1.1，关闭自动 follow-location 和自动内容解压。
- 重定向继续由 `HttpWorker` 逐跳处理，保留跳数限制、相对 URL、缓存目标失效刷新和逐跳计时。
  回归跨源 header 行为，确保代理凭据不进入源站请求。
- header callback 需要区分 1xx、代理 CONNECT 响应、源站最终响应及 trailers；只发布一次
  源站最终响应头。状态行后重建 header block，保留重复字段，限制头部总大小。
  不能把 CONNECT 的 200 当作文件的 200。[header callback](https://curl.se/libcurl/c/CURLOPT_HEADERFUNCTION.html)
- 不启用 `fail_on_error`，保留原始 HTTP 状态及 `Retry-After`，由当前 worker/session 分类。
- Range 保持精确字节范围和 identity 编码；206 的起止/总长、200 忽略 Range、416、
  ETag/Last-Modified 身份变化、未知长度、提前 EOF、超长响应继续走现有校验。
- libcurl 处理 HTTP chunked framing，但不改变实体内容编码。回归自定义 headers、Host、
  User-Agent、Authorization、Content-Disposition 以及 URL 转义，防止隐式默认值改变请求。

### 4.2 超时、重试与取消

| 当前语义 | curl 后端处理 |
| --- | --- |
| `connect_timeout` | 配置 curl 建连期限；curl 建连包含 DNS 与握手，需要记录与旧 connector 的差异 |
| `request_headers_timeout`，未设置时回退 `read_timeout` | 保留 Tokio 从发送本跳到收到最终头的 deadline，覆盖排队和外部解析；超时必须移除 handle |
| `read_timeout` | 保留按会话等待下一 body chunk 计时；本地限速/存储等待期间不运行这项等待 |
| 慢速恢复与重试预算 | 继续由 session 管理，不叠加 libcurl low-speed abort 或整请求总 timeout |
| pause/cancel/drop | 发送取消命令并移除 handle，清理积压数据，释放 lease 的顺序遵守现有会话契约 |

curl 的 connect timeout 覆盖 DNS 和协议握手；外部 Hickory 查询不在该选项控制范围内，
需要共享连接阶段 deadline，避免外部 DNS 和 curl 各消耗一整份预算。
[连接超时语义](https://curl.se/libcurl/c/CURLOPT_CONNECTTIMEOUT.html)

错误映射按失败阶段和实际原因制定表格：超时归 `Timeout`，可恢复建连失败归 `Connect`，
响应体重置/截断归 `Body`。保留 curl code、错误链和阶段用于诊断。
取消不能被翻译成可重试 write error；参数错误、证书不可信、主机名不匹配等确定性失败
须映射到不可重试路径。当前 `Connect/Request/Body` 都可重试，不能把所有 curl 错误塞入其中。

### 4.3 DNS、代理、TLS

当前实现保留 Hickory，并使用不依赖 Hyper `Service<DnsName>` 的解析接口，继续支持多 DNS、
多 DoH、IPv6 开关和 TTL 缓存。Tokio 异步解析后通过 `CURLOPT_RESOLVE` 注入这一跳的候选地址，
URL 仍保留域名以维持 Host、SNI 和证书验证。IP 字面量直接处理，IPv6 格式单独测试。
[RESOLVE 文档](https://curl.se/libcurl/c/CURLOPT_RESOLVE.html)

Multi 内部 DNS 缓存可能共享，必须处理地址覆盖、TTL 到期和旧条目移除；不能注入永久记录后
让它绕过 Hickory TTL。已有可复用连接也不保证按新的 RESOLVE 建连。固定 IP 实验应隔离
client/pool；后续多 IP 调度需要额外设计，不能仅调用 RESOLVE 就宣称支持地址调度。

代理继续由现有显式配置与环境变量规则计算，按 HTTP/HTTPS 选择代理并应用到每次请求。
明确设置 curl 的代理与 bypass 选项，避免它再次读取环境导致规则变化，尤其是 `HTTP_PROXY`
大小写和 `NO_PROXY`。先锁定现有行为，再单独提出兼容改进；禁用代理的实验配置不能成为生产默认。
代理模式区分“代理端点解析”和“由代理解析目标域名”，不强制所有源站在本地解析成功。
测试 HTTP 代理、HTTPS 代理、CONNECT、代理认证及重定向后的路由选择。

Windows 优先验证 vendored libcurl + Schannel，保持系统证书与主机名校验。Linux/macOS 的
TLS 后端、系统信任库和打包方式在 P0 确定；`static-curl` 不代表 TLS 等传递依赖全部静态。
保留 Hickory 意味着 DoH 仍可能使用 Rustls，此阶段不以彻底删除 Rustls 为目标。

### 4.4 连接池是切换门槛

现有公开配置包含每主机空闲连接上限和空闲超时；manager 还按网络配置复用 client。
curl 的 Multi 连接缓存总量、每主机活动连接上限及连接空闲可复用时间不是相同概念。
不能把 `pool_max_idle_per_host` 直接传给 `set_max_host_connections`，否则会改变并发数。
`MAXAGE_CONN` 是复用时检查年龄，不保证到时主动关闭空闲 socket。
[MAXAGE_CONN 文档](https://curl.se/libcurl/c/CURLOPT_MAXAGE_CONN.html)

P0 必须产出一份可运行的池语义验证，选定以下实现方向：

1. 优先保持 API 契约：在同一 driver 中按 origin、代理路由及网络配置隔离 pool，
   验证 Multi 缓存限制对 active/idle 的实际影响，并实现空闲回收。仅完全空闲的池可直接
   销毁；有活动传输时必须验证安全淘汰方式，不能靠重建池中断其他任务。
2. 若 curl 公共接口无法合理实现精确契约，明确设计新的池配置与旧配置弃用/版本迁移方案，
   同步 Rust/Python 文档和测试，再切换默认后端。不得静默近似映射旧参数。

禁用池时明确禁止连接复用，测试顺序请求确实建立新连接。启用池时验证连接复用、远端关闭、
失效连接恢复、跨任务共享和配置隔离；连接上限不能替代调度器的 worker/lease 限制。

## 5. 分阶段实施与验收

每阶段单独提交，前一阶段验收通过后再推进。阶段名称是工作分解，不是已完成状态。

| 阶段 | 工作与交付 | 验收条件 |
| --- | --- | --- |
| **P0 可行性与构建（已完成）** | 在仓库内保留可复现的小型 Multi 原型；验证线程、pause、取消、池语义、DNS/代理；确定依赖及各平台 TLS | Windows/Linux/macOS 目标能构建；取消无泄漏；池 API 方案明确；运行时记录真实 libcurl/TLS/resolver 特性 |
| P1 接口抽离（已完成） | 增加中立 HttpBody 和 `http` 直接依赖，仅由 Hyper 实现；适配 worker、session、adaptive | 原有功能测试通过，行为与旧基线一致；生产 session 不再引用 Incoming |
| P2 curl 后端（已完成） | 增加 driver、Easy2 handler、头部状态机、有界流、终态和 drop 取消 | 本地 HTTP/HTTPS 单连接和多 Range 正确；首头即时返回；暂停重放无重复；完成/取消竞争可控 |
| P3 兼容补齐（已完成） | 接入 DNS/DoH、代理、TLS、池配置、错误映射、超时与 diagnostics | 第 4 节契约测试通过；原有暂停/续传、重试、低速、连续请求和前缀测试在 curl 下通过 |
| **P4 打包与对照（构建门禁已接入）** | Rust/Python 构建矩阵、本地资源压力测试、公网随机交错比较 | wheel 干净环境可安装；输出哈希一致；资源有界；性能及失败率达到预先定义的门槛 |
| **P5 默认切换（已完成）** | curl 设为默认，更新架构/配置/发布说明；完成发布观察并为 P6 清理旧回退做准备 | 默认构建和 curl-only 构建通过；错误、资源与性能无未解释回归 |
| **P6 收尾（已完成）** | 移除 Hyper 生产 adapter、旧 feature、仅供旧栈的直接依赖及旧测试原型 | libcurl 独立构建通过；Rust/Python 文档、CI、发布脚本和示例无旧后端假设 |

### P0 实施记录（2026-09-16）

交付物：

- `Cargo.toml`：新增临时特性 `hyper-backend`（默认）与 `curl-backend`，把
  `hyper`/`hyper-rustls`/`hyper-util`/`http-body-util`/`hyper-http-proxy`/`tower-service`
  改为可选依赖，新增 `curl = { version = "0.4.50", features = ["static-curl"] }`。
  P1/P2 未完成前 `curl-backend` 暂时隐含 `hyper-backend`（中立 HTTP 类型仍由 Hyper
  adapter 提供），P2 完成后解除。
- `src/network/curl/runtime.rs`：运行时 `curl_version_info` 报告（版本、TLS、zlib、
  HTTP/2/3、`https_proxy`、IPv6、异步 DNS），并在 `ClientNetworkConfig::build_client`
  以 debug 级别记录；本机实测
  `libcurl 8.21.0-DEV (vendored=true) host=unknown tls=Schannel zlib=1.3.2 …`。
- `src/network/curl/spike.rs`：专用线程 `Multi`+`Easy2` 原型，包含头部即时发布、
  有界字节队列与 pause/unpause、终态（EOF/失败/取消）区分、drop/list 取消、
  task/连接失败上报、DNS(`RESOLVE`)/代理选项应用。
- `src/network/curl/pool_semantics.rs` 与 `docs/libcurl-pool-semantics.zh-CN.md`：
  连接池语义实测与实现方案（分池键、`pool_idle_timeout` 主动回收、
  `pool_max_idle_per_host` 映射结论）。

验证命令与结果（Windows，本机沙箱）：

```text
cargo build --offline --features curl-backend --lib                       # 通过（vendored libcurl 可离线构建）
cargo test  --offline --features curl-backend --lib curl::                # 36 passed; 0 failed
cargo clippy --offline --features curl-backend --all-targets              # 无 warning
cargo test  --offline --lib                                               # 400 passed; 11 failed（与 HEAD 基线完全相同的环境失败集合）
```

基线对照：对 `git archive HEAD` 的全新副本执行 `cargo test --lib`，结果同样是
400 passed、11 failed，且失败测试名完全一致（Windows 证书存储访问被拒绝导致的
proxy/网络用例），说明 P0 改动没有引入回归。取消无泄漏由
`cancelling_while_streaming_leaves_no_active_transfer`、
`dropping_an_unread_body_cancels_the_transfer` 与
`driver_failure_fails_waiters_instead_of_truncating` 覆盖（取消后
`active_transfers() == 0`、`completed() == 0`）。

尚未覆盖：Linux/macOS 的真实构建（本机只有 Windows 工具链，需要 CI 矩阵验证）；
P0 未接入生产传输路径，生产行为仍与旧 Hyper 实现一致。

### P1 实施记录（2026-09-12）

日期按本机时钟记录；P0 段落里的 “2026-09-16” 标签比本机时钟更晚，未改动该段落。

交付物：

- `src/http/body.rs`：内部 enum `HttpBody`。P1 只有 Hyper 变体，逐帧读取逻辑从
  `http/mod.rs` 原样搬入，保留原有语义（跳过非数据帧、EOF 与传输错误区分、按调用方
  deadline 计时、drop 取消）；`Debug` 只报告后端名，不暴露 body 内部状态。P2 在同处
  增加 curl 变体，`next_chunk` 的签名和调用点不变。
- `src/http/mod.rs`：`HttpResponse` 改为 `http::Response<HttpBody>`；`next_data_chunk`
  保持原调用形态，转调 `HttpBody::next_chunk`。请求体 `HttpRequestBody` 仍是
  `Empty<Bytes>`，属 P2 迁移面。
- `src/http/request.rs`、`response.rs`、`worker.rs`：通用 HTTP 类型改由 `http` crate
  直接提供（`Request`/`Response`/`HeaderMap`/`StatusCode`/`Version`/`header`）。
- `src/network.rs`：新增 `neutral_response`，是唯一把 `hyper::body::Incoming` 转成
  `HttpBody` 的位置；`BytehaulClient` 的请求签名改用 `http::Request`。
- `src/session/multi/adaptive.rs`：`RequestStream.body` 由 `Incoming` 换成
  `crate::http::HttpBody`，`CONTENT_LENGTH`/`TRANSFER_ENCODING` 读取改用 `http::header`。
- `Cargo.toml`：`http = "1"` 从 dev-dependencies 提升为直接依赖。
- 测试专用代码（`src/network/tls_tests.rs`、`src/network/multi_ip_prototype.rs`）同步
  改用中立 body；`HttpBody::collect_to_bytes`（仅测试）用于汇总 body。

验证命令与结果（Windows，本机沙箱）：

```text
cargo test  --offline --lib                                    # 403 passed; 11 failed
cargo test  --offline --no-fail-fast --tests                   # 集成测试 88 passed; 0 failed
                                                               #（lib target 仍为同一组 11 个环境失败）
cargo test  --offline --features curl-backend --lib            # 439 passed; 11 failed
cargo clippy --offline --all-targets                           # 无 warning
cargo clippy --offline --features curl-backend --all-targets   # 无 warning
cargo fmt --all -- --check                                     # 通过
cargo check --offline --no-default-features --features hyper-backend --lib   # 通过
```

基线对照：改动前后 lib 测试的失败集合完全一致（11 个 Windows 证书存储
`PermissionDenied` 引起的 proxy/网络用例），通过数从 400 增至 403。新增的三条是
`src/http/body.rs` 的中立 body 契约测试：chunked trailer 之后干净 EOF、body 停滞映射为
`Timeout`、截断 body 在已交付字节之后仍报 `Body` 错误。集成测试
`tests/m1_basic.rs` 至 `tests/m12_slow_transfer.rs` 与 `tests/http_header_timeout.rs`
共 88 条全部通过（default 与 `--features curl-backend` 分别运行）。`cargo fmt --check`
在本次一并修正了 P0 留下的 `src/network/curl/spike/tests.rs` 重复导入与缩进。

验收对应：生产 session、worker、adaptive 已不引用 `Incoming`（该类型只剩
`src/http/body.rs` 的 adapter 与 `src/network.rs::neutral_response`）；原有功能测试通过、
行为与旧基线一致；响应头先于 body 返回、按会话 deadline 的分块读取和
`next_data_chunk` 调用语义未变。

尚未覆盖：curl 变体未接入（P2）；`curl-backend` 仍隐含 `hyper-backend`；本次只验证
Windows 目标；中立 body 的背压/pause 契约要在 P2 的 curl 变体上重新验证，Hyper 版本
只是把现有行为原样搬运。

### P2 实施记录（2026-09-12）

交付物：

- `src/network/curl/driver/`（P0 的 `spike` 提升为生产 driver，测试一并迁移）：
  - 头部状态机 `HeadParser`：按块解析，跳过 1xx 与代理 `CONNECT` 响应，只发布一次源站
    最终头（trailers 因已发布而被忽略）；状态行无法解析时显式失败，而不是静默无头。
  - 有界流 `BodySink`：按字节预算；队列非空且放不下时返回 `WriteError::Pause` 且不入队，
    消费者取走数据后经命令通道请求 `unpause_write`（恢复可能同步重入回调，故不持锁）。
  - 终态 `Terminal::{Eof, Cancelled, Failed}`；取消路径丢弃已缓冲字节并移除 handle。
  - drop 取消：`BodyStream::drop`（未读到 EOF）与 `PendingHead::drop`（头部尚未到达）
    都会取消；`BodyStream` 持有命令通道的强引用，**活着的 body 会让 driver 继续运行**，
    因此丢弃 client 不会掐断在途传输。
  - `RequestOptions.ca_info` → `CURLOPT_CAINFO`（只增加信任锚，不关闭校验）；
    错误分类把确定性 TLS 失败（51/58/59/60/64/66/80/82/83/90/91）映射为不可重试。
- `src/network/curl/transport.rs`：`CurlTransport` 把中立 `http::Request` 映射成
  `RequestOptions`（`Range: bytes=a-b` → `CURLOPT_RANGE` 的 `a-b`，非 bytes 单位原样转发；
  不注入默认 User-Agent；代理按 scheme 选择，未配置代理时 `noproxy("*")`），再把
  `ResponseHead` + body 包装回 `http::Response<HttpBody>`。
- `src/http/body.rs`：新增 `HttpBody::Curl(driver::BodyStream)`；`src/http/mod.rs`：
  `HttpRequestBody` 改为自有 ZST（Hyper 下实现 `http_body::Body`，无需新增依赖）。
- `src/network.rs`：`BytehaulClient::Curl`、`TransportBackend` 与 `with_backend`（仅测试）；
  Hyper 代码全部 cfg 到 `hyper-backend`；`Cargo.toml` 中 `curl-backend` 不再隐含
  `hyper-backend`。
- 测试：`curl::` 单测 51 条 + 3 条 HTTPS（`#[ignore]`）——其中 driver 30 条（头部状态机、
  body 通道原子观测与并发读、drop 取消、暂停无重复、慢消费者隔离、连接复用开关、代理绝对形式、
  `RESOLVE`、本地 HTTPS）、`transport.rs` 10 条（含期限契约）、`pool_semantics` 8 条；另有
  `http::worker::tests::both_backends_publish_headers_first_and_deliver_the_same_range`
  用同一个 worker 在两个后端上比对首头时机与字节一致性。

验证命令与结果（Windows，本机沙箱，全部 `--offline`）：

```text
cargo test  --offline --lib                                                # 403 passed; 11 failed（Hyper 默认，与 P1 基线同名同数）
cargo test  --offline --features curl-backend --lib                        # 455 passed; 11 failed; 3 ignored
cargo test  --offline --no-default-features --features curl-backend --lib  # 433 passed; 0 failed; 3 ignored
cargo test  --offline --no-fail-fast --tests                               # Hyper 默认：集成 88 passed; 0 failed
cargo test  --offline --features curl-backend --no-fail-fast --tests       # 历史双后端（当时默认 Hyper）：集成 88 passed; 0 failed
cargo test  --offline --no-default-features --features curl-backend --no-fail-fast --tests
                                                                          # curl-only：lib 433 passed；集成 87 passed; 1 failed
cargo clippy --offline --all-targets（default / --features curl-backend / curl-only）  # 三组均无 warning
cargo fmt --all -- --check                                                # 通过
```

关键结论：**libcurl-only 构建下整套单元测试（433 条，含 session/single、multi/adaptive、
prefix/transfer）与集成测试（m1–m12 与 http_header_timeout 共 88 条中的 87 条）直接通过**，
没有改动调度、存储、限速或会话逻辑——P1 的中立接口把后端替换限制在适配层内。

P2 过程中发现并修复的真实缺陷（前两条为自测发现，后两条来自实现走查）：

1. `CURLOPT_RANGE` 只接受 `start-end`。最初把请求头 `Range: bytes=4-7` 原样传入，实际发出
   `Range: bytes=bytes=4-7`，测试夹具因此不再匹配并连带出现 curl 52；现由 `byte_range_spec`
   剥掉 `bytes=` 单位，非 byte 单位则原样作为请求头发送。
2. driver 原先只由 client 句柄维持存活：先丢弃 client、再读 body 的调用方会让 driver 退出
   并把在途传输标记为 `Cancelled`。现由 body 流与待发布头部各自持有命令通道强引用，
   驱动存活期 = client 引用 ∪ 在途传输引用。
3. body 读取存在竞争，可能吞掉尾部字节：`BodySink::next_chunk` 原先分两次加锁——先看队列、
   再看终态。若第一次看到空队列之后、第二次看终态之前，driver 恰好入队最后一块并置终态，
   消费者会直接返回 EOF（失败终态则直接返回错误），既丢字节也破坏「先交付有效前缀、再报错」
   的契约。现改为单次加锁的 `poll()`：**同一把锁内**先取数据，取不到才看终态；旧的两步语义
   只保留为 `#[cfg(test)]` 辅助方法，生产路径不再可用。
   新增测试：`body_sink_terminal_state_is_reported_after_queued_bytes`（数据+EOF 时先给数据）、
   `body_sink_reports_a_failure_only_after_the_queued_prefix`（数据+失败时先给前缀）、
   `a_two_step_read_would_lose_the_tail_that_poll_delivers`（把该交错逐步写出来：旧两步读法在
   此处返回 EOF 丢掉 `tail`，原子观测返回字节后才是 EOF）、以及并发读写下不丢字节的
   `concurrent_production_never_loses_the_tail_before_eof`。该窗口只有几条指令宽，靠两个任务
   抢跑无法稳定复现，因此交错测试把步骤显式写出，而不是依赖调度概率。
4. 固定的 30 秒响应头超时会覆盖用户配置：`transport.rs` 每次请求都传 `DEFAULT_HEAD_TIMEOUT`，
   而调用方期限只在外层包装，于是 `request_headers_timeout = 120s` 仍在第 30 秒失败。现在
   **调用方期限是唯一期限**：`request_with_timeout` 把该值直接交给 driver（超时即移除 handle），
   内部常量退化为 `HEAD_DEADLINE_BACKSTOP`（300 s），只在没有调用方期限的入口
   （`BytehaulClient::request`，仅 Hyper 分支与测试使用）生效，且不得缩短调用方期限；
   同时 driver 的超时消息统一为 `request timed out`，与 Hyper 路径和
   `tests/http_header_timeout.rs` 的断言一致。回归测试：
   `the_caller_deadline_bounds_the_wait_for_response_headers`（150 ms 期限对不响应的服务器必须
   在 2 s 内以 Timeout 结束；用旧的固定超时会让该测试超时失败——已验证红/绿）与
   `a_long_caller_deadline_is_not_clamped_by_an_internal_default`（120 s 期限原样下发）。

环境限制（与 P2 代码无关）：

- 本沙箱拒绝访问 Windows 凭据/证书存储，Schannel 无法获取客户端凭据：系统
  `curl.exe -sS https://example.com` 直接失败并报
  `(35) schannel: AcquireCredentialsHandle failed: SEC_E_NO_CREDENTIALS`。因此**本地 HTTPS
  无法在此环境验证**：`driver/tests.rs` 的三条 HTTPS 测试（可信连接复用、无信任 CA 必须
  失败、主机名不匹配必须失败）标记 `#[ignore]`，需在可做 TLS 的 CI 上以
  `cargo test --features curl-backend -- --ignored` 运行；集成测试
  `tests/http_header_timeout.rs::header_deadline_includes_tls_handshake`（本意是让 TLS 握手
  挂起后由 header deadline 触发超时）在本沙箱下因握手立即失败而报 Connect，同属该环境原因，
  在具备 TLS 的环境中应通过（deadline 覆盖 connect+握手+响应头）。
- 另修正一条时序过强的回归测试：
  `session::single::tests::test_stream_single_autosaves_existing_progress_before_body_arrives`
  原先断言「结束后控制文件等于 body 前的 autosave 值」，但 curl 把 body 与 EOF 分两步交付，
  其间可能再触发一次 autosave（合法的完整前缀 5）。现改为在 body 期间观测控制文件，断言
  **首个**快照是 body 前的值，并允许最终值为 1 或 5；P4 应把「curl 下 autosave/flush 频率」
  纳入性能对比项。

P2 尚未覆盖、由 P3 补齐的内容见下一节：DNS/DoH 与 `CURLOPT_RESOLVE` 注入、代理 bypass
（`NO_PROXY`）与「由代理解析目标域名」、池配置（`CURLMOPT_MAXCONNECTS`、空闲回收、分池键）、
超时阶段划分与 diagnostics 计数、以及 `HttpBody` 背压与 session `MemoryBudget` 的对接。
用户可配置的 TLS 选项与证书错误细分仍未开始（P3 只补了错误分组与诊断阶段，见下表）。

### P3 实施记录（2026-09-12）

交付物：

- `src/network/dns.rs`（新增）：Hickory 解析器从 `network.rs` 抽出并与后端解耦，两个后端共用。
  `resolve()` 返回 `DnsAnswer { addresses, valid_until }`（`valid_until` 来自 Hickory 的 TTL
  缓存），`lookup_host()` 只是 Hyper `Service<DnsName>` 的适配（cfg `hyper-backend`）。
  DoH URL 解析、`build_name_server_group` 与脚本化 DNS 测试服务器一并迁入，其单元测试随之
  从 `network.rs` 移到该模块，因此 **curl-only 构建也能运行这些测试**。
- `src/network.rs`：`build_dns_resolver` 不再 cfg 到 hyper；`BytehaulDnsResolver` 由
  `network::dns` 重导出。
- `src/network/curl/transport.rs`：请求前解析本跳地址并把答案交给 driver；把调用方的响应头
  deadline 在「共享解析」与「libcurl 传输」之间共享；代理按 scheme 选择并以显式
  `CURLOPT_PROXY` + bypass 下发；读取 `http::BodyBudget` 请求扩展并钳制到传输可实现的范围；
  最后一个 client 引用释放时用 `DriverStats` 记录一次 debug 汇总。
- `src/network/curl/driver/mod.rs`：
  - **多池 driver**：每个 `PoolKey`（origin + 代理路由）一个 `Multi`，池内维护活跃传输、
    每条空闲连接的起始时刻与 RESOLVE 记账；`CURLMOPT_MAXCONNECTS = pool_max_idle_per_host`
    （缓存上界，见下节复审修正 2），池内没有传输且空闲超过 `pool_idle_timeout` 即销毁该池
    （socket 随之关闭）；池仍忙于长传输时用 `CURLMOPT_NETWORK_CHANGED` 关闭其空闲连接，
    到期判定按连接而非按池（复审修正 4、5）。
  - **命令通道**改为 `Mutex<VecDeque<Command>> + Condvar`（`CommandQueue`）：driver 需要
    「带截止时间的阻塞等待」以便在无传输时既回收空闲池又不忙轮询；命令排队延迟记入统计。
  - **RESOLVE 记账**：按 `host:port` 保存已注入条目与 TTL；TTL 到期或地址变化时先发
    `-host:port` 再发新条目，地址变化额外对该传输设置 `CURLOPT_FRESH_CONNECT`。
  - **错误表**（`classify_curl_failure`）与 `TransferPhase`（响应头之前/之后）写入错误消息；
    `DriverStats` 统计提交/完成/取消/真实建连数（`CURLINFO_NUM_CONNECTS`）/命令数/暂停恢复数
    与最大命令延迟。
  - **头部块上限** `MAX_HEADER_BLOCK_BYTES = 128 KiB`（§4.1 要求限制重建后的头块大小）。
- `src/network/curl/test_support.rs`（新增）：脚本化 HTTP 服务器、要求代理认证的代理、环境变量
  锁，供 transport 级测试复用。
- `src/http/mod.rs`、`src/http/worker.rs`、`src/session/flow.rs`、`src/session/mod.rs`、
  `src/session/multi.rs`：新增 `http::BodyBudget` 请求扩展；`HttpWorker::with_body_budget` 把它
  写进每个请求；会话按 `MemoryBudget` 与并发传输数推导每传输预算
  （`clamp(memory_budget / 并发数, 64 KiB, 256 KiB)`）。worker/session 的调度、存储与限速职责不变。
- `src/network/curl/pool_semantics.rs`：新增「有活动传输时调整 `CURLMOPT_MAXCONNECTS`」实验。
- 文档：本文件与 `docs/libcurl-pool-semantics.zh-CN.md` §5。

按 §4 契约实现的要点：

1. **DNS/DoH（§4.3）**：直连请求用共享解析器解析 origin，得到 `host:port:addr[,addr]` 形式注入
   `CURLOPT_RESOLVE`，URL 保持不变（`Host`、SNI、证书校验仍按名字）；IP 字面量不注入；
   IPv6 字面量与地址在注入时加方括号。TTL 到期或答案变化会替换条目，地址变化强制新建连接。
   连接阶段（解析 + libcurl 建连）共用一份预算
   `min(connect_timeout, 调用方响应头 deadline)`：解析耗时从该预算中扣除，libcurl 只拿到剩余部分
   （复审修正 3）。
2. **代理（§4.3）**：区分「代理端点解析」与「由代理解析目标域名」——使用代理时**不**在本地解析
   origin（请求以绝对形式或 `CONNECT` 交给代理解析），代理解析不上时也照常工作；代理端点若是
   名字则用同一解析器解析并注入。`CURLOPT_PROXY` 与 bypass 列表始终显式设置
   （有代理：`proxy(url)` + `noproxy("")`；无代理：`proxy("")` + `noproxy("*")`），因此
   `HTTP_PROXY`/`ALL_PROXY`/`NO_PROXY` 环境变量无法改变路由；代理凭据随代理 URL 传给 libcurl
   （预置 Basic，与 Hyper 连接器行为一致）。
3. **池（§4.4）**：见 `docs/libcurl-pool-semantics.zh-CN.md` §5/§6。`k = 0` 时保持旧的「不复用连接」
   语义（每传输 `FRESH_CONNECT` + `FORBID_REUSE`，池空闲即销毁），不使用
   `CURLMOPT_MAX_HOST_CONNECTIONS` 与 `CURLMOPT_MAX_TOTAL_CONNECTIONS`（后者会限制并发，
   见复审修正 2）；`pool_idle_timeout` 的活跃池回收按连接记账（复审修正 5）。
4. **错误映射与超时阶段（§4.2）**：`28 → Timeout`；`5/6/7/35 → Connect`；`18/55/56 → Body`；
   `8/34/52/61/65 → Request`；其余（含 `1/2/3/9/42/51/58/59/60/64/66/80/82/83/90/91` 与所有未列
   出的码）→ `Other`（不可重试）。取消走独立终态，不会变成可重试的写错误。错误消息包含
   curl code、阶段与 transfer id；未启用 low-speed abort 与整请求总 timeout，慢速恢复与重试
   预算仍由 session 管理。
5. **背压（§3.3）**：每传输队列预算来自会话 `MemoryBudget`（见交付物），仍在提交给 libcurl 前
   整块入队或整体暂停，写入回调不等待 Tokio/磁盘/限速令牌。
6. **请求调用与真实建连分开统计（§3.1）**：`DriverStats` 同时报告 `submitted`/`completed`/
   `cancelled` 与 `connections`（`CURLINFO_NUM_CONNECTS` 之和），并有测试验证两次请求只建连一次。

验证命令与结果（Windows，本机沙箱，全部 `--offline`；下表为复审修正后的复验数据）：

```text
cargo test  --offline --locked --lib                                        # 默认 curl：487 passed; 0 failed; 3 ignored
cargo test  --offline --locked --no-default-features --features curl-backend --lib
                                                                           # curl-only：487 passed; 0 failed; 3 ignored
cargo test  --offline --locked --no-default-features --features hyper-backend --lib
                                                                           # Hyper fallback：417 passed; 0 failed; 0 ignored
cargo test  --offline --locked --all-features --lib                         # 双后端：504 passed; 0 failed; 3 ignored
cargo test  --offline --locked --no-default-features --features curl-backend --no-fail-fast --tests
                                                                           # curl-only：88 passed; 0 failed
cargo clippy --offline --all-targets（default / curl-only / Hyper fallback）           # 三组均无 warning
cargo fmt --all -- --check                                                # 通过
```

P3 复审时的关键结论：**libcurl-only 构建下整套单元测试（486 条）与集成测试（m1–m12 与
http_header_timeout 共 88 条中的 87 条）直接通过**，且 P3 新增的 DNS、代理、池、错误表与背压
全部由独立测试覆盖；默认 curl、curl-only、Hyper fallback 与双后端构建分别由矩阵门禁覆盖。
相对复审前的 473 条，单元测试净增 13 条：driver 生命周期 3 条、空闲缓存上界 1 条、连接阶段预
算 2 条、活跃池空闲回收 1 条、按连接记账 2 条、`pool_semantics` 3 条新增 + 2 条重写，以及复审方
补充的 1 条 `fresh_connections_preserve_the_deadline_of_unused_cached_connections`
（同一记账规则的另一种触发路径：`forbid_reuse` 或 DNS 答案变化强制新建连接时，不得占走未使用
的空闲记录的到期时间）。

P3 相关缺陷与修正（均为实现过程中自测或走查发现）：

1. **RESOLVE 条目会无限累积**：P2 的实现每次请求都往 handle 的 `CURLOPT_RESOLVE` 列表里追加
   条目，既不记录已注入内容也不处理 TTL，等于把解析结果永久钉在池里。现在按池记账：TTL 内
   不重复注入，到期先移除再写入，地址变化强制新建连接。
2. **显式代理会被环境变量绕过**：P2 只在「无代理」时设置 `noproxy("*")`，配置了代理时没有设置
   bypass 列表，libcurl 因此会读取进程的 `NO_PROXY`/`no_proxy`。现在两种情况都显式设置
   `CURLOPT_PROXY` 与 bypass，并有测试证明 `NO_PROXY=*` 无法把请求从已配置的代理上引开。
3. **`enable_ipv6` 对 curl 后端无效**：P2 由 libcurl 自行解析，用户的 DNS/IPv6 配置没有任何
   作用；现在统一走共享解析器（`LookupIpStrategy`）并注入，`enable_ipv6 = false` 只返回 IPv4。
4. **每传输固定 256 KiB 预算与会话内存预算无关**：现在按 `MemoryBudget`/并发数推导并钳制到
   [64 KiB, 256 KiB]，小预算下仍在「队列空时接受整块」的前提下保证前进。
5. **头部块无上限**：解析器会把对端发来的头无限追加进内存；现在超过 128 KiB 即让请求失败
   （不是静默截断）。
6. **测试工具自身的缺陷（影响 88 条集成测试的耗时，不是实现缺陷）**：
   - `src/network/curl/driver/tests.rs` 与 transport 测试新增的并发用例里，若一次
     `Multi::messages` 回调中同时收到两个完成消息，只保留最后一个会在测试循环里永久丢弃
     另一个 handle（表现为测试线程 100% CPU 空转）。现在按同一趟收集全部完成消息后再统一
     `remove2`，并给这类用例加硬性 deadline：即使将来回归，也是失败而不是挂起。
   - `tests/m2_resume.rs` 的测试服务器把 `Range` 末端直接当切片下标，而客户端在知道文件大小
     之前会按整片请求（例如 `bytes=0-1048575`，文件只有 30 KB）——服务器 panic 后返回 500，
     而 5xx 按设计可重试，于是每个下载都要走完一遍 1 s→30 s 退避才回落到普通 GET：两条受影响
     的用例从 41.7 s / 36.9 s 降到 0.73 s / 0.53 s，该文件整个二进制从 46.9 s 降到 1.6 s。
     真实服务器会把范围夹到表示长度内，因此这是夹具 bug，不是实现问题。
   - `tests/m10_boundary.rs::test_dns_failure_propagates` 用默认重试预算（5 次、1 s→30 s 退避）
     验证「DNS 失败会把错误传出来」，单条测试要 ~50 s；现改为 `max_retries(1)` +
     10 ms 基础退避（重试策略本身由 `m5_retry` 覆盖），该文件 4 条测试从 ~50 s 降到 ~0.04 s。
   - 修正后单次集成测试的墙钟时间从 ~180 s 降到 ~57 s（`tests/m12_slow_transfer.rs` 的 25 s 是
     有意保留的真实等待：它断言的是**默认**慢速检测时长下的尾部分片恢复策略）。

### P3 验收复审与修正（复审发现 5 项）

复审先后提出 5 项缺陷，全部复现、修正并补了回归测试。这些问题的共同要求是：**契约必须由
可执行证据支撑**，而不是由选项名、注释或调用时序推断。

1. **driver 线程不随最后一个引用释放退出（高）**。`run_driver` 与 `ExitGuard` 各自持有一个
   `Arc<CommandQueue>` 强引用，而退出条件写的是 `Arc::strong_count(&queue) == 1`——在线程内部
   永远至少是 2，于是客户端全部释放后线程仍在，反复创建客户端会累积线程。修正：`ExitGuard`
   改为持有 `Weak<CommandQueue>`（等待者自己持有强引用，队列在有人等待时必然可升级），
   退出条件因此真正等于「除本线程外无人引用」。同时修掉了唤醒与计数之间的时序竞争：
   `DriverHandle`/`BodyStream`/`PendingHead` 释放自己那份引用时，若发现只剩「自己 + driver 线程」
   就**关闭队列**（关闭状态与 `wait` 用同一把锁记录，driver 不可能错过），否则只做一次唤醒；
   driver 的退出条件相应为「队列已关闭 或 强引用数 == 1」。回归测试：
   `the_driver_thread_stops_with_the_last_reference`（观察 `DriverShared::alive` 与线程计数）、
   `repeated_driver_lifecycles_do_not_accumulate_threads`（连开 4 个 driver 后线程计数回到基线；
   线程计数由 `driver::live_threads` 在测试构建下统计，生产构建无额外开销）、
   `a_live_body_stream_keeps_the_driver_running`（client handle 先释放、传输继续跑完，最后一个
   body 释放后 driver 才退出）。
2. **空闲连接上限调用了错误的 libcurl API（中）**。原实现调用 `set_max_total_connections`
   （`CURLMOPT_MAX_TOTAL_CONNECTIONS`）却按 `CURLMOPT_MAXCONNECTS` 的语义写注释与文档。实测：
   8 个并发请求 + 上限 1 时**只建立 1 条连接**（并发被串行化），而停止新增请求后缓存也不会按
   空闲上限收缩（复审侧复现为「结束后仍保留 7 条空闲连接」，两者都说明该选项不是缓存上界）。
   修正为 `Multi::set_max_connects(pool_max_idle_per_host)`，并实测其生效时机：
   libcurl 在「某条连接变为空闲」时检查 `缓存条数 > 上界` 并关闭**最久空闲**的连接（in-use 连接
   永不被关闭，因此不改变并发语义）；仅下调该选项不会关闭任何已空闲连接。因此
   `pool_max_idle_per_host = k` 的实测语义是「每池最多缓存 k 条空闲连接」；**当同一 origin 的并发
   连接数大于 k 时，连接会在空闲时被关闭并在下次请求时重建**，默认配置（`max_connections = 4`、
   `pool_max_idle_per_host = 4`）不受影响，`ClientNetworkConfig::http_idle_pool` 文档已说明该关系。
   回归测试：driver 侧 `the_connection_cache_keeps_at_most_the_configured_idle_connections`
   （8 并发 + k=1，结束后服务器只观察到 1 条连接），`CURLMOPT_MAXCONNECTS` 实测 3 个：缓存上界
   在空闲转移点生效、上界不限制并发峰值、仅下调上界不关闭已空闲 socket
   （`pool_semantics::max_connects_*`、`lowering_the_cache_bound_*`）。
3. **DNS 未纳入 `connect_timeout`（中）**。原实现只把解析限制在调用方的响应头 deadline 内，
   之后仍把完整 `connect_timeout` 交给 libcurl。实测：`connect_timeout = 50 ms`、响应头 deadline
   = 2 s、DNS 无响应时，请求要 2.006 s 才超时。修正：连接阶段使用单一预算
   `min(connect_timeout, 响应头 deadline)`，解析后把**剩余**预算作为 `CURLOPT_CONNECTTIMEOUT`
   下发；预算在解析阶段耗尽时直接以超时结束，不再发起连接。回归测试：
   `a_hanging_lookup_cannot_outlast_the_connect_timeout`（50 ms 预算下 < 600 ms 返回）、
   `the_lookup_time_is_deducted_from_the_connect_budget`（延迟 150 ms 的解析 + 100 ms 预算必须失败
   且服务器 0 次请求；同一解析在 2 s 预算下成功）。
4. **活跃池中的空闲连接不按 `pool_idle_timeout` 回收（中）**。原实现在池内仍有传输时直接返回
   「无回收期限」，因此同源一个长下载持续运行时，其他已空闲的连接可以一直留着；
   `CURLOPT_MAXAGE_CONN` 只在复用（或新建连接触发死连接清理）时检查年龄，补不上「主动关闭」。
   修正：池内记录空闲连接的到期信息（见下条修正后的记账方式），到期后调用
   `CURLMOPT_NETWORK_CHANGED | CURLMNWC_CLEAR_CONNS`——libcurl 8.21 新增、语义正是
   「空闲连接被关闭，进行中的传输继续使用它已有的连接」。该选项 `curl` crate 尚未封装，因此新增
   可选依赖 `curl-sys` 并直接调用（常量按公开头文件写出；老版本 libcurl 返回
   `CURLM_UNKNOWN_OPTION`，驱动只记录一次并退回「池空闲才销毁」）。回归测试：
   `a_busy_pool_closes_connections_left_idle_for_the_idle_timeout`（长传输 + 空闲连接并存时，
   空闲 socket 在 `pool_idle_timeout` 后被服务器观察到关闭，长传输 body 完整）、
   `pool_semantics::clear_conns_closes_idle_sockets_and_keeps_a_running_transfer`（原始 API 行为）。
   `DriverStats` 新增 `idle_clears` 计数。
5. **空闲回收计时被其他请求清除（中，复审二次发现）**。修正 4 的第一版用**池级**时间戳
   `cached_idle_since`：提交请求时清空、每次完成时重设。于是「同池一条长传输 + 两条空闲连接，
   新长请求只复用其中一条」时，另一条空闲连接永远不会到期（复审复现：`pool_idle_timeout = 150 ms`，
   450 ms 后仍有 3 条连接）；同理，任何一次完成都会把更早进入空闲的连接的到期时间推后。
   修正：改为**按连接记账**。`Pool::idle` 是一个小顶堆，保存每条空闲连接「变为空闲的时刻」：
   - 传输加入池时**领取**一条记录（libcurl 优先复用最久空闲的连接，领走的正是池原本会据以判定的
     那条），完成时若 `CURLINFO_NUM_CONNECTS = 0`（确实复用了它），该连接以**新时刻**回到堆里，
     其余记录不变；若 > 0（自己新建了连接），把领取的记录按原时刻**退回**；
   - 失败的传输会关掉自己的连接，其记录作废；被取消的传输退回尚未使用的领取记录；
   - 清理成功后堆清空并递增 `clear_generation`，清理时正在运行的传输不会声称自己的连接被缓存。
   - 到期判定只看堆顶（最久空闲的那条），因此**提交、完成、失败、取消都不会推迟其他连接的回收**。
   回归测试：`a_request_does_not_postpone_the_deadline_of_connections_it_does_not_use`（端到端：
   新请求只复用一条时，另一条仍按自己的到期时间被关闭，且两个传输都完整跑完）、
   `reusing_one_connection_keeps_the_other_idle_deadlines`（确定性单测：领取/退回/按连接到期/
   清理后作废）与复审方补充的
   `fresh_connections_preserve_the_deadline_of_unused_cached_connections`（强制新建连接的两种
   触发路径）。把实现临时改回池级时间戳后前两条都失败。

四项修正的复现证据：把对应实现临时改回旧行为后，上述 4 条新测试全部失败（分别表现为：线程不退、
8 并发被串行化为 1 条连接、2.006 s 才超时、5 s 内空闲连接不关闭），恢复修正后全部通过；
复审二次发现的第 5 项同样以「临时改回池级时间戳 → 两条新测试失败」验证。

P4/P5 已补齐用户可配置的 TLS 选项（额外 CA bundle、CA 目录、客户端证书/私钥），并保持
证书与主机名校验开启；证书错误仍按「确定性失败不可重试」处理。`curl`/`curl-sys` 与 vendored
libcurl 的生产版本审核、wheel 打包、干净环境安装与公网性能对照由 P4 门禁和发布流程承载，
实际公网对照与发布观察仍需在网络可用的 CI/发布环境完成。

当前只保留 `curl-backend` feature，默认构建、显式 curl-only 构建和 Python wheel 均使用
同一条 libcurl 路径。运行时遇错不自动切换后端，避免隐藏错误和重复传输；也不提供公开的
后端选择 API。

### P4/P5 实施记录（2026-09-13）

本轮把 P4 的构建/分发门禁和 P5 的默认切换接入仓库；旧的 Hyper 回退记录如下，已由 P6
清理：

- `Cargo.toml` 默认 feature 已切为 `curl-backend`；当时的双后端构建中默认选择 curl，Hyper
  仅由显式 feature 构建作为观察窗口回退。
- `.github/workflows/test.yml` 当时对 Ubuntu、Windows、macOS 分别跑默认 curl、curl-only 和
  Hyper fallback；发布 workflow 对 crates 与 Python wheel 使用显式 feature，并在 Linux wheel
  构建后用 Python 3.9 的干净虚拟环境执行离线安装和导入检查。
- Python binding 不再继承根 crate 的隐式 feature，默认和 maturin 发布命令都显式走 curl；
  `compare_public.py` 当时支持 curl/Hyper 两个 `public_compare` 二进制交错运行，报告保存各二进制
  的路径与 SHA-256。
- Rust/Python 文档已把生产默认、DNS `CURLOPT_RESOLVE`、TLS 配置边界和分发约束
  对齐；新增的 `ca_info`、`ca_path`、`client_cert`、`client_key` 只增加凭据/信任来源，
  不提供关闭证书或主机名校验的开关。

本机已完成离线编译与测试矩阵：默认 curl `487 passed / 0 failed / 3 ignored`、curl-only
`487 / 0 / 3` 加集成测试 `88 / 0`、Hyper fallback `417 / 0 / 0`、双后端 `504 / 0 / 3`，
Python 绑定单元测试 `13 passed / 0 failed`；这些数字保留为 P5 观察窗口的历史记录。被忽略的
3 条是本机 Schannel 证书材料不可用的 TLS 测试；P6 后续验证改以 libcurl-only 测试为准。

### P6 实施记录（2026-09-13）

P6 已完成，生产代码、构建矩阵和文档不再保留 Hyper 回退：

- `Cargo.toml` 删除 `hyper-backend` 及 `hyper`、`hyper-rustls`、`hyper-util`、
  `hyper-http-proxy`、`http-body-util`、`tower-service` 等仅供旧生产栈使用的直接依赖；
  `curl-backend` 是唯一生产 feature。
- `src/network.rs`、`src/http/body.rs`、`src/http/mod.rs`、`src/error.rs` 删除旧 client、
  connector、body adapter、请求体 trait 实现和 Hyper 错误转换；`BytehaulClient`、`HttpBody`
  和 DNS 路径均收敛到 libcurl。
- 删除旧的 Hyper 专用 `src/network/tls_tests.rs` 与未接入生产的
  `src/network/multi_ip_prototype.rs`。证书验证覆盖由 `src/network/curl/driver/tests.rs`
  保留：可信证书复用、不可信 CA 和主机名不匹配仍分别验证；本机 Schannel 材料缺失时按原规则
  标记为 ignored。
- CI、crates/Python 发布检查、公共对照脚本及架构文档改为只描述默认/显式 curl-only 构建。
  `warp::hyper` 仍仅作为测试 HTTP 服务的传递依赖，不是生产传输实现。

验证结果（Windows，本机沙箱，全部离线）：

```text
cargo check --offline --locked --all-targets                         # 通过
cargo test --offline --no-default-features --features curl-backend --all-targets
                                                                       # 单元 489 passed; 0 failed; 3 ignored；集成 88 passed
cargo test --offline --no-default-features --features curl-backend --doc
                                                                       # 1 passed; 0 failed
cargo test --offline --locked -p bytehaul-python --no-default-features --features curl-backend
                                                                       # 13 passed; 0 failed
cargo clippy --offline --workspace --all-targets --all-features -- -D warnings
                                                                       # 通过
cargo fmt --all -- --check                                               # 通过
cargo package --offline --locked --allow-dirty --list -p bytehaul       # 通过
```

3 条被忽略的 TLS 测试仍是本机 Schannel 证书材料不可用，分别覆盖可信连接复用、不可信 CA
和主机名不匹配；并非关闭证书校验。正常依赖树不再包含生产 Hyper 1.x 组件，`warp` 的测试
服务仍通过传递依赖保留 Hyper 0.14。

## 6. 验证与发布门槛

### 确定性测试

现有 `tests/m1_basic.rs` 至 `tests/m12_slow_transfer.rs` 和
`tests/http_header_timeout.rs` 是回归基础，另复用源码内 worker、TLS、multi/prefix/transfer
测试。补充以下 adapter 特有测试，不以公网速度作为 CI 断言：

- 慢响应头、每字节滴流、body 中途重置、空 body、chunked/trailer、1xx、CONNECT 多头块。
- 队列满后反复 pause/unpause，逐字节哈希；body error 排在已接受数据之后，不能伪装成功结束。
- DNS/排队/头部/body/背压阶段取消，以及 runtime 退出、client drop 和 driver 故障。
- 同时运行一个慢消费者与一个快任务，确认共享 driver 不被阻塞；累计内存、线程和 socket 可回收。
- 限速与慢磁盘下不误触低速恢复；动态拆分后旧响应残余不能重复提交或污染新 lease。
- 证书不可信和主机名不匹配必须失败，可信证书成功；验证错误不得通过关闭校验解决。
- 代理和 DNS 配置组合、TTL 更新、IPv4/IPv6、池开关及空闲回收按第 4 节验证。

CI 与本机复验使用的检查命令示例（本轮已执行 Rust feature 矩阵、Python 绑定测试、Clippy、
格式检查和根 crate 文档检查；workspace 文档检查受本机不可读的 `.pytest_cache` 目录阻塞）：

```text
cargo test -p bytehaul --all-targets
cargo test -p bytehaul --doc
cargo test -p bytehaul --no-default-features --features curl-backend --all-targets
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

feature 命令自 P2 起对生产后端有效：`--no-default-features --features curl-backend` 构建出的
就是 libcurl 后端。当前默认 feature 也选择 libcurl；P0 的 `curl::runtime`、`curl::driver`、`curl::pool_semantics` 测试与
P1/P2 的中立 body / transport 测试都在这条命令下运行。HTTPS 相关测试标记为 `#[ignore]`
（原因见 §5 的环境限制），CI 需要额外执行
`cargo test --no-default-features --features curl-backend -- --ignored`。CI 还须分别检查每个
构建方式，避免 feature 配置问题被掩盖。Python 扩展重建后执行绑定测试；保留现有
发布矩阵，检查 wheel 动态依赖、最低系统版本、干净环境加载及取消/暂停行为。

### 性能与分发

对比“相同调度器 + libcurl”和 aria2。覆盖单连接、8 路、多任务，
正常 DNS 与固定 IP、池开/关、大文件与小文件、本地延迟/断流和真实公网源。
统一 User-Agent、代理、TLS 校验、日志、重试预算、分片配置和进程计时边界，随机交错运行；
每条件至少 10 轮，性能测量与编译/测试错开。

逐轮保存 SHA-256、成功率、失败原因、耗时、CPU、峰值 RSS、线程数、实际连接数、
请求数、连接 IP 和 libcurl/TLS 版本。报告所有失败样本及分布，不能只统计成功的快样本。
建议把代表性场景中位耗时回退超过 10% 作为调查门槛，完成复测后才能推广；这是工程阈值，
并非已有实验结论。数据正确性、可取消性、资源有界和池契约是硬门槛。

依赖从实验验证过的 `curl 0.4.50` 起评估，但生产应审核实际解析到的 `curl-sys` 和 bundled
libcurl 版本。实验报告的 `8.21.0-DEV` 不能直接视为生产稳定版本承诺。
在 lockfile、CI 日志和发布产物中记录真实版本；确认 C 编译工具链、TLS/压缩依赖、许可证
及安全更新流程。具体 Cargo feature 以 [curl-rust 官方说明](https://github.com/alexcrichton/curl-rust)
和各目标实际构建结果为准。

## 7. 推荐起点

P0 的原型与池语义、P1 的中立接口、P2 的 curl driver、P3 的兼容补齐以及 P6 的清理都已完成
（记录见 §5）：`HttpWorker` 在唯一的 libcurl 后端上流式下载，头部及时返回，body 队列按会话
预算有界，取消后 driver 中没有残留传输。P3 的四项按第 4 节的顺序全部落地：

1. DNS/DoH：共享 Hickory 解析器 + `CURLOPT_RESOLVE` 注入，含 TTL 刷新、地址变化与
   IPv6 字面量处理；解析与建连共用同一份 deadline。
2. 代理：显式 `CURLOPT_PROXY`/bypass，隔离环境变量；代理模式下不本地解析源站；代理端点用
   同一解析器；代理凭据随代理 URL 下发（有测试）。
3. 池配置：按 origin/代理路由分池，`CURLMOPT_MAXCONNECTS = k`（空闲条数上界，在连接变为空闲时
   生效），池完全空闲超过 `pool_idle_timeout` 销毁、池仍在忙时用
   `CURLMOPT_NETWORK_CHANGED | CLEAR_CONNS` 关闭其空闲连接（到期时间按连接保留，新请求不会
   推迟其他连接），`k = 0` 保持不复用语义（见 `docs/libcurl-pool-semantics.zh-CN.md` §5/§6）。
4. 错误映射、超时阶段与 diagnostics 计数：错误表按阶段与可重试性分组，`DriverStats` 区分
   请求数与真实建连数，`HttpBody` 背压预算由 session `MemoryBudget` 推导。

P4/P5 的 CI 与发布门禁已接入，P6 已删除旧实现。后续只需在网络可用的 CI/发布环境继续运行
libcurl 与 aria2 的公网随机交错比较及资源压力测试，并按结果维护 libcurl 版本与打包流程；
这不再是保留 Hyper 回退的前置条件。
