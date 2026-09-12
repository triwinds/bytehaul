# libcurl 传输后端迁移计划

日期：2026-09-12。状态：待实施；本文仅规划迁移，不代表当前后端已经切换。

## 1. 目标与结论

建议将当前 Hyper HTTP/TLS 传输栈替换为 `curl` crate 驱动的 libcurl，采用专用线程运行
`Multi + Easy2`，向 Tokio 下载会话提供异步响应头和有界流式 body。保留 bytehaul 的
分片调度、动态拆分、连续 Range 请求、前缀保留、断点续传、重试预算、限速和存储提交逻辑。

这不是仅替换 TCP connector：libcurl 同时管理 HTTP、TLS 和连接复用，需要把当前暴露给
会话层的 Hyper 响应体一并抽离。最终目标是生产默认使用 libcurl；迁移期间保留 Hyper
作为同条件对照和构建时回退，验证通过后再移除旧生产实现。

依据是 [libcurl 公网对照实验](libcurl-comparison.zh-CN.md)。其中单连接中位速度更高，
但计时、日志和 IP 条件未完全对齐，8 路实验也有重置与超时。它支持投入迁移验证，
不支持承诺速度提升倍数，也不能把“8 个线程各跑一个 Easy”的实验直接搬进生产。

首版继续使用 HTTP/1.1。HTTP/2、HTTP/3、多 IP 竞速和分片策略调优分别评估，避免同时改变
协议、连接数含义和调度行为。多 IP 相关文档及 `src/network/multi_ip_prototype.rs`
属于后续设计参考，不作为本次切换的前置实现。

## 2. 当前代码边界

| 位置 | 当前职责与耦合 | 计划改动 |
| --- | --- | --- |
| `src/network.rs` | `BytehaulClient` 的直连/代理 Hyper client；Rustls、Hickory、网络配置 | 保留配置入口，拆出 resolver 与后端实现，增加 curl driver |
| `src/http/mod.rs` | `Response<Incoming>`、`Empty<Bytes>`、逐帧读取和 read timeout | 改为中立 request/response/body，保留 `next_data_chunk` 的调用语义 |
| `src/http/request.rs`、`response.rs` | GET/Range 构造、HTTP 元数据解析 | 将通用 HTTP 类型改为直接依赖 `http` crate，继续复用校验逻辑 |
| `src/http/worker.rs` | 重定向、最终 URL 缓存及失效刷新、状态码、响应头超时、请求诊断 | 保留应用层行为，适配中立类型，禁用 curl 自动重定向 |
| `src/session/single.rs`、`multi.rs` | 消费响应体、限速、写盘、恢复 | 仅适配 body 接口；不移动调度与提交职责 |
| `src/session/multi/adaptive.rs` | `ResponseStream` 直接保存 `hyper::body::Incoming`，读取长度/编码头 | 替换显式 Hyper 类型，回归连续请求、拆分和有效前缀 |
| `src/manager.rs` | 按 `ClientNetworkConfig` 缓存与共享 client | 缓存可克隆的 driver 入口，处理启动、释放及配置隔离 |
| `src/error.rs` | Hyper 错误转换、公开 `TransportErrorKind` 与重试判断 | 增加 curl 错误映射，保留公开错误模型 |
| `bindings/python/`、`.github/workflows/` | Python API、wheel 构建与发布 | 保持调用签名，补 native 依赖和分发验证 |

`scheduler`、`storage`、`rate_limiter` 不因换库重写。传输回调收到字节不代表分片已提交；
进度、piece map 和控制文件仍以现有会话/存储规则为准。

## 3. 推荐实现

### 3.1 中立 HTTP 接口

优先沿用 `http::Request`、`http::Response`、`HeaderMap`、`StatusCode` 和 extensions，
只引入内部 `HttpBody`，避免额外创造整套 HTTP 对象。GET 请求体可用 `()`；过渡期由
Hyper adapter 转成 `Empty<Bytes>`。`HttpBody` 提供异步分块读取和 drop 取消行为。

接口必须满足：

- 当前这一跳的最终响应头完整后即可返回 response，不等待 body 下载完毕。
- body 依序返回 `Bytes`；结束必须区分成功 EOF 与传输错误。
- 响应头之前失败交给 request future；之后失败交给 body，原始原因不能退化成普通 EOF。
- 已排队的有效字节先交付，随后返回终态错误，供现有前缀恢复逻辑处理。
- request future 被丢弃或超时、response/body 被丢弃，都能取消对应传输。
- 保留 `RequestDiagnostics` extensions；请求调用次数与真实建连次数分开统计。

