# Python Bindings

## Boundary Responsibilities

`bindings/python/src/lib.rs` adapts the public `bytehaul` crate. It parses Python-friendly values, constructs `DownloaderBuilder`/`DownloadSpec`, maps errors, wraps progress/task lifecycle, and bridges blocking calls to the async engine. It must not reimplement range, retry, resume, filename, storage, or progress algorithms.

## Runtime and GIL Pattern

The module owns one lazily initialized multi-thread Tokio runtime through `OnceLock<Result<Runtime, String>>`. Release the GIL around blocking engine work:

```rust
let runtime = shared_runtime()?;
py.allow_threads(move || runtime.block_on(handle.wait()).map_err(map_download_error))
```

Do not hold the GIL across network/disk work. Progress callbacks reacquire it only for `callback.call1` and use `write_unraisable` on callback failure.

`PyDownloadTask` stores `Arc<Mutex<Option<DownloadHandle>>>`. `wait()` consumes the handle once; progress/callback registration then reports “already consumed,” while cancel/pause after consumption are no-ops. Preserve and test this lifecycle unless deliberately changing the public contract.

## Conversion and Validation

Centralize conversion helpers: `duration_from_secs`, DNS/socket parsers, file-allocation/log/checksum parsers, and non-zero integer helpers. Both download APIs flow through `build_download_spec`; client-level options flow through `apply_client_options`. After Python shape checks, call `DownloadSpec::validate` so Rust remains the domain authority.

Long keyword signatures mirror the flat Python API, so `#[allow(clippy::too_many_arguments)]` is localized on adapters. Do not spread broad allows.

## Names and Exports

There are three layers:

1. `#[pymodule] _bytehaul` registers native functions, classes, exceptions, and version.
2. `bindings/python/python/bytehaul/__init__.py` imports intended package-level names.
3. `__all__` defines the public facade.

Update all intended layers, `bindings/python/README.md`, and import/export tests together. Current nuance: `ResumeError` and `InternalError` are native registered classes used by mappings but are not in the package facade's `__all__`; registration alone is not a documented top-level export.

## Test Layers and Packaging

- `bindings/python/src/lib_tests.rs` tests private conversions, mappings, registration, and wrapper lifecycle.
- `bindings/python/tests/test_bytehaul.py` tests the built package using localhost HTTP and temporary paths, including threading and GIL release.

`pyproject.toml` uses maturin with `module-name = "bytehaul._bytehaul"`; the binding crate uses PyO3 `abi3-py39`. Keep workspace/native versions, release workflows, README claims, and Python metadata aligned.

## Avoid

- Do not create a runtime per call or `block_on` while holding the GIL/task mutex longer than needed.
- Do not pass unchecked Python values into narrower Rust types.
- Do not duplicate spec construction between convenience and object APIs.
- Do not edit only native registration and forget the facade, tests, and docs.
