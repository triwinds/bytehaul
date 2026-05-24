# 借鉴 aria2 的分片与连接复用改进计划

本文记录 bytehaul 可以从 aria2 的 HTTP 下载实现中借鉴的设计点，并把它们拆成适合本仓库逐步落地的计划。目标不是复刻 aria2 的命令对象模型，而是吸收它在分片正确性、进度统计、恢复语义和连接复用上的边界处理。

## 校准后的结论

原计划的大方向是正确的，但有几处需要更正：

- 当前代码已经把运行时下载进度和可恢复完成进度分开使用；问题主要是命名和 API 语义不够清楚，而不是完全缺失双进度模型。
- 当前“迟到数据污染新分配区间”还不是已证实的现有 bug。现有每个 piece 同一时刻只会有一个 worker 拥有，且失败路径会先等待 `DiscardPiece` 再重试或回收。`SegmentLease` 更准确地说是为后续 sub-segment、动态切分和更复杂回收路径提前建立边界。
- `WriteBackCache` 达到高水位时可能把未完成 piece 的部分字节刷到磁盘；控制文件仍只信任 completed bitset。后续如果扩展控制文件，只能把 inflight / dirty 信息当提示，不能当完成状态。
- 控制文件当前是固定 `VERSION = 1` 的 bincode payload；新增字段不是“顺手加字段”这么简单，必须设计 V1/V2 迁移或明确丢弃旧控制文件。
- 连接复用应先作为内部实验和基准验证，不应只因为测试通过就默认开启。

## 背景

aria2 的 HTTP 下载主线可以概括为：

1. `SegmentMan` 给连接分配当前要请求的 byte range。
2. `HttpRequestCommand` 发送带 `Range` 的请求。
3. `HttpResponseCommand` 校验 `206 Partial Content`、`Content-Range`、resume 元数据和 keep-alive 能力。
4. `DownloadCommand` 持续读取 body，并通过 `PieceStorage` / `DiskAdaptor` 写入目标文件。
5. 只有 piece 真正完成后，`PieceStorage` 才推进 bitfield 和可恢复进度。

这里最重要的启发是：aria2 把临时的网络下载区间和持久化完成状态分开建模。`segment` 表示某个连接当前领取的任务，`piece` 表示存储和恢复意义上的完成单元。

## 当前实现状态

bytehaul 当前已经具备几条比较好的基础：

- fresh probe 会在 `max_connections > 1` 时先发送 `Range` 请求；`HttpWorker::send_range` 会保留 `200 OK` 结果，让 session 层做 fresh fallback。
- 只有 fresh probe 得到 `206`、Range 响应未压缩、带有 `Content-Range` total，且文件超过 `min_split_size` 时，才进入多 worker。
- 多 worker 的普通 segment 会校验 `206`、`Content-Range` total/start/end，并在读取 body 时检查实际字节数必须等于 segment 期望长度。
- resume 多 worker 使用 `PieceMap` bitset 恢复完成态，`completed_bytes()` 只统计完整 piece，最后一个短 piece 使用真实长度。
- `WriteBackCache` 按 `piece_id` 聚合 offset range，合并相邻或重叠数据；piece 完成时按 piece flush，缓存达到高水位时也会 flush all。
- 多 worker 中 `ProgressSnapshot.downloaded` 的来源是 `multi.rs` 里 worker 累加的 `downloaded: AtomicU64`（received bytes，且 segment 失败时会 `fetch_sub` 回退）。
- 控制文件里的 `downloaded_bytes` 字段在两种模式下含义不同：多 worker 下来自 `scheduler.completed_bytes()`（已 mark complete 的 piece 累加），单连接下来自 `FlushAll` 返回的 `written_bytes`（已落盘前缀）。两者都不是 received bytes，但同名字段容易让阅读者把它读成单一概念。
- 保存多 worker 控制文件前会通过 `FlushAll { sync_data: true }` 让 writer 先 flush + `sync_data`；单连接保存路径同样依赖 `flush_stats.written_bytes` 作为已落盘前缀。
- `Downloader` 用 `client_cache: HashMap<ClientNetworkConfig, BytehaulClient>` 按生效网络配置缓存 `BytehaulClient`，避免重复构建 DNS/TLS/proxy client。
- 网络层目前通过 `builder.pool_max_idle_per_host(0)` 禁用 hyper 空闲连接池；`tests/m3_multiworker.rs::test_multi_worker_range_requests_use_distinct_connections` 显式断言默认多 Range 请求不会复用 TCP 连接。

