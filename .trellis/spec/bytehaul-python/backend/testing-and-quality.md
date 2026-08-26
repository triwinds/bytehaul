# Testing and Quality

## Test Topology

- Unit tests live beside implementation in `#[cfg(test)] mod tests`; see `src/error.rs`, `src/config.rs`, `src/session/range_validate.rs`, and `src/storage/control.rs`.
- Public Rust integration tests under `tests/` use ephemeral localhost HTTP servers and `tempfile::tempdir`, grouped by basic lifecycle, resume, multi-worker, retry, features, faults, pause, filenames, and boundaries.
- `bindings/python/src/lib_tests.rs` covers private PyO3 glue.
- `bindings/python/tests/test_bytehaul.py` validates the installed module, exceptions, threading/GIL, progress, cancellation, and files.

## Adding Tests

- Put pure helper/state tests beside their owner and caller-visible Rust behavior under `tests/`, importing only public `bytehaul` APIs.
- Model HTTP behavior with an ephemeral local server. Use temporary output/control paths and assert bytes, state, cleanup, or exact failure—not only `is_ok()`.
- Assert exact public variants/statuses for branch-sensitive behavior. `tests/m5_retry.rs` distinguishes 503 from 403; `tests/m8_pause_resume.rs` checks both `DownloadError::Paused` and progress state.
- Test single and multi paths when changing shared transfer semantics.
- Keep timing checks behavior-based with deadlines/coarse bounds.
- Use `pytest.raises` with public exception classes and retain GIL/thread progress coverage for runtime changes.

A regression must fail if production behavior is reverted:

```rust
let err = downloader.download(spec).wait().await.unwrap_err();
match err {
    DownloadError::HttpStatus { status, .. } => assert_eq!(status, 403),
    other => panic!("expected HttpStatus 403, got: {other:?}"),
}
```

Avoid tests that reproduce expected values with the helper under test or still pass after removing the feature.

## CI-Equivalent Checks

`.github/workflows/test.yml` is authoritative:

```bash
cargo test -p bytehaul --all-targets
cargo test -p bytehaul --doc
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace
```

Python tests require the built extension:

```bash
uv sync --project bindings/python
cd bindings/python
uv run maturin develop --bindings pyo3
uv run pytest
```

### Local Proxy Isolation

The HTTP tests intentionally use ephemeral localhost servers. A developer shell may export
`HTTP_PROXY`, `HTTPS_PROXY`, or `ALL_PROXY` (including lowercase variants); the Rust client then
inherits those values and can route localhost traffic through the proxy. Typical symptoms are an
`all_proxy URL ... must use http or https` error for a SOCKS5 proxy, broad client-construction
failures, or HTTP 502 responses in otherwise unrelated integration tests.

When validating local behavior rather than environment-proxy support, isolate the test process:

```bash
env -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY \
    -u http_proxy -u https_proxy -u all_proxy \
    cargo test -p bytehaul --all-targets
```

Apply the same isolation to `uv run pytest`. Do not change product proxy handling or weaken test
assertions to compensate for a developer-machine proxy. Tests that explicitly exercise environment
proxy behavior must continue to set and restore their own variables.

Ubuntu coverage uses:

```bash
cargo tarpaulin --engine llvm -p bytehaul --all-targets --out Stdout --fail-under 95
```

Use the Windows coverage helper documented in `README.md`. Focused checks come first; full relevant checks run before completion. Documentation-only specs still require template-marker, link, and index validation.

## Review Checklist

- Correct engine/binding layer and all option/state/error/export mirrors searched?
- Cancel/pause, retry, timeout, terminal progress, and persistence compatibility preserved?
- Python blocking work releases the GIL and keeps exception types?
- Tests cover observable success and failure?
- Clippy and rustdoc pass with warnings denied?

## Avoid

- No external network services, fixed shared paths, or fixed ports in tests.
- Do not weaken assertions to hide races; use deterministic signals or behavior deadlines.
- Do not add broad warning allowances; existing ones are narrow and justified.
