# Errors and Observability

## Typed Errors

`src/error.rs` is the source of truth. `DownloadError` separates transport, I/O, lifecycle, HTTP, configuration, resume/control-file, retry-budget, task, internal, and checksum failures. `TransportError` preserves its boxed source and classifies connect, timeout, request, body, and other failures.

Use `?`, `#[from]`, or narrow `map_err` calls that add domain meaning:

```rust
let n = file.read(&mut buf).await.map_err(DownloadError::Io)?;
```

Keep the original source when it affects retry or diagnosis. Use a string variant only where no useful typed source exists, as at the control-file serialization boundary.

## Retry Semantics

`DownloadError::is_retryable` centralizes classification. Connect/timeout/request/body transport failures, selected I/O failures, and HTTP 429/500/502/503/504 retry; cancellation, pause, bad config, and ordinary client errors do not. `retry_after_secs` carries 429/503 hints into `src/session/retry.rs`.

For new variants or transport parsing, decide retryability in `src/error.rs`, verify single and multi paths, preserve stop/budget short circuits, and assert exact variants/statuses. Do not turn retryable range-probe failures into unrelated plain GETs; `should_abort_range_probe_fallback` and `tests/m5_retry.rs::test_fresh_probe_503_does_not_fallback_to_plain_get` protect this.

## Single-transfer retry contract

### 1. Scope / Trigger

This contract applies when a single-connection download has already received a
valid response and then fails while consuming its body. The transfer-level
retry scope covers the body attempt, subsequent Range request/validation, and
any safe restart from zero; the initial fresh/resume probe keeps its existing
independent request scope.

### 2. Signatures

- `RetryState::new(max_retries, base_delay, max_delay, max_retry_elapsed)` owns
  one transfer's retry count and elapsed budget.
- `stream_single_attempt(response, start_offset, expected_total)` returns
  `SingleAttemptOutcome::{Complete, Failed { error, received_in_attempt }}`.
- `run_single_with_retry(worker, response, meta, ..., start_offset, ...)` owns
  the writer, metadata baseline, control snapshot, and transfer state.

### 3. Contracts

- Every storage failure during writer start, flush, reset or close must publish
  `Failed` before returning the original typed error. A body that reached EOF
  is not complete until the writer flush and final close both succeed.
- Finalization publishes the known received prefix before awaiting storage. A
  throttled progress update must not leave downloaded bytes stale on failure;
  this received counter still must not be used as a durable checkpoint offset.
- Failed flush/sync must leave the previous durable control file unchanged;
  never create a terminal checkpoint at the received-byte offset.
- A body failure first drops the response and awaits `FlushAll(sync_data =
  true)`; only the acknowledgement's `written_bytes` is a durable resume
  offset.
- A known-size continuation requests `Range: bytes=<prefix>-<total - 1>` and
  requires strict `ResumeProbe` validation plus matching initial ETag,
  Last-Modified, and total metadata.
- A Range `200`, validator/total mismatch, or unknown-size body failure resets
  the output and control snapshot before consuming a full GET. The same
  `RetryState` remains in force.

### 4. Validation & Error Matrix

| Condition | Result |
| --- | --- |
| Retryable body/request transport error | Flush barrier, then `RetryState::decide` and cancellable backoff |
| Known-size short or over-read body | Retryable body transport error; never mark complete |
| Range `206`/metadata/content-length mismatch | Safe restart decision; never append the response |
| Range `200` | Reset first; consume it only as a zero-offset full response |
| Writer/storage/channel error, 403, pause, or cancel | Close/join writer and return immediately; no network retry |
| Exhausted count or elapsed budget | Return original error or `RetryBudgetExceeded`, preserving policy order |

### 5. Good / Base / Bad Cases

- Good: a truncated 200 body flushes four durable bytes, then a matching 206
  starts at byte 4 and produces the exact file.
- Base: an unknown-length body fails and the next full GET starts from zero;
  no control snapshot records the unproven prefix.
- Bad: writing a retry response at the network-received offset, or consuming a
  Range-ignored 200 before truncating, would duplicate bytes or concatenate
  different objects.

### 6. Tests Required

- `tests/m5_retry.rs` must assert exact Range start/end, request count, final
  bytes, and reset behavior for truncation, Range ignored, and metadata change.
- Retry unit tests must cover zero retries, count exhaustion, Retry-After,
  elapsed limits, and pause/cancel during backoff.
- Portable writer-failure regressions must drive production finalization and
  assert the typed error, `Failed` progress, and unchanged previous checkpoint.
  Retain the Linux `/dev/full` integration with the same durability semantics;
  macOS/Windows success cannot replace exercising Linux-only code.
- Existing multi-worker short-body, lease, pause/cancel, and resume tests must
  remain green because each segment retains its own `RetryState`.

### 7. Wrong vs Correct

#### Wrong

```rust
let next_start = received_in_attempt;
worker.send_range(next_start, total - 1).await?;
```

`received_in_attempt` may still be buffered and is not a durable prefix.

#### Correct

```rust
let stats = writer.flush().await?;
let next_start = stats.written_bytes;
worker.send_range(next_start, total - 1).await?;
```

The writer acknowledgement establishes the FIFO durability barrier before the
next response is requested.

## Python Mapping

`bindings/python/src/lib.rs::map_download_error` is the single translation point: lifecycle/config/resume/internal variants map to specific Python exceptions; remaining failures become `DownloadFailedError`. All inherit `BytehaulError`. Match enum variants, not error strings, and update registration, intended facade exports, Rust binding tests, Python instance tests, and docs together.

## Logging

Task-visible structured `tracing` events use the `log_error!` through `log_trace!` macros in `src/logging.rs`, gated by `LogLevel`. Attach `download_id` and structured fields such as strategy, path, attempt, piece/segment, bytes, and elapsed time. Log decisions/lifecycle boundaries, not every chunk. Never log headers, credentials, or full proxy secrets.

The Rust library does not install a subscriber. The Python adapter deliberately uses `Once` in `init_tracing` when non-off logging is requested; preserve one-time initialization. Direct `tracing::debug!` is reserved for internal diagnostics such as checksum/control-file I/O, not as a bypass for task-level policy.

## Avoid

- Do not `unwrap`/`expect` network, filesystem, channel, or user parsing paths; tests and proven invariants are different.
- Do not log and discard failures that should affect terminal progress or `wait()`.
- Do not retry every failure or flatten errors to `Internal(String)` at each layer.
- Do not expose raw internal text as Python's only type signal.
