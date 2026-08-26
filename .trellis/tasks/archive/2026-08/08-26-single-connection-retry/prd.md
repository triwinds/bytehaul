# 实现单连接响应体自动重试

## Goal

补齐 bytehaul 单连接下载在响应体传输阶段的重试能力：当 body 发生可重试错误时，从 writer 已确认并完成 flush barrier 的连续前缀安全续传；当远端对象变化、Range 被忽略或总大小未知时，安全地从零重启，始终避免重复写入、字节跳过或拼接不同远端对象。

用户价值：`max_connections = 1` 与多连接下载对网络瞬断、超时和短读具备一致的恢复能力，同时保持现有公开配置与恢复文件语义不变。

需求来源：`docs/private/single-connection-retry-plan.zh-CN.md`。

## Background and Confirmed Facts

- 响应体传输错误已被归类为可重试错误（`src/error.rs:163-189`、`src/http/mod.rs`），但分类本身不会触发重试。
- fresh probe、plain GET 和 resume probe 只在“获取 response”阶段调用 `retry_with_backoff`（`src/session/mod.rs:39-75`、`src/session/resume.rs:239-248`）。
- `run_single_connection` 只接收已经打开的 response，不持有 `HttpWorker`；`stream_single` 的 body 错误会直接结束（`src/session/single.rs:40-55`、`src/session/single.rs:290-344`）。
- 多连接把 Range 请求、响应校验和完整 body 消费放在同一 segment attempt 循环内，但其退避没有使用通用 helper 的 equal jitter，已经发生策略漂移（`src/session/multi.rs:569-711`）。
- 单连接 `WriterCommand::Data { lease_key: None }` 会顺序直接写入；FIFO 的 `FlushAll` ack 可作为已提交写入的 barrier，其 `written_bytes` 在单 producer、顺序 offset 的前提下代表连续前缀（`src/storage/writer.rs:81-148`）。
- 集中的 Range validator 已支持 `FreshProbe`、`ResumeProbe`、`Segment` 三种严格校验模式（`src/session/range_validate.rs:1-186`）。
- `max_retries` 的现有公开语义是“初次尝试之后允许的额外重试次数”，`0` 表示不重试；`max_retry_elapsed` 是额外的时间预算。

## Requirements

### R1 — 统一重试决策

- 提取不执行网络 I/O 的共享重试状态/决策组件，统一处理：
  - `DownloadError::is_retryable()`；
  - 额外重试次数；
  - `max_retry_elapsed` 当前边界及 `elapsed + backoff` 边界；
  - `Retry-After`；
  - 指数退避与 equal jitter。
- `retry_with_backoff`、multi-worker segment attempt 与新增 single-transfer attempt 必须使用同一决策语义。
- 不改变 `max_retries`、`RetryBudgetExceeded`、Cancel/Pause 的公开行为。

### R2 — 单连接完整 transfer attempt

- 单连接 orchestration 必须持有 `HttpWorker`（或等价请求能力）、metadata baseline、writer 生命周期和一个覆盖完整传输的 `RetryState`。
- 调用者已经取得的 fresh/resume response 必须作为 attempt 0 复用，不能无条件重复请求。
- 单个 attempt 只负责消费一个 response body，并返回完成或携带 `received_in_attempt` 的失败结果。
- body、后续 Range 建连和响应校验失败必须共享同一个 single-transfer retry scope；每次新请求不得重置次数或 elapsed。
- 已知总大小时，EOF 只有在 final offset 精确等于总大小时才算完成；短读和多读均失败。

### R3 — 以持久化连续前缀续传

- body 失败后必须先丢弃旧 body，再通过 writer flush barrier 获取 `persisted_prefix_bytes`，随后才能发起新请求。
- 下一次续传请求必须为 `Range: bytes=<persisted_prefix_bytes>-<total_size - 1>`。
- 续传响应必须通过 `ResumeProbe` 等价校验：`206`、`Content-Range` start/end/total、可选 `Content-Length`、实际 body 长度和初始 metadata baseline 均一致。
- `received_in_attempt` 只用于日志/UI 校正；续传 offset 和控制快照不得使用尚未经过 barrier 的网络接收字节数。
- writer/channel/storage 错误、Pause、Cancel 和其他不可重试错误立即结束，不进入网络重试。

### R4 — 安全地从零重启

