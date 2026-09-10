# 基于测速报告的 HTTP 传输改造计划

日期：2026-09-10。状态：P1/P2 已实施为默认值，P3–P5 待实施。

本轮无代理实测与后续慢连接对照见
[验证报告](http-direct-validation.zh-CN.md)，含逐轮 CSV 和公网未完成轮次的说明。

## 1. 结论与证据边界

优先解决默认的小请求反复建连和请求聚合。当前已有连接池、跨 piece 请求批处理和受保护的前缀续传能力，本轮已把连接池与 4 MiB 请求批处理设为默认，不重新实现这些机制。响应头等待治理、恢复预算细化属于后续独立改造，不能代替前两项。

本次核实基线：

- 外部输入：`docs/private/speed-report.md`，2026-09-09 的 Windows 诊断，声明使用 bytehaul 0.2.3、aria2 1.37.0。
- 当前仓库：远端 `master` 已快进到 `b7d549a`（v0.2.3）；包含 `be98b02` 的批处理和前缀保留实现。本轮默认值改动在此版本之上。
- aria2：核对官方 `release-1.37.0` 标签源码；下面列出具体函数和链接。
- 当前仓库已有实验：[HTTP efficiency results](../examples/http_efficiency_results.md)。这是历史合成 HTTP/TLS 测试记录，不是本次重新执行的公网测试。

外部报告的 17 轮耗时、连接次数、SHA-256 是输入证据；其诊断程序、Cargo.lock 和原始日志没有随报告放入本仓库，本文没有独立重放或验证这些原始产物。报告中的 `src/main.rs`、`src/dns.rs`、`src-tauri/target/...` 都属于原项目，不能当成本仓库的复现入口。

报告支持的判断是：固定 IP 后连接池仍有收益；慢速贯穿传输，不能只解释成最后一个分片卡住。默认 DNS 组存在 IP 混杂，固定 IP 每组只有两次样本，不足以承诺固定提速倍数、特定 CDN 限速规律或稳定追平 aria2。TCP 建连后的响应头等待还包含 TLS/请求/服务端等待，不能全部记为 TLS 耗时。

## 2. 实现差异核实

| 项目 | 当前 bytehaul | aria2 1.37.0 | 判断及改造含义 |
| --- | --- | --- | --- |
| 空闲连接复用 | 默认 `pool_max_idle_per_host=4`、30 秒；已有 `http_idle_pool`，网络层直接配置 hyper pool | keep-alive 默认 true，pipelining 默认 false；完整消费响应后满足条件才 pool socket | 报告的核心机制已落地为默认值；无需自建 socket pool。[A1][A2] |
| HTTP 请求与 piece | `piece_size=1 MiB`、默认 `request_batch_size=4 MiB`；多个独立 piece lease 共用一个有限 Range 响应 | 请求终点可以扩展至下一已使用 piece 的边界；响应未结束时可以领取相邻 segment 并继续读同一响应 | aria2 内部 piece 不等于请求大小。当前 bytehaul 已用默认有限批次缩小差距，仍不宣称完全等价。[A3][A4] |
| `min_split_size` | fresh 路径主要用 `total_size > min_split_size` 判断是否进入 multi；未分配区间拆分另受 `min_segment_size` 管理 | `--min-split-size` 约束可拆区间，小于两倍阈值的区间不拆；还受 split、每服务器连接数约束 | 同设 4 MiB 不是同一请求策略，不应通过改变此参数含义来追齐 aria2。[A5] |
| 首个请求 | 多连接先请求 `0..piece_size-1`；精确匹配首个 lease 时直接消费 probe，随后才批处理；小文件、忽略 Range 等另走 fallback | 无 segment 时先发普通 GET，后续下载链可以跨 segment 持续读取 | 报告所述保留初始 GET 与源码机制相符；但不能推导所有 aria2 运行都只有 4 个请求。bytehaul 也不是必然额外丢弃一次 probe。[A3][A4] |
| 慢速与响应头 | `Observation::advance/sample` 只累计 Reading；Headers 不产生慢速证据；`request_with_timeout` 仍给请求到响应头设置超时 | `DownloadCommand::checkLowestDownloadSpeed` 在下载阶段检查显式低速阈值，默认成员值为 0 | bytehaul 的 Headers 盲区确实存在于自适应检测，不等于完全没有超时；没有证据证明报告中 aria2 靠自适应抢救 Headers 获胜。[A4] |
| 恢复预算 | `extra_limit=min(total/100,16 MiB)`；`reserve` 按当前 segment 全长累计预留；有冷却、lineage 次数、hedge 和退避门槛 | 本次核实的 HTTP 连续读取路径不能证明与此预算存在一一对应机制 | 对 64,469,455 字节文件，644,694 字节预算不足以批准普通 1 MiB segment 的性能恢复；不是所有尾片和拆分后的 segment 都必然被拒绝 |
| 已确认前缀 | 强 ETag 和兼容条件头下，失败/回收可经 writer 确认后只重下后缀；完整 piece 才进入持久完成态 | 本计划借鉴其持续传输机制，不复制其存储恢复实现 | 此能力已经存在，后续预算设计应利用它，不能以“实现前缀续传”重复立项 |

