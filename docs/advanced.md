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

`max_retries` still controls how many retry attempts are allowed. `max_retry_elapsed` adds a separate time budget. If the retry loop would exceed that budget, the request stops with `DownloadError::RetryBudgetExceeded` instead of continuing until the retry count is exhausted.

If you omit `.output_path(...)`, bytehaul will detect the filename from `Content-Disposition`, then the URL path, then `download`. Absolute output paths are still accepted when `.output_dir(...)` is not set.

## Network Settings

Network stack settings live on the shared downloader client:

```rust
use std::net::SocketAddr;
use bytehaul::Downloader;

let downloader = Downloader::builder()
    .all_proxy("http://127.0.0.1:7890")
    .dns_servers([
        SocketAddr::from(([1, 1, 1, 1], 53)),
        SocketAddr::from(([8, 8, 8, 8], 53)),
    ])
    .doh_server("https://dns.google/dns-query")
    .enable_ipv6(false)
    .build()?;
```

`DownloadSpec::connect_timeout` is still supported. If a task overrides it, bytehaul builds an equivalent client just for that download.

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

`speed_bytes_per_sec` and `eta_secs` are derived from the same recent throughput window:

- `speed_bytes_per_sec` is a recent-window rate, not a whole-download lifetime average.
- `eta_secs` divides remaining bytes by that same recent-window rate, so it rises and falls with the displayed speed instead of using a different smoothing rule.
- `eta_secs == None` means bytehaul does not have enough recent samples yet, or the total size is still unknown.
- `eta_secs == Some(0.0)` means the task has reached the end of the stream and the progress state is transitioning to `Completed`.

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

Pause is not an in-place suspension of the same handle. It ends the current task after flushing writer state and saving a control file. Resuming means starting a new `download(spec)` call against the same resolved output path.

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

`Cancelled`, `Paused`, and `Completed` are distinct end states. `cancel()` abandons the task. `pause()` preserves resumable state. A normally completed download removes its control file and returns `Ok(())`.
