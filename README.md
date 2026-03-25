# bytehaul

A Rust async HTTP download library with resume, multi-connection, write-back cache, rate limiting, and checksum verification.

## Documentation

- [简体中文 README](docs/README.zh-CN.md)
- [Advanced Usage (Rust)](docs/advanced.md) | [进阶用法](docs/advanced.zh-CN.md)
- [Python Bindings Guide](bindings/python/README.md) | [Python 使用文档](docs/python.zh-CN.md)

## Features

- **Single & multi-connection downloads** — automatic Range probing and fallback
- **Pause / resume** — cooperative pause with persisted control files for later continuation
- **Write-back cache** — piece-based aggregation to reduce random I/O
- **Memory budget & backpressure** — semaphore-based flow control
- **Retry with exponential backoff** — configurable max retries, respects `Retry-After`
- **Rate limiting** — shared token-bucket across all workers
- **SHA-256 checksum verification** — post-download integrity check
- **Cancellation** — cooperative cancel via stop signal
- **Progress reporting** — real-time speed, ETA, downloaded bytes, and state
- **Shared network configuration** — proxy, custom DNS servers, and IPv6 toggle on the downloader client

## Installation

### Rust

Add `bytehaul` to your project via Cargo:

```bash
cargo add bytehaul
```

Or add it manually to your `Cargo.toml`:

```toml
[dependencies]
bytehaul = "0.1.2"
```

### Python

```bash
pip install bytehaul
```

Requires Python 3.9+. A single wheel per platform covers all supported Python versions (abi3).

## Quick Start (Rust)

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

For configuration, progress monitoring, cancellation, and more, see the [Advanced Usage Guide](docs/advanced.md).

## Quick Start (Python)

```python
import bytehaul

# Simple one-line download
bytehaul.download("https://example.com/file.bin", "output.bin")

# With options
bytehaul.download(
    "https://example.com/file.bin",
    "output.bin",
    max_connections=8,
    max_download_speed=1_000_000,  # 1 MB/s
)
```

For the full Python API (object API, progress, cancellation, error handling, etc.), see the [Python Bindings Guide](bindings/python/README.md).

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