本地代码定位（相对于仓库根目录）：

- `src/config.rs`：`DownloadSpec::new`、`request_batch_size`、`http_idle_pool`、`disable_http_idle_pool`。
- `src/network.rs`：`ClientNetworkConfig::{default,build_client}`、`BytehaulClient::request_with_timeout`；pool 配置属于 client key。
- `src/manager.rs`：builder pool 默认值、显式 task override 和 client cache 选择。
- `src/session/mod.rs`：`probe_or_fallback_get`、fresh 响应分流。
- `src/scheduler.rs`：`extend_batch`、`retain_prefix`；批次最多 64 个 lease，遇到已完成/活动/已触碰 piece 停止，并为其他 worker 留工作。
- `src/session/multi/adaptive.rs`：`Coordinator::new/reserve`、`Observation`、`run_attempt`；每个请求共用 slot 和 observation，跨 piece 保留速度历史。
- `src/session/multi.rs`：`settle_prefix`、probe 精确匹配和 writer 确认。

## 3. 目标、范围和不变量

目标是减少可避免的 TCP/TLS 建连与响应等待次数，让现有能力能以可验证、可回退的配置用于公网下载，同时改善有明确证据的长等待恢复。

本轮已修改下载默认值和对应回归测试；后续阶段按下列顺序分别验收，不在没有新证据时扩大到响应头 hedge、固定 IP 或外部产品配置。

所有阶段保持：

1. `piece_size` 继续表示存储/断点粒度；请求批次和并发槽位单独计量。
2. 每个响应独立验证状态、Content-Range、对象身份、编码和 body 长度；取消的未读完 body 不人为归还池。
3. 停止旧 producer 后，经过 FIFO writer acknowledgement，才保留前缀、更新 lease 或回收后缀。
4. UI 有效进度不包含重复流量；跨进程恢复仍只信完整 durable piece，不增加控制文件格式。
5. 重试次数、总重试时间、Retry-After、性能恢复冷却和并发上限不因切批或换 lease 被重置。
6. 本轮方案不包含 HTTP/2、pipelining、自建连接池、固定 CDN IP 或更换 TLS 后端；报告不足以支持这些改造。

## 4. 分阶段实施

### P0：固定版本并补全诊断基线

**改动位置：** 扩展 `examples/http_efficiency_compare.rs` 的现有 fixture 和结果输出；公网比较使用单独诊断入口，避免把外部产品工程路径带入库。结果写入 `examples/` 的独立报告/CSV。

