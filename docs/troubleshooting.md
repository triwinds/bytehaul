# Troubleshooting Guide

Common issues and how to resolve them.

[中文版](troubleshooting.zh-CN.md)

## Control File Corrupted

**Symptom:** Download fails on resume with `ControlFileCorrupted` error.

**Cause:** The `.bytehaul` control file was partially written (e.g., power loss during save) or modified externally.

**Fix:** Delete the control file and restart the download. The control file is located next to the output file with a `.bytehaul` extension:

```bash
rm /path/to/your-file.zip.bytehaul
```

The download will restart from scratch.

## Proxy Configuration

**Symptom:** Connection failures when behind a proxy, or `ProxyError` in logs.

**Resolution:**

1. **SOCKS5 / HTTP proxy via API:**
   ```python
   import bytehaul

   dl = bytehaul.Downloader(
       proxy="http://proxy.example.com:8080"  # all protocols
       # or:
       # http_proxy="http://proxy:8080",
       # https_proxy="http://proxy:8080",
   )
   ```

2. **Environment variables:** bytehaul (via reqwest) also respects `HTTP_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY`.

3. **Verify proxy reachability** independently:
   ```bash
   curl -x http://proxy:8080 https://example.com
   ```

## DNS Resolution Failures

**Symptom:** `DnsError` or `ResolveError` in the error message.

**Possible causes and fixes:**

1. **Custom DNS servers unreachable:**
   If you configured custom DNS servers, verify they are accessible:
   ```python
   dl = bytehaul.Downloader(dns_servers=["8.8.8.8:53", "1.1.1.1:53"])
   ```

2. **IPv6 issues:**
   Some networks have broken IPv6 connectivity. Disable IPv6 resolution:
   ```python
   dl = bytehaul.Downloader(enable_ipv6=False)
   ```

3. **Corporate firewall blocking DNS:**
   Try using the system resolver (don't set `dns_servers`) and ensure your firewall allows DNS traffic.

## Log Levels

Enable logging to diagnose issues. bytehaul uses the `tracing` framework internally.

### Python

```python
bytehaul.download(url, log_level="debug")
# or with Downloader:
dl = bytehaul.Downloader(log_level="debug")
```

### Rust

```rust
use bytehaul::{Downloader, LogLevel};

let dl = Downloader::builder()
    .log_level(LogLevel::Debug)
    .build()?;
```

Available levels (from most to least verbose): `trace`, `debug`, `info`, `warn`, `error`, `off` (default).

**Recommended levels:**
- `debug` — shows HTTP request/response details, piece scheduling, control file operations
- `trace` — includes per-chunk data flow (very verbose, for deep debugging only)
- `info` — high-level progress events

## Download Stalls or Slow Speed

**Symptom:** Download appears to hang or runs significantly below expected speed.

**Checklist:**

1. **Rate limiter set?** Check that `max_download_speed` is not set to a low value.

2. **Memory budget too low?** If your disk is slow and the memory budget is small, workers will stall waiting for flushes. Try increasing `memory_budget`:
   ```python
   bytehaul.download(url, memory_budget=128 * 1024 * 1024)  # 128 MiB
   ```

3. **Too many connections?** Some servers throttle clients with many parallel connections. Try reducing `max_connections` to 1 or 2.

4. **Server does not support Range?** If the server doesn't support Range requests, bytehaul falls back to single-connection mode. Check debug logs for "server does not support range" messages.

## Resume Not Working

**Symptom:** Download restarts from the beginning instead of resuming.

**Possible causes:**

1. **`resume` is disabled:** Ensure `resume=True` (default).

2. **Server changed the file:** If the server's `ETag` or `Last-Modified` header changed since the last download attempt, bytehaul discards the control file and restarts to avoid data corruption.

3. **Control file missing:** The `.bytehaul` file was deleted or the output file was moved to a different directory.

4. **Server does not support Range requests:** Resume requires the server to accept `Range` headers. Single-connection mode tracks `downloaded_bytes` for resume but still requires Range support.

## Python: Import Errors

**Symptom:** `ImportError: No module named 'bytehaul'` or similar.

**Fix:** Ensure the native extension is built and installed:

```bash
cd bindings/python
uv sync
uv run maturin develop
```

For production use, install the wheel:
```bash
pip install bytehaul
```

## Windows Coverage Reports

**Symptom:** `cargo tarpaulin` on Windows fails with incomplete output, `os error 232`, `os error 5`, `LNK1104`, or leaves `cargo` / `rustc` processes behind that keep binaries and log files locked.

**Cause:** Windows coverage runs are sensitive to process cleanup and target-directory reuse. If an earlier run is interrupted, or if two coverage commands share the same target directory, later runs can fail while cleaning old artifacts or linking instrumented test binaries.

**Fix:**

1. Stop any stale `cargo` / `rustc` processes before starting a new coverage run.
2. Avoid running multiple coverage commands in parallel on Windows.
3. Use the dedicated helper, which forces a single build job and an isolated target directory:

```powershell
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov
powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope tests -Format html
```

If you want a JSON summary instead of HTML:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/coverage-windows.ps1 -Scope tests -Format json
```

The Linux CI gate still uses Tarpaulin for the repository-wide threshold check.
