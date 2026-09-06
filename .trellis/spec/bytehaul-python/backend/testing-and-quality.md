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
- Verify concurrent transfers with response gates/barriers and exact requested ranges: require all
  expected requests to arrive before releasing responses, then check output bytes and terminal state.
  Do not gate CI on a fixed wall-clock speedup between sequential runs (for example, split must save
  150 ms). Runner load, timer resolution and filesystem latency can change that delta despite correct
  parallel behavior. Use a generous timeout only to catch deadlocks; keep performance comparisons in
  benchmarks.
- Network fixtures must not assume one socket write becomes one body frame. Aggregate chunks and
  assert exact offsets/bytes, or choose boundaries whose expected outcome is independent of framing.
- Client configuration/cache tests use ephemeral local nonretryable responses and exact error/cache
  assertions. Do not wait through default retries against fixed unavailable ports when retry behavior
  is not the subject of the test. Keep retry/backoff verification in its dedicated tests.
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

## Coverage gate contract

### 1. Scope / Trigger

Apply this whenever reporting coverage or changing engine code, coverage tooling, scripts, or CI.
Functional tests, Clippy and docs are separate checks; passing them is never evidence of >=95% coverage.

### 2. Signatures

```bash
python3 scripts/coverage.py --install
python3 scripts/coverage.py
python3 -m unittest discover -s scripts/tests -v
```

The Linux entry reads `scripts/coverage-config.json` for Rust, Tarpaulin and the minimum line
percentage. `.github/workflows/test.yml` calls this entry on Ubuntu 24.04. The Windows PowerShell
helper checks llvm-cov line coverage using the same numeric threshold, but remains platform-specific.

### 3. Contracts

- Keep the root `bytehaul` package, all targets, LLVM engine and 95% line gate together in one Linux
  entry. No custom excludes or platform skips may be added merely to make the gate green.
- Preserve the underlying nonzero exit code through log capture and report export. Each run has
  fresh profiles/report paths; a previous report must never certify the current run.
- Record revision/dirty state, OS/architecture, actual tool versions, command, exit status and measured
  totals. A local report is comparable only when revision, tools, platform, scope and metric match.
- CI uploads log/metadata/available JSON/HTML reports even on failure. Classify failed tests separately
  from a completed coverage measurement below the threshold; partial coverage cannot pass the gate.
- Isolate shell proxy variables for localhost fixtures; do not change product proxy behavior.
- Tool upgrades require an explicit config update plus fresh measurement. Do not infer the cause of
  historical percentage changes from a badge when logs/tool metadata are unavailable.

### 4. Validation & Error Matrix

| Condition | Required outcome |
| --- | --- |
| Any test fails | Nonzero, coverage incomplete, retained log |
| Tests pass, measured lines <95% | Nonzero, reports retained, measured shortfall identified |
| Wrong tool version / missing report | Nonzero setup/report error |
| Windows report passes | Windows line gate passed; no claim of Linux equivalence |
| Linux-only storage path changes | Run Linux test; also use portable failure injection where feasible |

### 5. Good / Base / Bad Cases

- Good: the same Linux entry produces a fresh report and passes the pinned 95% gate.
- Base: a developer uses a focused Windows or macOS report to choose new tests, then verifies Linux.
- Bad: `cargo test` passes on macOS while a Linux-only `/dev/full` test fails; describing this as a
  coverage percentage difference hides the functional defect.

### 6. Tests Required

Gate script tests must assert actual failing exit propagation, preserved reports, fresh-run isolation,
and distinct diagnostics for failed tests, insufficient measured coverage and tooling failure. Run
the real Linux gate after script changes; stubs alone cannot verify tool output format or final totals.

### 7. Wrong vs Correct

Wrong: report “coverage meets 95%” after only functional tests or a successful report-export command.
Correct: cite the measured covered/total lines and gate exit code, with version/revision/platform.

## Release packaging

- Before publishing, synchronize the version examples and release applicability in `README.md`,
  `docs/README.zh-CN.md`, the Python README and translated guides. Check the actual default branch
  for badges/links and compare behavior descriptions against the release source. Mark historical
  design baselines explicitly; do not turn old implementation notes into current API claims.
- Bump the shared workspace version and matching workspace lock entries together; Python metadata
  derives its version from the binding crate. Verify both packages with `cargo metadata --locked`.
- After `maturin develop`, run local release checks with `uv run --no-sync` and verify both
  `bytehaul.__version__` and `importlib.metadata.version("bytehaul")`. A normal `uv run` can reinstall
  cached editable metadata from the previous workspace version while retaining the rebuilt extension.
- Run `cargo package -p bytehaul --allow-dirty --locked` while preparing a release, then inspect the
  generated archive. Keep source, public documentation and tests; exclude internal workflow directories
  such as `.agents/`, `.trellis/`, `.codex/` and `.cursor/` through the root package's `exclude` list.
  A successful compile alone does not verify the publication file list.

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
