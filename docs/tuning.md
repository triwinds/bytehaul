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

Controls the maximum amount of data buffered in the write-back cache before back-pressure is applied to workers. Workers pause when the cache exceeds this threshold, creating natural flow control between network speed and disk I/O speed.

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
| `max_retries` | 5 | Maximum retry attempts per failed request |
| `retry_base_delay` | 1 s | Initial back-off delay |
| `retry_max_delay` | 30 s | Maximum back-off cap |
| `max_retry_elapsed` | None | Total retry time budget (None = unlimited) |

Retries use exponential back-off with full jitter to avoid thundering-herd effects when multiple clients retry against the same server.
