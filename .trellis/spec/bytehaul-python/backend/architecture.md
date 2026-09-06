# Architecture and Module Boundaries

## Workspace Shape

Bytehaul has two deliverables:

```text
Cargo.toml                     Rust workspace and public `bytehaul` crate
src/                           async download engine
tests/                         public Rust integration tests
benches/                       Criterion benchmarks
bindings/python/
  Cargo.toml                   PyO3 extension crate (`_bytehaul`)
  pyproject.toml               maturin/Python metadata
  src/lib.rs                   Rust-to-Python adapter
  python/bytehaul/__init__.py  Python import facade
  tests/test_bytehaul.py       installed-package tests
```

Keep protocol, scheduling, persistence, and download behavior in the root crate. The Python member translates types, validates Python input shapes, manages the blocking runtime bridge, and presents Python names. `src/lib.rs` is the deliberate Rust public surface; implementation modules stay private.

## Engine Pipeline and Owners

```text
Downloader::download -> session::run_download -> range probe/plain GET
 -> single::run_single_connection OR multi::run_multi_worker
 -> WriterCommand -> WriterTask (WriteBackCache for leased multi-worker data)
 -> output file + optional `.bytehaul` resume control file
```

- `src/manager.rs`: downloader defaults, client cache, concurrency limit, task spawning, and `DownloadHandle` control.
- `src/session/`: orchestration, output resolution, retry/range decisions, single/multi transfers, pause/cancel, and terminal state.
- `src/http/`: request construction, response metadata, body reads, and transport calls.
- `src/network.rs`: hyper client stack, proxy/DNS/DoH/TLS configuration, and DNS caching.
- `src/scheduler.rs` and `src/storage/{piece_map,segment}.rs`: piece leases and completion state.
- `src/storage/cache.rs`: leased multi-worker data aggregation.
- `src/storage/writer.rs` and `src/storage/file.rs`: writer command handling, positioned disk writes, file creation, and preallocation.
- `src/storage/control.rs`: versioned, checksummed resume files and atomic write/fsync/rename. This is persistence, not a database.
- `src/{progress,eta,rate_limiter,checksum}.rs`: focused supporting behavior.

`docs/architecture.md` documents the same flow and must remain aligned with structural changes.

## Placement Rules

- Put wire/body concerns in `src/http/`, lifecycle and strategy decisions in `src/session/`, and cross-worker durability in scheduler/storage types.
- Preserve writer acknowledgements before marking pieces complete; piece map, lease, cache, writer, and control snapshot form one data-flow chain.
- Keep pure parsers near their domain, as in `src/filename.rs`, `src/session/range_validate.rs`, and `DownloadSpec::validate`.
- Use `pub(crate)` for engine internals. Add a root `pub use` only for an intentional supported Rust API.
- Follow `ControlSnapshot::{save_with_hints,load_with_hints}` for blocking filesystem work: use `tokio::task::spawn_blocking`, while orchestration remains async.

## Persistence Compatibility

`ControlSnapshot` stores URL, object validators, piece geometry, and durable progress. `src/storage/control.rs` reads V1 and V2, validates magic/version/length/CRC, and writes atomically. A format change must version the representation, retain intentional old reads, validate before use, and test round trips, corruption, truncation, and legacy data.

## Local Pattern

Orchestrators pass typed values and preserve typed errors:

```rust
let (response, meta) = retry_with_backoff(
    spec.max_retries,
    spec.retry_base_delay,
    spec.retry_max_delay,
    spec.max_retry_elapsed,
    cancel_rx,
    || worker.send_get(),
)
.await?;
```

`probe_or_fallback_get` and `retry_plain_get` in `src/session/mod.rs` keep policy in one helper instead of flattening failures to text.

## Avoid

- Do not put business behavior in `bindings/python/src/lib.rs`; Rust and Python callers share engine semantics.
- Do not expose storage/scheduler internals for convenience. `src/lib.rs::bench` is explicitly hidden and benchmark-only.
- Do not write final resume metadata outside the atomic persistence path.
- Do not update only one transfer strategy. Retry, progress, pause/cancel, timeout, and body changes often need single- and multi-path coverage.
- Do not introduce ORM, migration, or service-layer conventions; none exists here.