主要差距是：

- Range 响应校验分散在 fresh probe（`session/mod.rs::run_fresh_from_response`）、resume probe（`session/resume.rs::try_resume_download`）、worker segment（`session/multi.rs` 里的 `check_segment_status` + `validate_segment_meta`）和 stream body（`stream_segment` 内部按 expected_len 检查）四处。segment 路径已经做过局部集中，但 fresh / resume probe 仍是行内分支判断，模式差异靠调用点隐含表达。
- `downloaded` 在公开 API、日志和内部变量中容易同时让人联想到 received bytes 和 resumable completed bytes；尤其是 `ControlSnapshot.downloaded_bytes` 在单/多模式下分别表示已落盘前缀和 completed bytes，加剧了歧义。
- scheduler 目前只分配完整 piece，`Segment` 还没有 lease id、attempt id、current offset 或 sub-range 的显式模型。
- writer 命令只携带 `piece_id`（`WriterCommand::Data { piece_id: Option<usize>, .. }`），没有携带 attempt/lease 身份；这在当前单 lease per piece 模型下可工作，但会限制后续 partial range 和动态切分。
- 控制文件只能表达完成 bitset 和 `downloaded_bytes`，无法描述”磁盘上可能存在但不可信的 partial dirty ranges”。
- 如果未来要让连接池参数（如 `pool_max_idle_per_host`、idle timeout）可配置，必须把它们加进 `ClientNetworkConfig`（当前 `Downloader::client_cache` 的 HashMap key），否则会复用到网络行为不同的 client。

## aria2 的连接复用策略

aria2 的连接复用主要由三层组成：

- HTTP keep-alive 默认启用。响应头阶段会判断连接是否可以继续复用。
- `DownloadEngine` 维护 socket pool，通过类似 `poolSocket` / `popPooledSocket` 的机制按目标 IP、端口、代理等 key 缓存和取回连接。取回时会排除超时、已经可读但可能被对端关闭的 socket。
- HTTP/1.1 pipelining 是可选能力，默认关闭，通常也没有必要照搬。

对分片下载来说，连接复用不等于减少并发连接数。一个正在下载 segment 的 worker 仍然占用一条活跃连接；复用主要发生在一个请求 body 完整消费并确认连接仍可保持后，下一次请求可以复用底层 TCP/TLS 连接。每个 Range 响应仍必须独立校验状态码和 `Content-Range`。

对 bytehaul 的含义：

- 不建议自己实现 aria2 风格 socket pool。hyper client 已经提供连接池能力，更适合 Rust async 生态。
- 当前禁用空闲连接池是安全但偏保守的选择；后续可以通过内部配置重新打开 hyper idle pool 做实验。
- 不建议支持 HTTP pipelining。它和现代服务端、中间代理、错误恢复的交互复杂，收益有限。
- 连接复用不能弱化 Range 校验。即使复用同一 TCP 连接，每个 segment response 仍必须独立通过校验。

## 设计原则

- `piece` 只表达完成态和恢复语义。
- `segment` 表达当前 worker 的临时下载 lease；未来可以是 piece 内的 sub-range。
- 控制文件只能声明已经 durable 且被完整验证为 completed 的数据。未完成 piece 的已落盘字节只是 dirty data，恢复时必须重下或重新校验。
- 运行时 UI 进度可以使用 received bytes；resume/control 进度只能使用 completed bytes 或单连接已落盘前缀。
- 连接复用是性能优化，不是正确性前提。连接是否复用不应改变调度、校验和恢复语义。

