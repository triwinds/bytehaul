# Independent release check

## Findings (fixed)

Local verification only: after `maturin develop` installed 0.2.1, a normal `uv run pytest` synchronized cached editable distribution metadata back to 0.2.0 while the native extension remained 0.2.1. Reinstalled the editable extension with `uv run --no-sync maturin develop --bindings pyo3`, then explicitly verified both native and distribution versions are 0.2.1 using `uv run --no-sync`. This did not require a product source change. Added the empirically verified `--no-sync`/dual-version check to the release-packaging guideline.

Removed only the reviewer-generated untracked `lib_bytehaul.dylib.dSYM` bundle after validation.

## Findings (not fixed)

No new release code, metadata or packaging defect found. The existing transitive spin 0.9.8 yanked warning reported by package verification is outside this patch release's explicitly agreed scope; no unrelated dependency remediation was attempted. Current cross-platform Actions, Linux coverage and registry publication remain main-session gates; native success is not a substitute for those results.

## Review evidence

- Read active PRD/design/implementation and check.jsonl references. Dependency PR dispositions and upstream evidence are in `research/pr-review.md`.
- Both workspace packages inherit version 0.2.1; both matching Cargo.lock entries are updated. Python pyproject version is dynamic, and the native module exports `CARGO_PKG_VERSION`. Existing release workflows resolve inherited versions and validate the tag against both packages.
- The only dependency change is the root dev-dependency requirement rand 0.8→0.9 and its lock reference 0.8.6→already-resolved 0.9.4. No direct rand API call site, production retry-policy change or unrelated lockfile update is introduced. Bincode/PyO3/warp remain at the previously validated versions.
- Four explicit crate excludes remove `.agents`, `.trellis`, `.codex`, and `.cursor`; source/public documentation/tests remain. Reused the implementer's successful `cargo metadata --locked` and `cargo package -p bytehaul --allow-dirty --locked` compile verification rather than rebuilding that package again.
- Independently inspected `target/package/bytehaul-0.2.1.crate`: 67 files, package version 0.2.1, dev rand requirement 0.9, zero files under the four excluded workflow directories. Confirmed `src/lib.rs`, README, Cargo.lock and the flow-control integration test are retained.
- Main-session Python sdist audit reports 74 files, version 0.2.1, expected Rust/Python sources and no internal workflow content. This is reused evidence, not an independently rebuilt sdist.
- Release packaging guidance matches the archive checks and version derivation; no executable release workflow was changed by this patch.

## Verification

Host: macOS/aarch64, native Rust 1.96.0 and Python 3.12.13. Network tests clear all six HTTP/HTTPS/ALL proxy environment variables.

- **Tests:** `cargo test -p bytehaul --all-targets` passed: **363 unit + 66 integration = 429**. All benchmark smoke executions passed; example target compiled.
- **Lint / TypeCheck:** `cargo clippy --workspace --all-targets -- -D warnings` passed for bytehaul 0.2.1 and bytehaul-python 0.2.1.
- **Doc tests:** `cargo test -p bytehaul --doc` passed (1).
- **Documentation:** `RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --workspace` passed.
- **Format:** `cargo fmt --all -- --check` passed.
- **Python:** rebuilt and installed the abi3 Python>=3.9 extension at version 0.2.1; all **42 pytest tests passed** in 4.81 seconds using that native extension. After correcting cached editable metadata, native `bytehaul.__version__` and distribution `importlib.metadata.version('bytehaul')` both explicitly verified as **0.2.1**. No unchanged functional suite was rerun solely for the local metadata correction.
- **Diff whitespace:** `git diff --check` passed.
- No local Linux coverage run, commit, push or registry publication was performed by this reviewer; CI supplies the authoritative Linux gate for the final release revision.