- 获取原项目锁文件和依赖 source/checksum，记录 aria2 二进制 SHA、完整参数、Rust 工具链、目标平台；历史 0.2.3 与当前 commit 分开跑。若无法取得历史源码，只报告当前 checkout 结果，不阻塞后续本地验证。
- 对 64,469,455 字节大小建立确定性本地样本。统一 4 worker、1 MiB piece、4 MiB min split、UA、代理/IPv6策略、预分配、超时及计时边界。
- 分别测默认、仅 pool、仅 batch、pool+batch；batch 候选为 4/8/16 MiB。另设 aria2 1.37.0 对照。先固定 batch 的 2×2 实验，再比较 batch 大小，避免一次混入所有变量。
- 统计请求发起数、206 数、失败/重试数、TCP 尝试与成功数、复用数、请求长度分布、首字节和 50/90/100% 时间、body 字节、有效/重复字节、内存峰值。不能把服务端成功 write 的字节称为抓包总流量。
- 日志使用 request ID 关联跨 piece 的一次请求；记录请求到响应头时间、Reading 和本地背压耗时。TCP/TLS 细分只有 connector 能可靠观测时才报告，不能用 Headers 采样数量换算时间。

**验收：** 每轮产物校验一致且可追溯版本；连接数、请求数、请求长度三项能独立解释。保留日志开启的诊断轮与低日志性能轮，编译及哈希验证不进入传输计时。

### P1：把连接池从实验能力推进到可采用配置（最高优先级）

**已实施：** 默认配置为 `http_idle_pool(4, Duration::from_secs(30))`，4 是每 host 的空闲上限，不是新增全局并发上限。复测时仍可用 `disable_http_idle_pool()` 隔离收益。

**通过验证后的改动位置：** `src/config.rs`、`src/network.rs`、`src/manager.rs` 及对应默认值/override 测试、Rust/Python 文档和配置规范。

- 默认值已采用每 host 4 条、空闲 30 秒；是否继续调整这两个数值仍需跨平台和公网复测，不能仅凭单一公网样本反复修改。
- builder 和 task 继续共享默认语义，保留显式 `disable_http_idle_pool()`；task 禁用可以覆盖 builder 启用，默认字段不会伪装成显式 override。
- 评估长期复用 Downloader 时的 client-cache 数量与每 host 空闲连接总量；pool 上限不是整个 Downloader 的 socket 上限。
- Python 目前没有独立 pool 开关，两个入口直接继承 Rust 默认的 4/30 秒连接池；显式 pool 调优仍保留在 Rust API。未来若向 Python 暴露开关，需经共享配置转换并明确 None 继承、0 禁用、正数启用和超时单位。

**验证：** 已有低层 HTTP/TLS pool 测试之外，补公开 Downloader 多 worker HTTPS 覆盖、服务端主动 close/空闲失效、代理连接、暂停取消、不同 TLS/DNS/proxy/pool 配置隔离。默认“每个 Range 不同连接”的旧测试改为显式禁用模式，再新增默认模式复用断言。

**验收：** 可复用本地服务端下连接量降至并发量附近；不把这个值硬编码成所有重试/重定向场景上限。所有响应和文件仍正确，无 socket 泄漏、失效连接导致的不可恢复错误。回退只需显式关闭池。

### P2：推广已有请求批处理，必要时改善批次均衡

**已实施：** `request_batch_size` 已暴露于 Rust 和两种 Python 下载入口，默认 4 MiB，设置为 0 时关闭。后续仍可比较 8/16 MiB，不增加 piece_size 来获得大请求。

**本轮改动位置：** `src/config.rs` 的默认值、配置单元测试、`tests/m3_multiworker.rs` 的默认行为回归以及 tuning/advanced/Python 文档；scheduler 的批次算法保持不变。只有数据证明默认配置仍不能充分利用并发时，才修改 `src/scheduler.rs::extend_batch` 和 `src/session/multi/adaptive.rs` 的批次分配，并补 `transfer_tests.rs`/scheduler 测试。

- 当前字节 cap 是聚合目标，不会把一个大于 cap 的 piece 强制拆小；最多 64 lease。保留这些现有契约。
- 当前给其他 worker 留的是工作区间数量，不保证剩余字节均衡。大 batch 可能让一个请求占据较多尾部工作；根据请求长度分布验证后，再考虑依据剩余字节和可用 request slot 限制单次预留量。
- 默认值与显式配置保持同一实现；`request_batch_size(0)` 明确关闭，不把 0 偷换成 auto。
- 保留匹配 probe 的消费路径；批次请求的中间 piece 完成不重置请求速度历史，失败时释放未开始 lease。断点存在洞、部分 piece 和活动 lease 时不得跨洞聚合。