## 阶段计划

### M1: Range 响应校验集中化

状态：已完成（2026-05-24）。

- 已落地共享校验模块 `src/session/range_validate.rs`，把 fresh probe、resume probe、worker segment 的状态码、`Content-Range`、`Content-Encoding`、`Content-Length` 规则集中到同一入口表达。
- fresh probe 不再把“未进入 multi-worker 的 partial 206 响应”直接复用到单连接路径；当 probe 响应只是首个 piece 或带有非 `identity` 编码时，会改为重新发起完整 GET，再进入 single path。
- `src/session/mod.rs`、`src/session/resume.rs`、`src/session/multi.rs` 已切换到共享 validator；新增回归测试覆盖“小文件低于 `min_split_size` 但大于 `piece_size`”时必须 refetch full body 的路径。
- 本阶段验证：`cargo test range_validate --lib`；`cargo test --test m3_multiworker test_small_file_below_min_split_size_refetches_full_body`；`cargo test --test m2_resume test_resume_after_cancel`。

新增内部校验模块，例如 `RangeResponseValidator` 或 `range_validate`，把 fresh probe、resume probe、worker segment 的校验规则集中表达出来。当前 `session/multi.rs` 里已经存在 `check_segment_status` + `validate_segment_meta` 两个 segment 级别的小集中化函数，M1 应该把它们和 fresh / resume probe 的行内分支一起折进新的 validator，而不是另起炉灶。

建议把校验输入显式建模：

```rust
enum RangeValidationMode {
    FreshProbe,
    ResumeProbe,
    Segment,
}

struct ExpectedRange {
    start: u64,
    end_inclusive: u64,
    total_size: Option<u64>,
}
```

需要覆盖：

- fresh probe 收到 `206` 时返回“可进入多 worker 的候选”；收到 `200` 时返回“服务端忽略 Range，可 fallback 单连接”。
- resume probe 和 worker segment 必须要求 `206`，不能把 `200` 当作可续传响应。
- `Content-Range` total/start/end 必须与期望精确匹配；fresh probe 还要拿到 total 才能拆分。
- `Content-Encoding` 非 `identity` 时不能进入多 worker；fresh download 可以 fallback 单连接，resume/segment 应拒绝该 Range 路径。
- `Content-Length` 如果存在，应与期望 range 长度一致；body 实际长度仍由 stream 层做最终检查，不能只信 header。
- `416 Range Not Satisfiable`、`429`、`503` 等状态仍要保留现有错误和 retry-after 语义。

预期收益：

- 减少校验逻辑散落在 session 分支里。
- 后续引入 sub-segment、lease 或连接复用时，不会漏掉某条路径的 Range 校验。
- `HttpWorker::send_range` 可以继续作为较低层请求函数返回 `200`，由 validator 决定当前模式是否接受。

测试重点：

- fresh probe `200` fallback 单连接。
- resume probe `200` 丢弃控制文件并重新开始。
- `206` 但 `Content-Range` total/start/end 任一不匹配时拒绝。
- `206` 且 `Content-Encoding: gzip` 时 fresh fallback，resume/worker 拒绝。
- body 短读和多读都失败，并且不会提前标记 piece complete。

### M2: 双进度语义显式化

状态：已完成（2026-05-24）。

- 已把多 worker 运行时累计器从 `downloaded` 改为 `received_bytes`，并保留 `ProgressSnapshot.downloaded` 作为兼容字段；公开注释已明确它面向 UI、在多 worker retry 时可能回退，不等同于 durable resume 进度。
- 多 worker control snapshot 保存路径的局部变量和日志字段已改为 `completed_bytes`；单连接保存路径已改为 `persisted_prefix_bytes` / `pending_prefix_bytes`，避免把两种语义都继续记录成 `downloaded_bytes`。
- `ControlSnapshot.downloaded_bytes` 保持现有 on-disk 字段不变，注释已明确：单连接下表示 persisted prefix， 多连接下表示 completed pieces 的 durable byte count。
- 本阶段验证：`cargo test --lib test_worker_loop_retries_then_completes_piece`；`cargo test --test m2_resume test_resume_after_cancel`；`cargo test --test m3_multiworker test_multi_worker_download`。