- Range 返回 `200`、ETag/Last-Modified/total 不一致，或总大小未知且 body 失败时，不得在非零 offset 追加。
- 从零重启必须按顺序完成：关闭旧 writer、truncate/recreate 输出、删除或重置旧控制快照、清零单连接进度与 ETA 采样，并从完整 GET/可安全复用的完整 `200` response 建立新的 metadata baseline。
- 从零重启与原 transfer attempt 共用同一重试次数和时间预算，远端持续变化时不会无限循环。
- 可评估 `If-Range` 作为额外服务端保护，但客户端严格校验是必需条件，验收不依赖 `If-Range`。

### R5 — 兼容性与可观测性

- 不改变 Python binding 的公开参数或现有错误类型，除非实现发现无法避免且重新回到规划阶段确认。
- 不改变多连接 lease、piece completion、控制文件完成态和每个 segment assignment 独立 retry scope 的语义。
- 单连接重试日志至少能表达：attempt/max retries、error、resume offset、本 attempt 接收量、backoff、是否从零重启及原因、elapsed。
- 更新中英文 architecture/tuning/advanced/troubleshooting 与相关 Rust/Python 使用说明，明确单/多连接都覆盖 body retry、equal jitter，以及 `max_retries` 是额外重试次数。

## Acceptance Criteria

- [ ] AC1：`max_retries = 0` 时第一次错误原样返回；默认值 5 时最多为 1 次初始尝试 + 5 次重试；不可重试错误无等待。
- [ ] AC2：通用 helper、单连接 transfer 和 multi segment 对 `Retry-After`、equal jitter、时间预算上下界使用同一共享决策；Cancel/Pause 可打断 backoff。
- [ ] AC3：`max_connections = 1` 时，首次响应截断、第二次按正确 Range 返回余下内容，最终文件逐字节正确且请求次数准确。
- [ ] AC4：存在 writer/channel 缓冲时，后续 Range start 等于 flush barrier 返回的连续前缀；无重复写入、跳字节或进度虚增。
- [ ] AC5：已知大小的单连接 body 短读和多读均不能提前完成；重试耗尽返回最后一个传输错误，时间预算耗尽返回 `RetryBudgetExceeded`。
- [ ] AC6：Range 返回 `200` 时先 reset/truncate 再从零消费；结果不出现“旧前缀 + 完整文件”。
- [ ] AC7：ETag、Last-Modified 或 total 改变时绝不拼接两个对象；在同一预算内从零建立新 baseline，持续变化最终受预算限制停止。
- [ ] AC8：未知总大小的可重试 body 失败从零 GET，第二次完整响应可成功；控制快照不记录无法证明的 offset。
- [ ] AC9：writer/storage/channel、403 和其他不可重试错误不进行网络重试；Pause/Cancel 在 body、barrier、backoff 阶段都能及时结束并保留正确状态。
- [ ] AC10：现有 multi-worker 的 429/503、短 body、lease 回收、Pause/Cancel 和恢复测试全部回归通过，lease/piece/control 完成语义不变。
- [ ] AC11：日志字段能区分“从持久化前缀续传”和“安全地从零重启”，且不会把 `received_in_attempt` 标为 durable progress。
- [ ] AC12：Rust 格式、lint、单元/集成测试通过；中英文文档与实际 equal jitter、重试 scope 和公开参数语义一致。

## Out of Scope

- 复制 aria2 的 command/exception 对象模型。
- 把 `max_retries` 改为总尝试次数，或把 `0` 改为无限重试。
- 用固定 `retry-wait` 替换 bytehaul 的指数退避、`Retry-After` 和时间预算。
- 自动重试 checksum mismatch、写盘失败、控制文件损坏、配置错误等非网络错误。
- 新增多 URI、镜像轮换或最低速度退出策略。
- 修改 Python binding 的配置表面。

## Risks and Deferred Items

- 单连接 writer 的 `written_bytes` 只有在“单 producer + 严格顺序 offset”不变量下才等价于连续前缀；实现与测试必须守住这个前提。
- 从零重启涉及 writer 生命周期、预分配文件、控制快照与 UI 状态的原子性，是最高风险路径，应单独测试 rollback 边界。
- `If-Range` 仅作为可选增强；若强 ETag/Last-Modified 选择规则会扩大接口或兼容性风险，则延后处理，不阻塞本任务的客户端正确性。
