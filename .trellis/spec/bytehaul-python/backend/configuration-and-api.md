# Configuration and Public API

## Configuration Levels

`src/manager.rs::DownloaderBuilder` owns reusable client defaults: connection timeout, proxies, DNS/DoH, IPv6, idle pooling, task concurrency, and default log level. `src/config.rs::DownloadSpec` owns one task's URL, output, headers, timeouts, memory/pieces, retries, rate limit, resume, and checksum.

Per-task values can override downloader defaults. `DownloadSpec` records explicit timeout/pool override intent, while proxy presence is itself an override. `Downloader::download` then derives or reuses the matching client. Preserve the difference between “equal to the default” and “explicitly overridden.”

## Builder and Validation Pattern

```rust
let spec = DownloadSpec::new(url)
    .output_path(path)
    .max_connections(4)
    .retry_policy(5, base_delay, max_delay);
spec.validate()?;
```

Follow `src/config.rs`:

- initialize task defaults in `DownloadSpec::new` and downloader defaults through `Downloader::builder`/`ClientNetworkConfig::default`;
- expose a consuming setter and a getter when adapters/users need it;
- record explicit override state where client selection depends on it;
- reject invalid values in `DownloadSpec::validate` or `DownloaderBuilder::build` with `DownloadError::InvalidConfig`;
- validate at the engine boundary even when Python performs earlier shape checks.

Do not silently clamp invalid values. Existing messages name the field and rule, such as `max_connections must be >= 1`.

## Public Surface

The supported Rust API is the `pub use` list in `src/lib.rs`. A new public concept should use stable types, have rustdoc, be deliberately re-exported, have a `tests/` case importing it through `bytehaul::{...}`, and update `README.md` or `docs/advanced.md` when user-visible. A Rust export does not automatically become a Python export.

## Mirror Audit for Options

Search an option and its default before changing it. Depending on scope, update:

- `DownloadSpec` or `DownloaderBuilder`, validation, getters, and unit tests;
- client keys/construction in `src/manager.rs` and `src/network.rs`;
- PyO3 signatures and `build_download_spec`/`apply_client_options` in `bindings/python/src/lib.rs`;
- `bindings/python/src/lib_tests.rs` and `bindings/python/tests/test_bytehaul.py`;
- `README.md`, `docs/advanced.md`, `bindings/python/README.md`, and translated docs advertising it.

Keep units explicit. Rust uses `Duration` and byte counts; Python seconds go through `duration_from_secs`. DNS values and enums use named parsers, not per-entry-point conversions.

Defaults are contracts: `src/config.rs::test_download_spec_defaults` asserts them and READMEs describe them. Python `None` should select the Rust default unless the Python API explicitly promises otherwise. Reuse `build_download_spec` for object and convenience APIs.

## Avoid

- Do not duplicate defaults in execution branches or expose raw public fields to bypass validation.
- Do not mutate a reusable `Downloader` for a per-task override.
- Do not update only the Rust setter for an option exposed in Python/docs.
- Do not widen root exports merely to let binding glue reach internals.