把内部进度命名拆成两个明确概念：

- `received_bytes`: 当前运行中已经从网络收到、并计入 UI/速度/ETA 的有效字节。多 worker segment 失败时可以回退或重新采样。
- `completed_bytes`: 已完整 flush 并标记完成的 piece 字节，可用于 resume/control snapshot。

单连接路径还需要一个单独概念：

- `persisted_prefix_bytes`: 单连接 resume 中已经 flush/sync 的连续前缀，可写入控制文件的 `downloaded_bytes`。

落地建议：

- 内部先改名，把多 worker 的 `downloaded: AtomicU64` 改为 `received_bytes`，把控制保存路径中保存到 snapshot 前的变量改为 `completed_bytes` / `persisted_prefix_bytes`，以区分两条来源。
- 注意 `ControlSnapshot.downloaded_bytes` 是 on-disk 字段名，单纯改名会牵动控制文件兼容性，最好放到 M5 一起处理；当前阶段只在内部变量、日志键和文档里消歧。
- `ProgressSnapshot.downloaded` 是公开字段，为兼容可以继续保留，但文档要说明它面向 UI，对应 received bytes（多 worker 时会在 segment 失败后回退），不等同于多 worker 可恢复进度。
- 如果要新增公开字段，例如 `completed_bytes`，要评估 Rust 公共 struct 加字段的兼容性，并同步 Python 绑定和文档。
- 日志字段也要避免把两种含义都叫 `downloaded_bytes`。

测试重点：

- segment 失败后，`received_bytes` 可以被修正，但 `completed_bytes` 不提前增长。
- pause / cancel 时，控制文件不声明未完成 piece。
- 最后一个短 piece 的 `completed_bytes` 使用真实长度。
- 单连接控制文件保存的是已落盘前缀，而不是尚在 channel/cache 中的字节。

### M3: SegmentLease 与数据归属边界

状态：已完成（2026-05-24）。

- 已为 runtime segment 引入 `LeaseKey` / `lease_id`，并让 scheduler 在 `assign` / `renew` / `complete` / `reclaim` 路径上校验活动 lease，旧 lease 不能完成或回收新 attempt。
- writer/cache 已从按 `piece_id` 聚合切换为按 lease 聚合；新增 `BeginLease` / `FlushLease` / `DiscardLease` 消息，过期 lease 的迟到 `Data` 会被直接丢弃并归还内存 budget permit。
- 当前外部行为仍保持“一次分配一个完整 piece”，但 lease 归属边界已经独立于 `piece_id` 建模，为后续 sub-range 和动态切分提供了稳定身份层。
- 本阶段验证：`cargo test --lib test_scheduler_rejects_stale_lease_completion`；`cargo test --lib test_writer_drops_late_data_for_discarded_lease`；`cargo test --lib test_writer_keeps_same_piece_leases_isolated`；`cargo test --lib test_worker_loop_retries_then_completes_piece`；`cargo test --test m3_multiworker test_multi_worker_download`。

当前 `WriterCommand::DiscardPiece` 已经能丢弃失败 piece 的缓存数据；在现有“一个 piece 同时只有一个 worker attempt”的模型里，不能把迟到数据污染描述成已确认 bug。

但在引入 sub-segment、动态切分或更复杂的取消/回收之后，需要给 scheduler 发放的 segment 增加 lease 身份，并让 writer、scheduler 和 completion 路径都验证 lease。

建议结构：

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct LeaseKey {
    piece_id: usize,
    lease_id: u64,
}

