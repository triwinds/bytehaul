# bytehaul

Python bindings for the [bytehaul](https://github.com/triwinds/bytehaul) Rust download library.

[中文使用文档](../../docs/python.zh-CN.md)

## Requirements

- Python 3.9+
- Rust toolchain (for building from source)
- `uv`

Commands below assume you are running them from the repository root.

## Installation

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

## API Reference

### `download(url, output_path=None, output_dir=None, **options)`

Blocking convenience function. Downloads a file and returns when complete.

- `output_path`: explicit filename or relative output path
- `output_dir`: destination directory for explicit or auto-detected filenames
- If `output_path` is omitted, bytehaul chooses `Content-Disposition` → URL path → `download`
- Absolute `output_path` values are still accepted when `output_dir` is omitted

### `Downloader(connect_timeout=None, proxy=None, http_proxy=None, https_proxy=None, dns_servers=None, doh_servers=None, enable_ipv6=None)`

Reusable downloader instance.

- `downloader.download(url, output_path=None, output_dir=None, **options) -> DownloadTask`

### `DownloadTask`

Handle to a running download.

- `task.progress() -> ProgressSnapshot` — current download progress
- `task.pause()` — pause the download and persist resume metadata when available
- `task.cancel()` — cancel the download
- `task.wait()` — block until download completes (releases GIL)

### `ProgressSnapshot`

Frozen snapshot of download progress.

| Attribute      | Type           | Description                     |
|----------------|----------------|---------------------------------|
| `total_size`   | `int \| None`  | Total file size (if known)      |
| `downloaded`   | `int`          | Bytes downloaded so far         |
| `state`        | `str`          | `"pending"`, `"downloading"`, `"completed"`, `"failed"`, `"cancelled"`, `"paused"` |
| `speed`        | `float`        | Recent-window speed in bytes/second |
| `eta_secs`     | `float \| None`| Estimated remaining seconds     |
| `elapsed_secs` | `float \| None`| Elapsed time in seconds         |

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
| `max_download_speed`| `int`            | `0` (unlimited)|
| `checksum_sha256`   | `str \| None`    | `None`        |
| `log_level`         | `str \| None`    | `None` (`"off"`) |

Valid `log_level` values: `"off"`, `"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"` (case-insensitive).

### Network options

Use these on `Downloader(...)` for the object API, or pass them directly to the blocking `download(...)` helper.

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
uv run --project . pytest tests/ -v
```

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