**验收：** 固定本地样本下批处理请求数显著少于单 piece 模式，输出正确、活跃请求不超 4；不要求恰好等于 aria2 的 4/7/5 次。覆盖跨 piece frame、截断、多读、身份变化、中途取消、小内存背压和 batched 慢尾；证明较大批次没有靠牺牲尾部延迟取得平均吞吐收益。回退配置为 batch=0。

### P3：对响应头等待建立独立治理

**问题边界：** 当前 `Headers` 大体覆盖请求 future 的等待，可能包含 DNS、连接、TLS 和响应头。fresh probe 发生在 adaptive coordinator 建立之前；只修改 Observation 会漏掉首请求，报告的首次出数据约 14 秒正包含此阶段。

**改动位置：** 请求耗时和超时配置在 `src/http/worker.rs`、`src/network.rs`；probe/fallback 在 `src/session/mod.rs`；多 worker 策略在 `src/session/multi/adaptive.rs`；若新增公共选项，同步 `src/config.rs`、Python 转换与文档。

- 先输出耗时与超时原因，不将 Headers 强行计为 Reading=0；继续排除限速、内存、channel 和 writer 等待。
- 第一版优先增加可单独配置的响应头等待期限，默认继承现有请求 timeout；明确它从请求发起计时，包含建连，不能误命名为纯服务端 TTFB。连接超时仍独立生效。
- 对 probe、重定向、fallback、普通 Range 和续传统一规定作用范围；取消和超时沿既有 RetryState 处理，避免 probe→GET 切换绕过用户期望的总等待边界。
- 自动缩短等待期限属于后续实验：需要同源成功请求延迟样本、保守下限和全体变慢抑制；没有成熟样本时使用配置的硬超时。第一版不启用 Headers hedge。

**验收：** 确定性服务端分别控制 probe 和后续 Range 的响应头延迟，证明期限生效、正常慢首响不被默认新策略打断、取消及时、429/503 与 Retry-After 不被性能策略绕过。通过阶段时间证据确认收益，不以更短默认 timeout 掩盖网络问题。

### P4：把性能恢复预算与实际额外成本对齐

**改动位置：** `src/session/multi/adaptive.rs` 的 `reserve`、取消/回收和诊断计数，以及 prefix/writer 确认测试；同步 slow-transfer 规范。

- 先增加恢复拒绝原因计数：证据不足、预算不足、冷却、并发不足、validator 不可用、限速抑制等。`recovery_actions=0` 不再是唯一可见结果。
- 分离“先取消再续传”和“主请求+挑战者并行”。前者在强 validator、writer 确认前缀后只重下后缀，后缀是必需流量，不应直接按完整 segment 当作额外流量。
- 新预算模型要同时记录已消耗和在途预留：安全前缀路径按取消时无法保留的 read-ahead/丢弃字节保守记账；无 validator 的重放和 hedge 按其最坏额外字节预留。成功/失败/取消均结算，不能在每个新 lease 上清零。
- 先支持预算允许时的非并行恢复；如果完整 hedge 超过剩余预算，保持不启动。为约 61 MiB 文件设置最小 hedge 配额会改变原 1% 上限，应作为后续独立选项评估，不在本阶段静默扩大预算。
- 继续保留 10 秒冷却、lineage 次数上限、收益判定和全体变慢抑制；零 body 的重启也不能无限重试。区分性能额外预算与普通故障重试流量，禁止把 1% 描述成所有网络重复流量的硬上限。

**验收：** 对本报告大小的文件，强 ETag 且无不可保留前缀的慢请求，不再仅因 1 MiB 全长大于 644,694 字节而一律被拒；不足预算的 hedge 仍被拒。精确验证 read-ahead、弱 ETag、失败 challenger、重复恢复、取消和 writer 失败的结算；确保不会重复记进度或提前标记 piece 完成。

### P5：按剩余差距决定是否改初始请求策略

仅当 P1/P2 后的阶段数据证明 probe 限制仍占主要开销，再单独设计普通 GET 保留或更大初始 Range。涉及 `src/session/mod.rs`、probe handoff、Range validator 和调度；收益目标是缩短启动阶段并减少请求转换。

