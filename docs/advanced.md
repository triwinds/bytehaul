# bytehaul — Advanced Usage (Rust)

This guide covers advanced configuration, progress monitoring, cancellation, and network settings for the Rust API.

For basic usage, see the [main README](../README.md).
[中文版](advanced.zh-CN.md)

## Configuration

```rust
use std::time::Duration;
use bytehaul::{Checksum, DownloadSpec, FileAllocation};

let spec = DownloadSpec::new("https://example.com/file.bin")
    .output_dir("downloads")
    .output_path("file.bin")
    .max_connections(8) // parallel workers
    .piece_size(2 * 1024 * 1024) // 2 MiB pieces
    .min_split_size(10 * 1024 * 1024) // split only if > 10 MiB
    .file_allocation(FileAllocation::Prealloc)
    .resume(true)
    .retry_policy(5, Duration::from_secs(1), Duration::from_secs(30))
    .max_retry_elapsed(Duration::from_secs(120)) // stop retrying after 2 minutes total
    .max_download_speed(1024 * 1024) // 1 MB/s limit
    .checksum(Checksum::Sha256(
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
    ));
```

bytehaul now validates these task-level settings through `DownloadSpec::validate()` before network work starts, so invalid combinations fail consistently instead of relying on scattered runtime checks.

`max_retries` controls the additional retries allowed after the initial request/transfer attempt. `max_retry_elapsed` adds a separate time budget. In single-connection mode, body failures stay in one transfer retry scope and resume from the contiguous prefix confirmed by the writer flush barrier; later Range connection and validation failures consume the same budget. If the retry loop would exceed that budget, the request stops with `DownloadError::RetryBudgetExceeded` instead of continuing until the retry count is exhausted.

If you omit `.output_path(...)`, bytehaul will detect the filename from `Content-Disposition`, then the URL path, then `download`. Absolute output paths are still accepted when `.output_dir(...)` is not set.

## Slow-transfer recovery

This feature is available starting with version 0.2.2.

Multi-connection Range downloads use `SlowTransferMode::Adaptive` by default: sustained slow requests can be cancelled and their segments reassigned, including near completion. Reading speed excludes intentional rate-limit and local forwarding waits. Short fluctuations and requests about to finish do not automatically trigger recovery. Use `Disabled` to keep the previous scheduling behavior.

```rust
use bytehaul::{DownloadSpec, SlowTransferMode};
use std::time::Duration;

let spec = DownloadSpec::new("https://example.com/file.bin")
    .slow_transfer_mode(SlowTransferMode::AdaptiveWithHedging)
    .low_speed_duration(Duration::from_secs(15))
    .slow_start_grace(Duration::from_secs(5))
    .slow_sample_window(Duration::from_secs(5));
```

`low_speed_limit(bytes_per_second)` optionally sets a positive absolute threshold; the default uses a healthy-request baseline without an absolute floor. The three durations must be positive and at most 86,400 seconds. Defaults are 15 seconds below the threshold, 5 seconds of startup grace and a 5-second sample window.

In the current development version, a trailing range of at most 1 MiB can recover earlier when no unassigned work remains and a request slot is idle. A separate detector caps the configured sample window, grace and sustained duration at 1, 1 and 2 seconds respectively. It requires recent healthy reference speeds and a worthwhile estimated time saving; an absolute speed floor alone is insufficient. Missing or immature live-peer evidence, collective slowdown and local backpressure prevent acceleration. Ordinary detection retains the configured durations. This applies to both adaptive modes; `Disabled` disables it.

Hedging is opt-in. It uses at most one spare request for a small trailing segment, requires a strong ETag and compatible conditional headers, and stages the spare response separately before switching the writer. The original and spare requests together remain within `max_connections`; all network payload shares `max_download_speed`. Automatic recovery and hedging share an extra-work budget of `min(total_size / 100, 16 MiB)`. A range that cannot fit is skipped, so small downloads may receive no automatic retry for performance. Ordinary error retries retain their existing separate retry policy.

Progress counts effective download bytes, not duplicate traffic; it can decrease when an incomplete attempt is reclaimed. Enable debug logging to inspect recovery decisions. These policies do not increase a shared server or disk bandwidth limit. Single-connection and non-Range fallback behavior is unchanged.

When `max_download_speed` is nonzero, automatic slow-request recovery and hedging are suppressed to avoid treating intentional rate limiting as a network fault. Ordinary timeouts and error retries still apply.

## Network Settings

Network stack defaults live on the shared downloader client. DNS / DoH / IPv6 settings are configured on `Downloader::builder()`, and each download can still override `connect_timeout` and proxy settings on `DownloadSpec`:

```rust
use std::net::SocketAddr;
use std::time::Duration;
use bytehaul::{DownloadSpec, Downloader};

let downloader = Downloader::builder()
    .dns_servers([
        SocketAddr::from(([1, 1, 1, 1], 53)),
        SocketAddr::from(([8, 8, 8, 8], 53)),
    ])
    .doh_server("https://dns.google/dns-query")
    .enable_ipv6(false)
    .build()?;

let spec = DownloadSpec::new("https://example.com/file.bin")
    .output_path("file.bin")
    .all_proxy("http://127.0.0.1:7890")
    .connect_timeout(Duration::from_secs(10));

let handle = downloader.download(spec);
```

Builder-level `all_proxy(...)`, `http_proxy(...)`, and `https_proxy(...)` are still useful as defaults. If a task sets `DownloadSpec::all_proxy(...)`, `http_proxy(...)`, `https_proxy(...)`, or `connect_timeout(...)`, bytehaul derives an equivalent client for that effective configuration and reuses it for later downloads with the same settings.

`doh_server(...)` and `doh_servers(...)` accept HTTPS URLs. When the DoH host is a domain name instead of a literal IP, bytehaul resolves that host once with the system resolver during client construction so it can bootstrap the DoH connection.

## Progress Monitoring

```rust
use bytehaul::{DownloadSpec, DownloadState, Downloader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dl = Downloader::builder().build()?;
    let handle = dl.download(
        DownloadSpec::new("https://example.com/file.bin").output_path("file.bin")
    );

    let mut rx = handle.subscribe_progress();
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            let snap = rx.borrow().clone();
            println!(
                "state={:?} downloaded={} speed={:.0} B/s eta={:?}",
                snap.state,
                snap.downloaded,
                snap.speed_bytes_per_sec,
                snap.eta_secs
            );
        }
    });

    handle.wait().await?;
    Ok(())
}
```

`downloaded` is a UI-oriented count of received bytes, not the durable resume offset in the control file. Retries may move it backward; even when it equals the total size, a final write or synchronization failure can leave the task `Failed`. Use the result of `handle.wait().await` to determine success.

`speed_bytes_per_sec` and `eta_secs` are derived from the same recent throughput window:

- `speed_bytes_per_sec` is a recent-window rate, not a whole-download lifetime average.
- `eta_secs` divides remaining bytes by that same recent-window rate, so it rises and falls with the displayed speed instead of using a different smoothing rule.
- `eta_secs == None` means bytehaul does not have enough recent samples yet, or the total size is still unknown.
- `eta_secs == Some(0.0)` means the current byte count leaves no estimated transfer time; it does not independently prove that writing, synchronization or final verification succeeded.

## Pause And Resume

```rust
let handle = downloader.download(spec.clone());
handle.pause();

match handle.wait().await {
    Err(bytehaul::DownloadError::Paused) => {
        let resumed = downloader.download(spec);
        resumed.wait().await?;
    }
    other => other?,
}
```

Pause is not an in-place suspension of the same handle. It ends the current task, flushing writer state and saving a control file when resume is enabled and storage succeeds. Resuming means starting a new `download(spec)` call against the same resolved output path.

Resume safety has two checks before bytehaul trusts the saved state:

- Remote metadata still has to match the saved snapshot.
- The local output file must still be consistent with the saved progress snapshot.

If either check fails, bytehaul discards the stale control file and restarts the download from scratch.

## Logging

Bytehaul uses `tracing` internally. Logging is **off by default** — opt in by setting the log level on the builder:

```rust
use bytehaul::{Downloader, LogLevel};

let downloader = Downloader::builder()
    .log_level(LogLevel::Debug)
    .build()?;
```

Available levels (from least to most verbose): `Off` (default), `Error`, `Warn`, `Info`, `Debug`, `Trace`.

To see logs you also need a `tracing-subscriber` in your application:

```rust
tracing_subscriber::fmt::init();

let downloader = Downloader::builder()
    .log_level(LogLevel::Info)
    .build()?;
```

## Cancellation

```rust
let handle = downloader.download(spec);
// Cancel from another task or after a timeout
handle.cancel();
let result = handle.wait().await; // returns Err(DownloadError::Cancelled)
```

`Cancelled`, `Paused`, and `Completed` are distinct end states. Both `cancel()` and `pause()` end the current task and attempt to preserve resumable state when resume is enabled. A write or synchronization failure leaves the previous durable checkpoint in place. Normal completion attempts to remove the control file; single-connection cleanup failure is an error, while multi-connection cleanup is best effort.
