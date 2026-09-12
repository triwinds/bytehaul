# bytehaul

[![Tests](https://img.shields.io/github/actions/workflow/status/triwinds/bytehaul/test.yml?branch=master&logo=githubactions&label=tests)](https://github.com/triwinds/bytehaul/actions/workflows/test.yml)
[![Crates.io](https://img.shields.io/crates/v/bytehaul?logo=rust)](https://crates.io/crates/bytehaul)
[![Docs.rs](https://img.shields.io/docsrs/bytehaul?logo=docs.rs)](https://docs.rs/bytehaul)
[![PyPI](https://img.shields.io/pypi/v/bytehaul?logo=pypi)](https://pypi.org/project/bytehaul/)
[![Python](https://img.shields.io/pypi/pyversions/bytehaul?logo=python&logoColor=white)](https://pypi.org/project/bytehaul/)
[![License](https://img.shields.io/github/license/triwinds/bytehaul)](LICENSE)

A Rust async HTTP download library with Python bindings (also available on PyPI), supporting resume, multi-connection downloads, write-back cache, rate limiting, and checksum verification.

These examples target **0.2.4**, which improves HTTP connection reuse, contiguous Range requests, cancellation, response-header deadlines, and slow-tail recovery while preserving existing resume-file compatibility. See the [0.2.4 release notes](https://github.com/triwinds/bytehaul/releases/tag/v0.2.4).

Version 0.2.4 includes default HTTP pooling and request batching, cooperative cancellation, per-request response-header deadlines, confirmed-prefix reuse, and bounded slow-tail recovery; see [advanced usage](docs/advanced.md).

## Documentation

- [简体中文 README](docs/README.zh-CN.md)
- [Advanced Usage (Rust)](docs/advanced.md) | [进阶用法](docs/advanced.zh-CN.md)
- [Python Bindings Guide](bindings/python/README.md) | [Python 使用文档](docs/python.zh-CN.md)

## Features

- **Single & multi-connection downloads** — automatic Range probing and fallback
- **Pause / resume** — cooperative pause with persisted control files for later continuation
- **Write-back cache** — piece-based aggregation to reduce random I/O
- **Memory budget & backpressure** — semaphore-based flow control
- **Retry with exponential backoff** — shared by single/multi transfers, resumes body failures safely, respects `Retry-After`
- **Rate limiting** — shared token-bucket across all workers
- **SHA-256 checksum verification** — post-download integrity check
- **Cancellation** — cooperative cancel via stop signal
- **Progress reporting** — real-time speed, ETA, downloaded bytes, and state
- **Shared network configuration** — proxy, custom DNS servers, DNS-over-HTTPS endpoints, and IPv6 toggle on the downloader client

## Installation

### Rust

Add `bytehaul` to your project via Cargo:

```bash
cargo add bytehaul@0.2.4
```

Or add it manually to your `Cargo.toml`:

```toml
[dependencies]
bytehaul = "0.2.4"
```

### Python

```bash
pip install "bytehaul==0.2.4"
```

Requires Python 3.9+. A single wheel per platform covers all supported Python versions (abi3).

## Quick Start (Rust)

```rust
use bytehaul::{DownloadSpec, Downloader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let downloader = Downloader::builder().build()?;

    let spec = DownloadSpec::new("https://example.com/largefile.zip")
        .output_path("largefile.zip")
        .all_proxy("http://127.0.0.1:7890");

    let handle = downloader.download(spec);
    handle.wait().await?;
    println!("Download complete!");
    Ok(())
}
```

Configure downloader-wide defaults on `Downloader::builder()`, and put per-download overrides such as proxies on `DownloadSpec` when you submit a task. For configuration, progress monitoring, cancellation, and more, see the [Advanced Usage Guide](docs/advanced.md).

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

CI and local Linux validation use one entry point. On Ubuntu 24.04 x86_64 (with Python 3, rustup, a C compiler, pkg-config and OpenSSL development headers installed), run:

```bash
python3 scripts/coverage.py --install
```

`--install` installs the Rust and Tarpaulin versions pinned in [coverage-config.json](scripts/coverage-config.json). After setup, omit it to reuse those exact versions. The gate uses LLVM, `-p bytehaul --all-targets`, the locked dependencies and a **95% line threshold**, without extra source exclusions. It ignores ambient Tarpaulin config and isolates proxy environment variables for localhost tests.

Use a writable source checkout: LLVM-instrumented build scripts can write profile files there even when the build directory is elsewhere. On a Linux VM with limited memory, reduce concurrent compiler processes. Linking can still exceed a 2 GB VM; use more memory if the linker is killed:

```bash
CARGO_BUILD_JOBS=1 python3 scripts/coverage.py --install
```

This changes build concurrency only; the target scope and threshold remain the same. Metadata includes the Linux distribution and the explicit `CARGO_BUILD_JOBS` value, if set.

Every run gets separate build and report directories under `target/coverage/`. JSON, HTML, the raw log, tool versions, revision and dirty-worktree status are retained under `target/coverage/reports/<run>/`. A failed test is reported as incomplete coverage; a measured percentage below 95% is reported as a threshold failure. Both fail the command. Actions also publishes the summary and uploads diagnostics on failure.

Run this gate before submitting Rust changes, using the same revision intended for CI. Passing `cargo test`, Clippy or a report generator without a threshold does **not** certify coverage. macOS/Windows compile different paths; a local report there does not replace the Linux gate. On those hosts, use an Ubuntu x86_64 VM/WSL environment or the GitHub coverage job for the reference measurement. Change the pins deliberately and rerun the full gate when upgrading tools.

For additional Windows diagnostics:

```powershell
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov
powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope all-targets -Format html
# Machine-readable alternative:
powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope all-targets -Format json
```

The Windows helper uses cargo-llvm-cov, explicitly enforces the shared 95% **line** threshold, and defaults to `all-targets`. `tests` and `lib` measure narrower scopes. Its success certifies only the selected Windows scope, whose coverage denominator differs from Tarpaulin on Linux. Each run uses a fresh target and default report path with a single build job to avoid locked files and stale reports.

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

### Request response-headers deadline

Rust `DownloadSpec::request_headers_timeout(Duration)` and the appended Python
`request_headers_timeout` argument (seconds, default `None`) bound each request
from invocation through response headers, including pool waiting and DNS/TCP/TLS
connection setup. This is not pure server TTFB. Omission preserves the existing
header deadline inherited from `read_timeout`; body reads still use `read_timeout`.
The value must be positive and representable as a monotonic-clock deadline.
The connector's timeout can expire earlier.

Each retry and redirect hop gets a fresh deadline, including probes, GET fallback,
resume, and ordinary Range requests. This is not a total redirect-chain or download
deadline: existing retry counts, `max_retry_elapsed` check boundaries, and 429/503
`Retry-After` behavior remain unchanged. There is no automatic deadline shortening
or response-headers hedging.

Existing probe transport failures (including timeouts) may enter GET fallback;
these retain their separate retry scopes. `max_retry_elapsed` is not a hard
whole-download deadline or a combined deadline for both phases.
