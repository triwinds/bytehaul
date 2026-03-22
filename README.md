# bytehaul

A Rust async HTTP download library with resume, multi-connection, write-back cache, rate limiting, and checksum verification.

## Documentation

- [简体中文 README](docs/README.zh-CN.md)
- [Python bindings guide](bindings/python/README.md)

## Features

- **Single & multi-connection downloads** — automatic Range probing and fallback
- **Resume / breakpoint continuation** — control file persistence with atomic save
- **Write-back cache** — piece-based aggregation to reduce random I/O
- **Memory budget & backpressure** — semaphore-based flow control
- **Retry with exponential backoff** — configurable max retries, respects `Retry-After`
- **Rate limiting** — shared token-bucket across all workers
- **SHA-256 checksum verification** — post-download integrity check
- **Cancellation** — cooperative cancel via watch channel
- **Progress reporting** — real-time speed, downloaded bytes, state

- **Shared network configuration** - proxy, custom DNS servers, and IPv6 toggle on the downloader client

## Quick Start

```rust
use bytehaul::{DownloadSpec, Downloader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let downloader = Downloader::builder().build()?;

    let spec = DownloadSpec::new(
        "https://example.com/largefile.zip",
        "largefile.zip",
    );

    let handle = downloader.download(spec);
    handle.wait().await?;
    println!("Download complete!");
    Ok(())
}
```

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
                "state={:?} downloaded={} speed={:.0} B/s",
                snap.state, snap.downloaded, snap.speed_bytes_per_sec
            );
        }
    });

    handle.wait().await?;
    Ok(())
}
```

## Cancellation

```rust
let handle = downloader.download(spec);
// Cancel from another task or after a timeout
handle.cancel();
let result = handle.wait().await; // returns Err(DownloadError::Cancelled)
```

## Architecture

```
DownloadManager
  └─ DownloadSession
       ├─ Scheduler (piece assignment, segment reclamation)
       ├─ HttpWorker ×N (Range requests, retry)
       │    └─ channel ─→ Writer (WriteBackCache → FileWriter)
       └─ ControlStore (atomic save/load/delete)
```

## License

MIT. See [LICENSE](LICENSE).