初期可使用内部 enum 包装 Hyper/curl body，无需引入公开后端 trait 或让用户接触 libcurl handle。

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

迁移首版保留 Hickory，抽出不依赖 Hyper `Service<DnsName>` 的解析接口，继续支持多 DNS、
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
| P0 可行性与构建 | 在仓库内保留可复现的小型 Multi 原型；验证线程、pause、取消、池语义、DNS/代理；确定依赖及各平台 TLS | Windows/Linux/macOS 目标能构建；取消无泄漏；池 API 方案明确；运行时记录真实 libcurl/TLS/resolver 特性 |
| P1 接口抽离 | 增加中立 HttpBody 和 `http` 直接依赖，仅由 Hyper 实现；适配 worker、session、adaptive | 原有功能测试通过，行为与旧基线一致；生产 session 不再引用 Incoming |
| P2 curl 后端 | 增加 driver、Easy2 handler、头部状态机、有界流、终态和 drop 取消 | 本地 HTTP/HTTPS 单连接和多 Range 正确；首头即时返回；暂停重放无重复；完成/取消竞争可控 |
| P3 兼容补齐 | 接入 DNS/DoH、代理、TLS、池配置、错误映射、超时与 diagnostics | 第 4 节契约测试通过；原有暂停/续传、重试、低速、连续请求和前缀测试在 curl 下通过 |
| P4 打包与对照 | Rust/Python 构建矩阵、本地资源压力测试、公网随机交错比较 | wheel 干净环境可安装；输出哈希一致；资源有界；性能及失败率达到预先定义的门槛 |
| P5 默认切换 | curl 设为默认，更新架构/配置/发布说明；保留一个发布观察窗口的 Hyper 构建回退 | 默认构建和无默认特性的回退构建分别通过；错误、资源与性能无未解释回归 |
| P6 收尾 | 观察期完成后移除 Hyper 生产 adapter、旧 feature 和仅供旧栈的直接依赖 | libcurl 独立构建通过；Rust/Python 文档和示例无旧后端假设 |

临时 feature 建议为 `hyper-backend` 与 `curl-backend`。P1/P2 默认仍为 Hyper；两者同时启用
时须有明确选择规则，并给内部测试/基准提供显式选择入口。P5 默认改为 curl，Hyper 回退使用
`--no-default-features --features hyper-backend` 重新构建。运行时遇错不自动切换后端，
避免隐藏错误和重复传输；正式公开后端选择 API 不作为首版必要条件。

P6 检查移除 `hyper-rustls`、`hyper-util`、`hyper-http-proxy`、`http-body-util`、
`tower-service` 等直接生产依赖的可行性；`hyper` 可能仍是测试服务器或其他库的传递依赖。
`bytes`、`http`、Tokio 和 Hickory 按实际用途保留。同步审视测试专用 TLS 构造逻辑，
尤其是 `src/network/tls_tests.rs`，避免删掉旧 adapter 时把证书验证覆盖一起删掉。

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

后续实现阶段的检查命令示例（本文档工作不执行这些测试）：

```text
cargo test -p bytehaul --all-targets
cargo test -p bytehaul --doc
cargo test -p bytehaul --no-default-features --features curl-backend --all-targets
cargo test -p bytehaul --no-default-features --features hyper-backend --all-targets
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

feature 命令在 P2 实现后才有效；CI 还须分别检查每个单独 feature，避免 all-features 掩盖
缺失依赖。Python 扩展重建后执行绑定测试；保留现有发布矩阵，检查 wheel 动态依赖、
最低系统版本、干净环境加载及取消/暂停行为。

### 性能与分发

对比“相同调度器 + Hyper”“相同调度器 + curl”和 aria2。覆盖单连接、8 路、多任务，
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

先完成 P0 的 Multi 流式原型与连接池兼容验证，再做 P1 接口抽离。最早可以独立验收的结果
是：同一 `HttpWorker` 能在两个后端上流式下载，头部及时返回，body 队列有界，取消后
driver 中没有残留传输，输出哈希一致。达到这一步再接入完整会话与性能评估，最终按 P5/P6
完成默认后端替换和旧实现清理。