struct SegmentLease {
    lease_id: u64,
    piece_id: usize,
    start: u64,
    end: u64,
    owner_worker_id: usize,
    attempt: u32,
}
```

建议消息流：

- `WriterCommand::Data` 携带 `lease_id`。
- cache 以 `(piece_id, lease_id)` 聚合，而不是只以 `piece_id` 聚合。
- `FlushLease` / `DiscardLease` 只影响对应 lease。
- scheduler 的 `complete` / `reclaim` 也校验 lease，旧 lease 不能完成新分配。
- writer 不应在每条数据上反查 scheduler；更好的做法是通过 `BeginLease` / `DiscardLease` / `FlushLease` 维护本地 active lease 集合。过期数据到达时直接丢弃，并归还内存预算 permit。

预期收益：

- 明确 worker attempt、writer cache 和 scheduler completion 的归属关系。
- 为后续 partial range、慢尾拆分和更细粒度恢复打基础。

测试重点：

- 旧 lease 的 `Data` 在 `DiscardLease` 之后到达时被丢弃，且 budget permit 被归还。
- 旧 lease 的 flush/complete 不能把新 lease 或新 attempt 标记完成。
- 同一个 piece 的两个 sub-range lease 不互相 discard 或 flush。

### M4: piece 与 segment 解耦

状态：已完成（2026-05-24）。

- scheduler runtime 已引入按 piece 维护的 `missing_ranges` / `completed_ranges` 状态，segment lease 现在可以表示 piece 内部任意子区间；piece 只有在完整覆盖后才会推进 `PieceMap` completed bitset。
- 现有默认分配策略仍先发放完整 piece，但内部已经支持多个同 piece subrange lease 并存、独立完成和独立回收；失败 subrange 只会把自己的区间放回 missing set，不会误伤同 piece 的其他 lease。
- 完成判断已改为基于区间覆盖而不是“一个 segment 等于一个 piece”，重叠完成区间会合并而不会重复计数。
- 本阶段验证：`cargo test --lib test_scheduler_partial_subranges_do_not_complete_piece_early`；`cargo test --lib test_scheduler_reclaims_only_failed_subrange`；`cargo test --lib test_range_set_merges_overlapping_completed_ranges_without_double_counting`；`cargo test --lib test_worker_loop_retries_then_completes_piece`；`cargo test --test m3_multiworker test_multi_worker_download`。

当前一个 segment 对应一个完整 piece。下一步可以允许一个 piece 被拆成多个 segment，但 piece 仍然只在完整覆盖并 flush 后标记完成。

可选策略：

- 先支持 `segment.start/end` 是 piece 范围内的子区间。
- `PieceMap` 保持 completed bitset 不变。
- 增加运行时 `PieceRuntimeState`，记录 active leases、received ranges、flushed ranges。
- 使用 interval set 或等价结构判断 piece 是否被完整覆盖。
- 初期不把 partial range 写入控制文件，恢复时仍整 piece 重下。

预期收益：

- 为慢尾优化和动态切分铺路。
- 不破坏当前控制文件的保守恢复语义。

验收标准：

- 完整 piece 覆盖前绝不 `mark_complete`。
- 重叠 sub-range 写入不会造成重复计数或错误完成。
- 失败 sub-range 只回收自己的 lease，不误伤同 piece 的其他有效 lease。

### M5: 控制文件 V2 dirty/inflight 提示

状态：已完成（2026-05-24）。

- control 文件 header 已升级到 `VERSION = 2`；保存路径默认写入包含 `snapshot` + `dirty_piece_ids` / `inflight_piece_ids` / `snapshot_seq` 的 V2 payload，同时仍兼容读取旧 V1 payload。
- `ControlSnapshot` 对外结构保持不变；resume 路径会读取并记录 hints，但恢复时仍只信任 completed bitset，不把 dirty/inflight 当作 partial completion 继续下载。
- multi-worker checkpoint 保存路径现在会从 scheduler runtime state 生成 hints；未完整完成的 piece 即使已有 partial covered range 或 active lease，也只会体现在 hints 中，不会推进 `downloaded_bytes`。
- 本阶段验证：`cargo test --lib test_save_load_roundtrip_with_hints`；`cargo test --lib test_load_v1_snapshot_upgrades_with_empty_hints`；`cargo test --lib test_scheduler_control_hints_track_dirty_and_inflight_pieces`；`cargo test --lib test_persist_multi_control_snapshot_writes_v2_hints_for_partial_piece_state`；`cargo test --test m2_resume test_resume_after_cancel`。

在保持 completed bitset 为唯一完成真相的前提下，控制文件可以增加恢复提示字段。但这必须作为控制文件格式演进处理。

当前 `ControlSnapshot` 使用固定 `VERSION = 1` 和 bincode payload。新增字段前需要先决定：

- bump 到 `VERSION = 2`，并支持读取 V1 后升级为 V2；或
- 明确在版本变化时丢弃旧控制文件并从头开始；或
- 引入显式 versioned payload enum，避免后续每次结构变化都靠 serde 结构体猜测。

建议 V2 先记录提示字段：

- `dirty_piece_ids`
- `inflight_piece_ids`
- `snapshot_seq` 或 `saved_at_unix_ms`

恢复语义：

- completed bitset 仍然是唯一可信任完成状态。
- inflight / dirty 只用于启动时识别可能有半截写入的 piece。
- 恢复时这些 piece 统一回到 missing，不能当成部分完成。
- 如果 hint 与 bitset 或文件长度矛盾，以保守重下为准。

后续如果要恢复 partial range，需要先引入 per-piece flushed range 校验和更强的落盘一致性约束。仅有 dirty/inflight hint 不足以证明 partial data 正确。

### M6: 连接复用实验

状态：已完成（2026-05-24）。

- 已增加实验性 idle-pool 配置：`DownloaderBuilder::http_idle_pool(...)` 与 `DownloadSpec::http_idle_pool(...)` / `disable_http_idle_pool()` 会进入 `ClientNetworkConfig`，因此也自动进入 `Downloader::client_cache` key，避免不同 pool 参数误复用 client。
- 默认值仍然保持 `pool_max_idle_per_host = 0`，因此现有默认行为不变；只有显式开启实验配置时才允许 hyper 复用空闲连接。
- 新增低层连接池测试覆盖 keep-alive 复用和 `Connection: close` 后自动重连；默认禁用 idle pool 的多 worker 集成测试仍保持“每个 Range 请求使用不同 TCP 连接”。
- 本阶段验证：`cargo test --lib test_idle_pool_reuses_keep_alive_connection`；`cargo test --lib test_idle_pool_reconnects_after_connection_close`；`cargo test --lib test_download_rebuilds_client_for_spec_idle_pool_override`；`cargo test --test m3_multiworker test_multi_worker_range_requests_use_distinct_connections`。

当前网络层关闭 hyper idle pool：

```rust
builder.pool_max_idle_per_host(0);
```

建议分两步做：

1. 增加内部实验配置，允许设置 `pool_max_idle_per_host` 和 idle timeout。
2. 用集成测试和基准确认复用行为、性能收益和失败恢复，再决定是否暴露公共配置或调整默认值。

实现注意：

- 如果 idle pool 参数进入 downloader/task 配置，必须加进 `ClientNetworkConfig`（它当前已经是 `client_cache` 的 HashMap key），否则不同 pool 参数的 client 会被错误复用。
- `tests/m3_multiworker.rs::test_multi_worker_range_requests_use_distinct_connections` 应保留，用来锁定默认禁用 idle pool 的行为。
- 新增开启 idle pool 的测试应只覆盖实验配置下的连接复用。

测试场景：

- keep-alive server 下，顺序 Range 请求可以复用 TCP 连接。
- `Connection: close` 或服务端关闭 idle socket 时，下一请求能自动重连。
- 多 worker 并发下载的活跃连接仍由 worker 数量限制，不因 idle pool 改变并发调度边界。
- Range probe 到 worker segment 的复用不会跳过 `Content-Range` 校验。
- 代理、DNS、TLS、connect timeout、idle pool 参数不同的 client cache key 不能误复用 client。
- pause / cancel 后未 drain 的 body 不会被放回 idle pool。

默认策略建议：

- 初期继续保持禁用 idle pool，避免改变现有稳定行为。
- 只有在集成测试、压力测试和基准都显示收益明确时，才考虑默认开启小规模 idle pool。
- 不做 HTTP/1.1 pipelining。

### M7: 动态切分和慢尾优化

状态：已完成（2026-05-24）。

- 已新增 `DownloadSpec::min_segment_size`，并让 multi-worker runtime 按 `max_connections` 启动 worker；当当前 missing range 数不足以填满 worker 且 range 足够大时，scheduler 会主动把未分配的 missing range 切成多个 sub-segment。
- 动态切分只作用于尚未发出的 missing range，不会去拆已经在飞的 active lease；因此不会引入“边下载边改 lease 边界”的重叠计数风险。piece 仍然只在完整覆盖后才 mark complete，控制文件 completed bitset 语义不变。
- probe response 只有在仍与分配到的 segment 范围精确匹配时才会复用；若动态切分让首个 segment 范围发生变化，则会丢弃 probe response 并发起新的精确 Range 请求。
- 新增验证同时覆盖正确性和收益：单 piece 也可以被拆成多个 subrange 并正确合并写回；在无可复用 probe response 的条件下，主动切分能显著降低 slow-tail 延迟。
- 本阶段验证：`cargo test --lib test_scheduler_splits_large_missing_range_when_workers_exceed_work_units`；`cargo test --lib test_scheduler_keeps_full_piece_when_remaining_tail_is_too_small_to_split`；`cargo test --lib test_run_multi_worker_dynamic_split_reduces_tail_latency`；`cargo test --test m3_multiworker test_multi_worker_dynamic_split_issues_subranges_with_single_piece`。

在 M3 / M4 完成后，再考虑动态切分。

可选策略：

- 当剩余 piece 少于空闲 worker 数时，把较大的未完成 range 拆成子 segment。
- 对长期低速 segment，只拆未收到的尾部区间。
- 保留 `min_split_size` 或新增 `min_segment_size` 约束，避免过度碎片化和小 Range 请求放大。
- 动态切分只操作未完成 range；已经完成或正在 flush 的 range 不应被重新分配。

验收标准：

- 总下载结果 byte-for-byte 一致。
- 任意 segment 失败不会导致 piece 误完成。
- 动态切分不改变控制文件中 completed bitset 的语义。
- 慢尾优化带来的吞吐收益要通过基准或可重复集成测试证明。

## 不建议短期照搬的能力

- 自己实现 socket pool。hyper 的连接池已经能覆盖主要需求。
- HTTP pipelining。收益有限，错误恢复复杂。
- 恢复半截 piece。除非引入 flushed range、lease 校验、piece hash 或其它完整性证明，否则容易制造 silent corruption。
- 多协议统一抽象。当前仓库聚焦 HTTP/HTTPS，先把 HTTP 分片正确性做扎实。

## 建议优先级

优先级从高到低：

1. M1 Range 响应校验集中化。
2. M2 双进度语义显式化。
3. M6 连接复用实验。
4. M3 SegmentLease 与数据归属边界。
5. M4 piece 与 segment 解耦。
6. M5 控制文件 V2 dirty/inflight 提示。
7. M7 动态切分和慢尾优化。

这个顺序的原因是：M1/M2 直接收紧当前代码的正确性表达和可维护性；M6 可以作为独立性能实验推进，但默认行为应保持不变；M3 是 M4/M7 的前置边界；M5 涉及控制文件格式演进，应在确认 partial runtime state 的形状之后再做。
