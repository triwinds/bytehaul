# bytehaul — Advanced Usage (Rust)

This guide covers advanced configuration, progress monitoring, cancellation, and network settings for the Rust API.

For basic usage, see the [main README](../README.md).
[中文版](advanced.zh-CN.md)

## Configuration

```rust
use std::time::Duration;
use bytehaul::{Checksum, DownloadSpec, FileAllocation};

let mut spec = DownloadSpec::new("https://example.com/file.bin", "file.bin");
spec.max_connections = 8;               // parallel workers
spec.piece_size = 2 * 1024 * 1024;      // 2 MiB pieces
spec.min_split_size = 10 * 1024 * 1024;  // split only if > 10 MiB
spec.file_allocation = FileAllocation::Prealloc;
spec.resume = true;                      // enable breakpoint resume
spec.max_retries = 5;
spec.retry_base_delay = Duration::from_secs(1);
spec.max_download_speed = 1024 * 1024;   // 1 MB/s limit
spec.checksum = Some(Checksum::Sha256(
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
));
```

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
    .enable_ipv6(false)
    .build()?;
```

`DownloadSpec::connect_timeout` is still supported. If a task overrides it, bytehaul builds an equivalent client just for that download.

## Progress Monitoring

```rust
use bytehaul::{DownloadSpec, DownloadState, Downloader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dl = Downloader::builder().build()?;
    let handle = dl.download(DownloadSpec::new(
        "https://example.com/file.bin", "file.bin"
    ));

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
