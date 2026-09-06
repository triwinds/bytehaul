# Performance Tuning Guide

This guide covers the key parameters that affect bytehaul's download performance and resource usage.

[中文版](tuning.zh-CN.md)

## `piece_size`

**Default:** 1 MiB  
**Range:** Must be > 0

The piece size determines the granularity of multi-connection downloading and resume tracking. Each piece is independently assigned to a worker, downloaded, and checkpointed in the control file.

| Scenario | Recommended Value |
|----------|-------------------|
| Small files (< 10 MB) | 256 KiB – 512 KiB |
| Medium files (10 MB – 1 GB) | 1 MiB (default) |
| Large files (> 1 GB) | 4 MiB – 16 MiB |

**Trade-offs:**
- Smaller pieces → finer resume granularity, but more scheduling overhead and larger control files
- Larger pieces → less overhead, but more data to re-download on resume after a partial piece failure

## `memory_budget`

**Default:** 64 MiB  
**Range:** Must be > 0

Limits payload bytes reserved for the writer queue and write-back cache. Bytehaul splits large response chunks before forwarding them and flushes the cache with enough headroom for further chunks, so small or non-divisible budgets still make progress. Very small budgets increase channel and disk I/O overhead. HTTP/TLS receive buffers and other process memory are outside this payload budget.

| System Memory | Recommended Budget |
|---------------|-------------------|
| ≤ 2 GB | 16 – 32 MiB |
| 4 – 8 GB | 64 MiB (default) |
| ≥ 16 GB | 128 – 256 MiB |

**Trade-offs:**
- Lower budget → less memory usage, but workers may stall waiting for disk flushes on slow storage
- Higher budget → smoother throughput on fast networks with slow disks, but increased memory footprint

## `max_connections`

**Default:** 4  
**Range:** Must be ≥ 1

Number of parallel HTTP connections used for a single download. Each worker fetches a different piece concurrently.

| Network / Server | Recommended Value |
|------------------|-------------------|
| Single server, residential connection | 2 – 4 |
| CDN-backed server, high bandwidth | 8 – 16 |
| Rate-limited or fragile server | 1 – 2 |

**Trade-offs:**
- More connections → higher aggregate throughput (up to network/server limits), but more server load
- Fewer connections → more polite to the server, simpler failure handling
- Many servers enforce per-IP connection limits; exceeding them may cause 429 or connection resets

## `channel_buffer`

**Default:** 64  
**Range:** Must be > 0

Size of the internal Tokio channel buffer between HTTP stream readers and the write-back cache. Controls how many data chunks can be in-flight between the network layer and the caching layer.

In most scenarios the default is optimal. Increase it only if you observe workers frequently blocking on channel sends (visible in `trace`-level logs).

## `min_split_size`

**Default:** 10 MiB

Files smaller than this threshold are downloaded with a single connection regardless of `max_connections`. This avoids the overhead of multi-connection coordination for small files.

## `control_save_interval`

**Default:** 5 seconds

How often the downloader evaluates whether it should persist a durable control file (`.bytehaul`) during a download. The actual durable save cadence is also gated by `autosave_sync_every`. Lower values improve resume accuracy at the cost of more disk writes; higher values reduce I/O overhead but risk losing more progress on crash.

| Scenario | Recommended Value |
|----------|-------------------|
| High-speed network, large files | 2 – 3 s |
| Slow or metered connection | 10 – 30 s |
| SSD storage | 3 – 5 s (default) |
| HDD or network storage | 10 – 15 s |

## `autosave_sync_every`

**Default:** 2  
**Range:** Must be ≥ 1

Coalesces multiple autosave ticks into one durable save. If unsaved progress exists, bytehaul will only call the heavy `sync_data + control save` path on every Nth autosave tick. User-triggered `pause`, cancellation, and failure paths still force an immediate durable save.

| Scenario | Recommended Value |
|----------|-------------------|
| Fast SSD, tight crash window | 1 |
| Default mixed workload | 2 (default) |
| Slow HDD or network share | 3 – 4 |

**Trade-offs:**
- Lower values → less progress loss after a crash, but more `sync_data` and control-file churn
- Higher values → fewer durable flushes and smoother throughput on slow disks, but a larger crash window
- Approximate crash-loss window: `control_save_interval * autosave_sync_every`

## `max_download_speed`

**Default:** 0 (unlimited)  
**Unit:** bytes per second

Rate limiter for the download. Set to a non-zero value to cap bandwidth usage. Useful when sharing a connection with other applications.

## Retry Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `max_retries` | 5 | Maximum additional retries after the initial request/transfer attempt (`0` = no retries) |
| `retry_base_delay` | 1 s | Initial back-off delay |
| `retry_max_delay` | 30 s | Maximum back-off cap |
| `max_retry_elapsed` | None | Total retry time budget (None = unlimited) |

Requests and response-body retries share exponential back-off with equal jitter to avoid thundering-herd effects when multiple clients retry against the same server. A single-connection body failure resumes from the writer's flushed contiguous prefix; a Range or object-metadata mismatch safely truncates and restarts from zero.

## Benchmark Snapshot

Local Windows baseline captured on 2026-04-06 with:

```text
cargo bench --bench storage_bench -- --sample-size 10 --measurement-time 0.05 --warm-up-time 0.05 --noplot
```

Treat these as directional local baselines, not portable capacity claims. The shortened Criterion run was chosen to keep iteration time low while validating that the new benchmark surfaces produce usable numbers.

| Benchmark | Current local range |
|-----------|---------------------|
| `single_progress_reporting_throttled` | `12.883 µs – 14.010 µs` |
| `control_save_only` | `2.1611 ms – 2.6577 ms` |
| `scheduler_snapshot/10000` | `9.2247 ms – 11.477 ms` |
| `scheduler_snapshot/100000` | `879.09 ms – 926.72 ms` |
| `client_cache_default` | `7.5650 µs – 10.125 µs` |
| `client_cache_custom_dns` | `2.7433 ms – 3.3432 ms` |
| `client_cache_custom_doh_timeout` | `2.7134 ms – 3.8286 ms` |
| `cache_seq_append_single_piece` | `1.2518 ms – 1.3304 ms` |
| `cache_seq_append_multi_piece` | `3.2731 ms – 3.7718 ms` |

If you need stable comparisons across commits, rerun the same command on an idle machine and compare Criterion's generated history rather than treating a single short local run as definitive.

## HTTP idle pool comparison

Idle pooling remains opt-in: `DownloadSpec::http_idle_pool(4, Duration::from_secs(30))` or `DownloaderBuilder::http_idle_pool(...)`. The default remains disabled.

Reproduce the local diagnostic with:

```bash
env -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u http_proxy -u https_proxy -u all_proxy cargo run --release --example pool_compare -- 3 2
```

On 2026-09-06, three alternating runs downloading 32 MiB with four workers and 1 MiB pieces gave median times of 101.1 ms with pooling disabled and 68.5 ms with pooling enabled. Accepted TCP connections fell from 33 to 5; all runs made 33 requests and verified every output byte. The server delays each request by 2 ms. This localhost HTTP comparison measures neither WAN/TLS handshakes nor proxy reliability, so it does not justify changing the default.
