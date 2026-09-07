# bytehaul

Python bindings for the [bytehaul](https://github.com/triwinds/bytehaul) Rust download library. This guide targets the published **0.2.1** release.

[中文使用文档](../../docs/python.zh-CN.md)

## Requirements

- Python 3.9+
- Rust toolchain and `uv` only when building from source; neither is required to install an available wheel.

Each source-build command block below assumes you start from the repository root.

## Installation

### From PyPI

```bash
pip install "bytehaul==0.2.1"
```

See the [0.2.1 release notes](https://github.com/triwinds/bytehaul/releases/tag/v0.2.1).

### From source (development)

```bash
uv sync --project bindings/python
cd bindings/python
uv run --project . maturin develop -m Cargo.toml
```

### Build wheel

```bash
cd bindings/python
uv run --project . maturin build --release -m Cargo.toml
```

## Usage

### Simple download

```python
import bytehaul

bytehaul.download("https://example.com/file.bin", output_path="output.bin")

# Let bytehaul decide the filename and place it in downloads/
bytehaul.download("https://example.com/file.bin", output_dir="downloads")
```

### With options

```python
bytehaul.download(
    "https://example.com/file.bin",
    output_path="output.bin",
    max_connections=8,
    max_download_speed=1_000_000,  # 1 MB/s
    headers={"Authorization": "Bearer token"},
)
```

### Network settings

```python
bytehaul.download(
    "https://example.com/file.bin",
    output_path="output.bin",
    proxy="http://127.0.0.1:7890",
    dns_servers=["1.1.1.1", "8.8.8.8:53"],
    doh_servers=["https://dns.google/dns-query"],
    enable_ipv6=False,
)
```

`doh_servers` expects HTTPS URLs. If you pass a hostname such as `dns.google`, bytehaul will use the system resolver once during client construction to bootstrap the DoH endpoint addresses.

### Logging

```python
# Enable debug logging on the convenience function
bytehaul.download(
    "https://example.com/file.bin",
    output_path="output.bin",
    log_level="debug",
)

# Or on the Downloader object
from bytehaul import Downloader

downloader = Downloader(log_level="info")
```

Valid levels: `"off"` (default), `"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"`.

### Object API with progress and cancellation

```python
from bytehaul import Downloader

downloader = Downloader(
    connect_timeout=15.0,
    dns_servers=["1.1.1.1"],
    doh_servers=["https://dns.google/dns-query"],
    enable_ipv6=False,
)
task = downloader.download(
    "https://example.com/large.bin",
    output_dir="downloads",
    proxy="http://127.0.0.1:7890",
)

# Poll progress
snap = task.progress()
print(
    f"State: {snap.state}, Downloaded: {snap.downloaded}, "
    f"Speed: {snap.speed:.0f} B/s, ETA: {snap.eta_secs}"
)

# Pause or cancel if needed
# task.pause()
# task.cancel()

# Wait for completion
task.wait()
```

### Error handling

```python
from bytehaul import download, DownloadFailedError, CancelledError, PausedError, ConfigError

try:
    download("https://example.com/file.bin", output_path="output.bin")
except ConfigError as e:
    print(f"Invalid parameter: {e}")
except PausedError:
    print("Download was paused")
except CancelledError:
    print("Download was cancelled")
except DownloadFailedError as e:
    print(f"Download failed: {e}")
```

Response-body timeouts, connection resets and early EOF for a known-size body are retried within `max_retries`, which counts additional attempts after the first (`0` disables retries). With a known total and matching object validators, continuation starts at the durable prefix confirmed by the writer; ignored Range requests or changed object metadata trigger a safe restart from zero. Disk write and synchronization errors are not retried as network failures.

## API Reference

### `download(url, output_path=None, output_dir=None, **options)`

Blocking convenience function. Downloads a file and returns when complete.

- `output_path`: explicit filename or relative output path
- `output_dir`: destination directory for explicit or auto-detected filenames
- If `output_path` is omitted, bytehaul chooses `Content-Disposition` → URL path → `download`
- Absolute `output_path` values are still accepted when `output_dir` is omitted

### `Downloader(connect_timeout=None, proxy=None, http_proxy=None, https_proxy=None, dns_servers=None, doh_servers=None, enable_ipv6=None, log_level=None)`

Reusable downloader instance.

- `downloader.download(url, output_path=None, output_dir=None, **options) -> DownloadTask`

Proxy settings passed to `Downloader(...)` act as defaults. You can override them per task by passing `proxy`, `http_proxy`, or `https_proxy` directly to `downloader.download(...)`.

### `DownloadTask`

Handle to a running download.

- `task.progress() -> ProgressSnapshot` — current download progress
- `task.pause()` — pause the download and persist resume metadata when available
- `task.cancel()` — cancel the download
- `task.wait()` — block until download completes (releases GIL)

`wait()` consumes the task handle. It cannot be called twice, and `progress()` is unavailable after it returns or raises.

### `ProgressSnapshot`

Frozen snapshot of download progress.

| Attribute      | Type           | Description                     |
|----------------|----------------|---------------------------------|
| `total_size`   | `int \| None`  | Total file size (if known)      |
| `downloaded`   | `int`          | UI-oriented received bytes, not a durable resume offset |
| `state`        | `str`          | `"pending"`, `"downloading"`, `"completed"`, `"failed"`, `"cancelled"`, `"paused"` |
| `speed`        | `float`        | Recent-window speed in bytes/second |
| `eta_secs`     | `float \| None`| Estimated remaining seconds     |
| `elapsed_secs` | `float \| None`| Elapsed time in seconds         |

`downloaded` may decrease during retries or already equal `total_size` when final synchronization fails. Final write or synchronization failures produce `failed` state, and the control file retains only confirmed durable progress. Use the result or exception from `task.wait()` to determine success; do not use the displayed byte count as a resume offset.

`speed` and `eta_secs` are computed from the same recent throughput window. `speed` is not a whole-download lifetime average, and `eta_secs` stays `None` until bytehaul has enough recent samples or a known total size.

### Download options

| Parameter           | Type             | Default       |
|---------------------|------------------|---------------|
| `output_path`       | `str \| Path \| None` | `None` |
| `output_dir`        | `str \| Path \| None` | `None` |
| `headers`           | `dict[str, str]` | `{}`          |
| `max_connections`   | `int`            | `4`           |
| `connect_timeout`   | `float` (secs)   | `30.0`        |
| `read_timeout`      | `float` (secs)   | `60.0`        |
| `memory_budget`     | `int`            | `67108864`    |
| `file_allocation`   | `"none" \| "prealloc"` | `"prealloc"` |
| `resume`            | `bool`           | `True`        |
| `piece_size`        | `int`            | `1048576`     |
| `min_split_size`    | `int`            | `10485760`    |
| `max_retries`       | `int`            | `5`           |
| `retry_base_delay`  | `float` (secs)   | `1.0`         |
| `retry_max_delay`   | `float` (secs)   | `30.0`        |
| `max_retry_elapsed` | `float \| None` (secs) | `None` |
| `control_save_interval` | `float` (secs) | `5.0` |
| `autosave_sync_every` | `int` | `2` |
| `max_download_speed`| `int`            | `0` (unlimited)|
| `checksum_sha256`   | `str \| None`    | `None`        |
| `log_level`         | `str \| None`    | `None` (`"off"`) |

`max_retries` counts additional retries after the initial request/transfer attempt; `0` disables retries. Single-connection body failures resume from the writer's flushed contiguous prefix, while Range or object-metadata mismatches reset the file before restarting.

`control_save_interval` checks whether a checkpoint is due; `autosave_sync_every` batches those checks when unsaved progress exists. Set `log_level` on the convenience `download(...)` function or the `Downloader(...)` constructor.

Valid `log_level` values: `"off"`, `"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"` (case-insensitive).

### Slow-transfer recovery (source checkout)

Not available in the published 0.2.1 package. These options apply to both `download(...)` and `Downloader.download(...)`; `None` selects the Rust engine default.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `slow_transfer_mode` | `"adaptive"` | `"disabled"`, `"adaptive"`, or `"adaptive_with_hedging"` (case-insensitive) |
| `low_speed_limit` | `None` | Optional positive absolute floor, bytes/second |
| `low_speed_duration` | `15.0` | Sustained low-speed time, seconds |
| `slow_start_grace` | `5.0` | Startup grace, seconds |
| `slow_sample_window` | `5.0` | Speed observation window, seconds |

Durations must be finite, positive and at most 86,400 seconds. Adaptive recovery is on by default for multi-connection Range downloads. Hedging is opt-in, needs a strong ETag and a spare connection slot, and stages at most one small spare response before choosing a writer. It does not duplicate progress or exceed `max_connections`. All network payload shares the configured rate limit. Performance recovery and hedging together reserve at most `min(total_size / 100, 16 MiB)` of extra Range work; requests that do not fit are skipped. Normal error retries use the existing retry policy.

```python
from bytehaul import Downloader

task = Downloader(log_level="debug").download(
    "https://example.com/file.bin",
    "file.bin",
    slow_transfer_mode="adaptive_with_hedging",
)
task.wait()
```

Use `slow_transfer_mode="disabled"` to retain the previous scheduling behavior. Single-connection and non-Range fallback behavior is unchanged. Recovery excludes intentional rate limiting and local backpressure; it cannot remove a shared origin bandwidth limit.

When `max_download_speed` is nonzero, automatic slow-request recovery and hedging are suppressed to avoid treating intentional rate limiting as a network fault. Ordinary timeouts and error retries still apply.

### Network options

Use these on `Downloader(...)` to set defaults, or pass `proxy`, `http_proxy`, and `https_proxy` directly to `downloader.download(...)` or the blocking `download(...)` helper.

| Parameter      | Type                | Default |
|----------------|---------------------|---------|
| `proxy`        | `str \| None`      | `None`  |
| `http_proxy`   | `str \| None`      | `None`  |
| `https_proxy`  | `str \| None`      | `None`  |
| `dns_servers`  | `list[str] \| None`| `None`  |
| `doh_servers`  | `list[str] \| None`| `None`  |
| `enable_ipv6`  | `bool \| None`     | `True`  |

## Running tests

```bash
uv sync --project bindings/python
cd bindings/python
uv run --project . maturin develop -m Cargo.toml
uv run --no-sync --project . pytest tests/ -v
```

After `maturin develop`, use `--no-sync` for tests so uv does not replace the freshly built development extension during another environment sync.

## Building wheels for release

Single platform:

```bash
cd bindings/python
uv run --project . maturin build --release -m Cargo.toml
```

Cross-platform (via CI):

```bash
# Linux x86_64 + aarch64, macOS x86_64 + arm64, Windows x86_64
# Use maturin's GitHub Actions: https://github.com/PyO3/maturin-action
```

The project uses `abi3-py39`, so a single wheel per platform covers all Python 3.9+ versions.

## License

MIT. See the repository LICENSE file.
