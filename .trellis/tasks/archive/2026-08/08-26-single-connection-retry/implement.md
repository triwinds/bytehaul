# 单连接响应体自动重试：实施计划

## M1 — 共享重试决策与策略校准

- [x] 在 `src/session/retry.rs` 提取 `RetryState`/`RetryDecision`（或等价内部类型），支持 equal jitter 范围、Retry-After、次数和时间预算边界测试。
- [x] 让 `retry_with_backoff` 消费共享决策，保持调用接口或最小化调用点改动。
- [x] 让 `src/session/multi.rs` 的 segment loop 消费共享决策，同时保持 received bytes 回退、DiscardLease、renew/reclaim 和可取消 sleep 的现有顺序。
- [x] 增加 `max_retries = 0`、1+N 次数、不可重试、Retry-After、elapsed 已耗尽、elapsed+backoff 超限、Cancel/Pause 的单元测试。
- [x] 更新中英文 architecture/tuning 中错误的 “full jitter/完整抖动” 表述为 equal jitter，并核对 `max_retries` 文案。

验证：

```bash
cargo test --lib session::retry
cargo test --lib session::multi
cargo test --test m5_retry
```

Rollback point：共享状态机接入后、single runner 改造前，确保现有功能行为等价。

## M2 — 单连接完整 transfer attempt

- [x] 调整 single runner 的调用边界，使其获得 `HttpWorker`/请求能力并复用 fresh/resume 已取得的 attempt-0 response。
- [x] 将 `stream_single` 拆为单 response attempt，返回完成或带 `received_in_attempt` 的失败结果。
- [x] 在已知 total 时拒绝 body 多读和 EOF 短读；正常完成必须精确到 `total_size`。
- [x] body 可重试失败后 drop response，执行 `FlushAll(sync_data = true)` barrier，以 ack 的 `written_bytes` 作为唯一 resume offset。
- [x] 使用 `send_range(prefix, total - 1)` 创建下一 attempt，并复用 `ResumeProbe` validator 与 metadata baseline 校验。
- [x] single-transfer 的 body、后续建连、校验失败共享一个 retry scope；Pause/Cancel 在 body、barrier/backoff 中均可终止。
- [x] 增加本地 TCP/warp integration tests，记录请求次数和 Range header，覆盖截断→Range 恢复、writer 缓冲边界、未知长度重启、403、writer/channel/storage 错误。

验证：

```bash
cargo test --lib session::single
cargo test --test m5_retry
cargo test --test m8_pause_resume
```

Rollback point：新的 single runner 必须在 `max_retries = 0` 时等价于旧单响应路径，便于隔离重试编排问题。

## M3 — 安全 fallback 与远端变化

- [x] 为 Range `200`、metadata/total mismatch、unknown total body failure 建立显式 `RestartFromZero` transition；不要改变这些错误的全局 retryable 分类。
- [x] 实现 writer close/join、输出 recreate/truncate、控制快照删除/重置、progress/ETA/control tracker 清零和新 baseline 建立。
- [x] Range ignored 的完整 `200` response 在 reset 完成后安全复用；其他不匹配响应重新完整 GET。
- [x] 保证 restart transition 继续使用同一个 `RetryState`，持续变化最终受次数或时间预算限制。
- [x] 评估 `If-Range`：不加入公开 API 或替代客户端校验，记录为后续项。
- [x] 增加 Range ignored、ETag/Last-Modified/total 改变、unknown/chunked 重启、进度回退和控制快照正确性的测试。

验证：

```bash
cargo test --lib session::single
cargo test --test m2_resume
cargo test --test m5_retry
cargo test --test m8_pause_resume
```

Rollback point：任何 reset 失败都必须安全终止；不得退化为在非零 offset 消费完整响应。

## M4 — 回归、文档与接口收口

- [x] 清理只包住 response acquisition、会与 single-transfer 重复计数的调用方式，保留 probe scope 与 transfer scope 的明确边界。
- [x] 回归 multi worker 的 429/503、短/多 body、lease 回收、Pause/Cancel 与恢复行为。
- [x] 更新 `docs/architecture*.md`、`docs/tuning*.md`、`docs/advanced*.md`、`docs/troubleshooting*.md`、Rust README/API 说明和 Python 文档/README；明确 body retry、equal jitter、额外重试次数语义。
- [x] 核对 Python binding 无需新增参数，内部错误/outcome 类型没有公开 API 泄漏。
- [x] 核对日志字段和测试名称不再暗示 body retry 只属于 multi-worker。

最终验证：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
```

若 Python 测试依赖可用，再执行：

```bash
cd bindings/python && uv run pytest
```

## Pre-start Review Gates

- [x] 需求来源、正确性约束、非目标和验收矩阵已写入 PRD。
- [x] 已确认当前代码锚点与历史讨论，没有待用户决定的产品问题。
- [x] 设计明确 retry scope、writer barrier、metadata validation、reset 和兼容性边界。
- [x] 计划按 M1→M4 排序，每阶段有验证命令和 rollback point。
- [x] 用户审阅并明确批准本计划后，运行 `task.py start` 进入实现。
