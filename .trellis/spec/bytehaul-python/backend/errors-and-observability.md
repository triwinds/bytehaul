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