这不是 aria2 请求数量的机械复制：普通 GET 的无限/长 body 与并行 Range 可能重叠，需要先设计唯一写入归属和取消边界。必须覆盖小文件、未知长度、Range 被忽略、压缩响应、重定向和断点入口。收益不足则不实施，保留精确 probe 方案。

## 5. 复测与发布门槛

依赖顺序：P0 → P1/P2 的显式配置验证 → 各自默认值评审；P3/P4 在诊断数据具备后分别实施；P5 最后按证据决定。一次变更只推广一个机制，保留独立开关便于归因和回退。

| 层级 | 场景 | 通过条件 |
| --- | --- | --- |
| 确定性功能 | HTTP/HTTPS、close、失效连接、批次边界、断流、身份变化、暂停续传 | 每个输出字节正确；请求/并发/预算/持久状态符合契约 |
| 合成性能 | 分别增加连接建立延迟、每响应延迟、持续低速、仅响应头慢 | pool 主要减少连接，batch 主要减少请求，恢复策略有明确触发证据；不用公网速度做 CI 断言 |
| 公网比较 | Windows 与原报告环境优先；多个源和固定 IP 交叉顺序；同时保留正常 DNS 组 | 每条件至少 10 轮，报告中位数、离散程度、失败率和逐轮数据；小样本不声称稳定 P95 |
| 长尾与兼容 | 大小文件、4/8/16 MiB batch、小内存、限速、代理、多任务共用 client | 收益不以明显尾延迟/错误率/资源增长为代价；异常单独调查，不能只平均掉 |

建议默认值推广门槛：在预先选定的延迟场景中达到预期连接/请求数下降；代表性性能样本的中位耗时改善，且其他场景超过 10% 的中位回退均有解释并完成复测。10% 是待采用的工程回归阈值，不是现有实验结论。公网不承诺固定 MiB/s 或相对 aria2 的速度倍数。

后续实施验证命令（本次文档工作未执行）：

```bash
rtk cargo test -p bytehaul --all-targets
rtk cargo test -p bytehaul --doc
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo fmt --all -- --check
rtk cargo run --release --example http_efficiency_compare -- 10
```

性能实验独占运行，与编译和测试错开，并按现有 example 要求处理代理环境。Python 接口变化需重建扩展后执行绑定测试。最终更新对应规范、英中文配置说明和示例结果，清楚区分已发布版本、当前源码和候选默认值。

## 6. 与已有计划的关系及 aria2 依据

本计划接续 `docs/aria2-inspired-plan.zh-CN.md` 的 M1–M7。其连接池实验、lease/subrange、动态拆分等已经存在；不能把旧文档后半部分的历史建议当成当前缺失能力。当前批处理和前缀语义以 `examples/http_efficiency_results.md` 及本次核实源码为准。

- [A1：aria2 1.37.0 OptionHandlerFactory.cc](https://github.com/aria2/aria2/blob/release-1.37.0/src/OptionHandlerFactory.cc)：`PREF_ENABLE_HTTP_KEEP_ALIVE`、`PREF_ENABLE_HTTP_PIPELINING` 默认值。
- [A2：aria2 1.37.0 HttpDownloadCommand.cc](https://github.com/aria2/aria2/blob/release-1.37.0/src/HttpDownloadCommand.cc)：`prepareForNextSegment` 的 socket pooling 条件。
- [A3：aria2 1.37.0 HttpRequestCommand.cc](https://github.com/aria2/aria2/blob/release-1.37.0/src/HttpRequestCommand.cc)：`executeInternal` 的无 segment GET、`getNextUsedIndex` 和 end offset override。
- [A4：aria2 1.37.0 DownloadCommand.cc](https://github.com/aria2/aria2/blob/release-1.37.0/src/DownloadCommand.cc)：`prepareForNextSegment` 的连续读取以及 `checkLowestDownloadSpeed`。
- [A5：aria2 官方手册](https://aria2.github.io/manual/en/html/aria2c.html#cmdoption-k)：min-split-size；同页 split、max-connection-per-server 和 stream-piece-selector 的定义。
