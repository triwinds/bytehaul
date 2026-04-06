# bytehaul

[![Tests](https://img.shields.io/github/actions/workflow/status/triwinds/bytehaul/test.yml?branch=main&logo=githubactions&label=tests)](https://github.com/triwinds/bytehaul/actions/workflows/test.yml)
[![Crates.io](https://img.shields.io/crates/v/bytehaul?logo=rust)](https://crates.io/crates/bytehaul)
[![Docs.rs](https://img.shields.io/docsrs/bytehaul?logo=docs.rs)](https://docs.rs/bytehaul)
[![PyPI](https://img.shields.io/pypi/v/bytehaul?logo=pypi)](https://pypi.org/project/bytehaul/)
[![Python](https://img.shields.io/pypi/pyversions/bytehaul?logo=python&logoColor=white)](https://pypi.org/project/bytehaul/)
[![License](https://img.shields.io/github/license/triwinds/bytehaul)](LICENSE)

A Rust async HTTP download library with Python bindings (also available on PyPI), supporting resume, multi-connection downloads, write-back cache, rate limiting, and checksum verification.

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
- **Shared network configuration** — proxy, custom DNS servers, DNS-over-HTTPS endpoints, and IPv6 toggle on the downloader client

## Installation

### Rust

Add `bytehaul` to your project via Cargo:

```bash
cargo add bytehaul
```

Or add it manually to your `Cargo.toml`:

```toml
[dependencies]
bytehaul = "0.1.6"
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

    let spec = DownloadSpec::new("https://example.com/largefile.zip")
        .output_path("largefile.zip");

    let handle = downloader.download(spec);
    handle.wait().await?;
    println!("Download complete!");
    Ok(())
}
```

For configuration, progress monitoring, cancellation, and more, see the [Advanced Usage Guide](docs/advanced.md).

If you omit `output_path`, bytehaul will automatically choose a filename from `Content-Disposition`, then the URL path, and finally `download`. You can combine that with `.output_dir("downloads")` to control the destination directory. Absolute `output_path` values are still accepted when `output_dir` is not set.

## Quick Start (Python)

```python
import bytehaul

# Simple one-line download
bytehaul.download("https://example.com/file.bin", output_path="output.bin")

# Automatic filename detection into a directory
bytehaul.download("https://example.com/file.bin", output_dir="downloads")

# With options
bytehaul.download(
    "https://example.com/file.bin",
    output_path="output.bin",
    max_connections=8,
    max_download_speed=1_000_000,  # 1 MB/s
)
```

For the full Python API (object API, progress, cancellation, error handling, etc.), see the [Python Bindings Guide](bindings/python/README.md).

## Coverage

The CI coverage gate is still enforced on Ubuntu with Tarpaulin:

```bash
cargo tarpaulin --engine llvm --workspace --all-targets --out Stdout --fail-under 95
```

On Windows, Tarpaulin can leave locked binaries or produce incomplete summaries after interrupted or parallel coverage runs. Use the PowerShell helper instead for a local report:

```powershell
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov
powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope all-targets -Format html
```

For a machine-readable summary on Windows:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope all-targets -Format json
```

The helper now defaults to the same `all-targets` scope as the CI gate, while still forcing `CARGO_BUILD_JOBS=1` and a fresh isolated `CARGO_TARGET_DIR` on each run to avoid the `os error 5`, `LNK1104`, and stale `cargo` / `rustc` handle conflicts that show up when multiple coverage builds overlap on Windows.

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
