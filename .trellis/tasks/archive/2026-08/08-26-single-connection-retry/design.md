# 单连接响应体自动重试：技术设计

## 1. Architecture Boundaries

### 1.1 Shared retry policy

在 `src/session/retry.rs` 中引入共享状态机（命名可在实现时微调）：

```rust
struct RetryState {
    retries_started: u32,
    started_at: Instant,
}

enum RetryDecision {
    Stop(DownloadError),
    RetryAfter(Duration),
}
```

状态机只做错误分类、次数/时间预算判断和 backoff 计算，不执行 I/O、sleep 或资源清理。为了让 equal jitter 的边界测试稳定，计算接口应允许注入/传入随机样本，生产路径使用 `fastrand`。

调用方职责保持分离：

- `retry_with_backoff`：执行任意请求闭包并做可取消 sleep。
- multi worker：先 discard/renew/reclaim lease，再消费共享决策并做可取消 sleep。
- single runner：先建立 writer barrier/必要的 reset，再消费共享决策并做可取消 sleep。

`RetryState` 的生命周期定义为：fresh probe 一个 scope、resume probe 一个 scope、single transfer 一个 scope、每个 multi segment assignment 一个 scope。

### 1.2 Single orchestration and attempt

将 `run_single_connection` 重构为两层：

```text
run_single_with_retry
  ├─ HttpWorker + first response
  ├─ RetryState + metadata baseline + range capability
  ├─ writer/control/progress runtime
  ├─ stream_single_attempt(response, start_offset, expected_len)
  └─ on failure: barrier → decide retry → backoff → next response/reset
```

`stream_single_attempt` 只消费一个 response，按显式 start offset 发送顺序 `WriterCommand::Data`，并返回：

```rust
enum SingleAttemptOutcome {
    Complete { final_offset: u64, speed_bytes_per_sec: f64 },
    Failed { error: DownloadError, received_in_attempt: u64 },
}
```

具体字段可以为便于 progress/ETA 复用而调整，但 attempt 不得自行发请求、改变 retry budget 或把 received bytes 当作持久化 offset。

## 2. Data Flow and Contracts

### 2.1 Retry from a persisted prefix

```text
body error
  → drop old body
  → enqueue FlushAll(sync_data = true)
  → await FIFO ack
  → persisted_prefix_bytes = ack.written_bytes
  → optionally save control snapshot
  → RetryState::decide(error)
  → cancellable backoff
  → GET Range[persisted_prefix, total - 1]
  → strict range + metadata validation
  → next stream attempt
```

单连接数据严格按 offset 顺序进入 writer，因此 `FlushAll` ack 之后的 high-water mark 是连续前缀。若未来允许乱序单连接写入，则必须先替换为显式 contiguous-prefix tracking，不能继续依赖 high-water mark。

### 2.2 Response validation

续传 response 复用 `validate_range_response(..., RangeValidationMode::ResumeProbe, ...)`，并补充/复用 metadata baseline 比较：

- status 必须为 `206`；
- start = persisted prefix；end = total - 1；total 精确一致；
- 可选 Content-Length = remaining length；
- ETag/Last-Modified 中初始存在的 validator 必须一致；
- attempt 收到的实际字节不能多于 expected length；EOF 时必须恰好相等。

metadata mismatch 是 single orchestration 的“restart from zero”转换条件，不应把 `ResumeMismatch` 全局改成 retryable，也不应改变 multi/resume probe 对该错误的既有分类。

### 2.3 Restart from zero

从零重启使用显式 runtime transition：

1. drop 当前 response；
2. 关闭 writer sender并等待 writer task，传播 writer 错误；
3. 删除/重置旧 control snapshot；
4. 使用 `create_output_file` recreate/truncate（保留当前 allocation 语义）并启动新 writer；
5. 重置 durable/received offset、progress reporter、ETA estimator、control tracker；
6. 复用安全的完整 `200` response，或通过 `HttpWorker::send_get` 获取完整 response；
7. 从该 response 建立新的 total/ETag/Last-Modified baseline。

该 transition 不创建新 `RetryState`。如果 Range 被明确忽略，可在本 transfer scope 内记住“不支持续传”，后续 body 错误直接完整 GET，避免重复无效 Range；无论是否做此优化，都必须先 reset 才能消费 `200`。

未知总大小时没有可证明的非零 Range end；body 出错后直接进入同一从零重启路径。正常 EOF 仍是未知长度响应的完成条件。

## 3. Progress, Control, and Stop Semantics

- UI 的 attempt-local received progress 可以前进，但 retry barrier 后必须校正为 persisted prefix；从零重启必须校正为 0 并清空 ETA samples。
- control snapshot 只保存 flush barrier 返回的 prefix；unknown total 不启用现有 single control snapshot。
- Pause/Cancel：
  - body 阶段由 attempt 的 `select!` 捕获；
  - barrier 阶段在 writer ack 前后检查 signal，不能遗留未 join writer；
  - backoff 使用现有可取消 select；
  - 终止前尽力保存已确认的 control snapshot，但 writer/storage 错误本身必须向上传播。
- writer/channel/storage 错误不传给 `RetryState` 作为网络重试候选。

## 4. Compatibility

- `DownloadSpec` 字段及默认值不变；Python binding 不增加参数。
- `DownloadError` 公开变体不要求变化；内部 outcome/transition 类型保持 crate-private。
- `max_retries`、`RetryBudgetExceeded`、Retry-After 秒数解析保持兼容。
- multi worker 的 lease 清理顺序必须保持：失败接收量回退 → discard lease → retry decision → renew/reclaim → sleep/return。若共享状态机接入导致顺序变化，测试必须证明没有 stale lease 或进度泄漏。

## 5. Observability

single retry/restart 日志使用结构化字段：

- `attempt`、`max_retries`、`elapsed_ms`；
- `error`、`received_in_attempt`；
- `resume_offset`、`backoff_ms`；
- `restart_from_zero`、`restart_reason`。

日志中的 `received_in_attempt` 不命名为 downloaded/persisted/durable。正常完成、从前缀续传、Range ignored、metadata mismatch、unknown length restart 应可区分。

## 6. Rollout and Rollback

- 按 M1→M2→M3→M4 顺序提交/验证，每一阶段保持可编译和已有测试通过。
- M1 的共享策略重构若造成 multi 行为变化，可先回退 multi 接入而保留纯状态机与 helper 接入。
- M2/M3 的 single runner 应保留清晰的 attempt/orchestration 边界；若 reset 路径失败，安全失败并保留原始错误/控制状态，不得继续追加。
- 无配置迁移、控制文件版本迁移或数据迁移。

