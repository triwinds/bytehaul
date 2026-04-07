# experiments/

Standalone Rust experiment binaries used for HTTP-client performance research.
These are **not** part of the main `bytehaul` crate and are intentionally excluded
from the workspace to keep the primary CI surface clean.

Each sub-directory is its own independent Cargo project with its own `[workspace]`
table and `Cargo.lock`.

## hyper-dl

Minimal multi-connection parallel Range downloader built directly on **hyper 1.x**
with curl-inspired socket options:

- `TCP_NODELAY = true` (mirrors `CURLOPT_TCP_NODELAY`)
- `SO_RCVBUF = 512 KiB` (mirrors curl's effective receive buffer)
- `HTTP/1.1` forced per connection (avoids H2 multiplexing)
- Each worker creates a fresh `Client` with `pool_max_idle_per_host=0`

```
cd experiments/hyper-dl
cargo run --release -- <URL> [connections]
```

## isahc-dl

Minimal multi-connection parallel Range downloader built on **isahc 1.8**
(async Rust wrapper around libcurl).  Each worker gets its own `HttpClient`
with `connection_cache_size=1` and `HTTP/1.1` forced, matching the §9
`curl`-crate setup.

```
cd experiments/isahc-dl
cargo run --release -- <URL> [connections]
```

## Benchmark reference

Baseline numbers (GitHub CDN, 134.7 MiB LLVM release, 8 connections):

| Tool               | Library          | Avg speed    |
|--------------------|------------------|--------------|
| aria2 1.37.0       | libcurl (C)      | ~193 MiB/s   |
| curl crate (§9)    | libcurl (Rust FFI)| ~219 MiB/s  |
| bytehaul           | reqwest + rustls | ~61 MiB/s    |
| **hyper-dl**       | hyper 1.x direct | TBD          |
| **isahc-dl**       | isahc / libcurl  | TBD          |
