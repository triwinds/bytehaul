# Validation record

## Regression baseline

Before fixes, `cargo test --test m11_flow_control` failed both tests: the single transfer with budget 1 exceeded the 15-second deadline, and cancellation during a rate-limited single body exceeded the 2-second deadline. The server and outputs are local and ephemeral; the test disables autosave in the small-budget matrix.

## Scheduler comparison

Direct source harness, release build, median of 9 samples, 1,000 calls to `assign_to_with_split(0, 4, 262144)` plus `complete`, setup excluded:

| Piece count | Before | After |
| --- | --- | --- |
| 1,000 | 0.536 ms | 0.264 ms |
| 10,000 | 29.974 ms | 0.199 ms |
| 100,000 | 376.603 ms | 0.196 ms |

This isolates scheduler work and is not a download-speed claim. The committed Criterion group measures this actual split-enabled path.

## Pool comparison

`cargo run --release --example pool_compare -- 3 2` with proxy variables unset: 32 MiB, 4 workers, 1 MiB pieces, 2 ms server delay/request. All 33,554,432 bytes verified in each run.

| Round | Pool | ms | Requests | TCP accepts |
| --- | --- | --- | --- | --- |
| 1 | off | 122.617 | 33 | 33 |
| 1 | on | 70.625 | 33 | 5 |
| 2 | on | 68.502 | 33 | 5 |
| 2 | off | 101.109 | 33 | 33 |
| 3 | off | 99.350 | 33 | 33 |
| 3 | on | 66.426 | 33 | 5 |

Median elapsed 101.109 to 68.502 ms; localhost diagnostic only. Keep default pooling disabled because this does not establish WAN/TLS/proxy reliability.

## Final gates

- `cargo test -p bytehaul --all-targets`: PASS, 345 unit + 66 integration tests; benchmark smoke cases and example target passed.
- `cargo test -p bytehaul --doc --quiet`: PASS, 1 doc test.
- Final session rerun after terminal-save cleanup and strengthened checkpoint test: 102 passed.
- Final m11 rerun after strengthening nonzero-prefix stop/resume: 2 passed.
- Final network suite after Arc client representation: 33 passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: PASS. Earlier large-enum warning resolved using shared Arc clients, no suppression.
- `RUSTDOCFLAGS=-Dwarnings cargo doc --no-deps --workspace`: PASS.
- `uv sync --project bindings/python`, `uv run maturin develop --bindings pyo3`, `uv run pytest -q`: PASS, 42 Python tests on the rebuilt extension.
- Independent Trellis check: no unresolved findings. Reviewer strengthened failure preservation of an existing checkpoint; targeted test passed.
- Task context manifests, modified Markdown relative links and diff whitespace: PASS.

Validation ran on local macOS. Linux-only/Windows-only paths and Ubuntu Tarpaulin coverage remain CI checks, not claimed as locally executed. No supported API/default-pooling/control-format changes. No commits or remote actions.
