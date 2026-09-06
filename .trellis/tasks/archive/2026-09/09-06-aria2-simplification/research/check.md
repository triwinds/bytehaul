# Independent Trellis review

Reviewed the task PRD/design/implementation plan, all check.jsonl context, the new transfer-storage contract, and every changed production path. No additional production correctness or performance regression was found.

## Findings (fixed)

- File: `src/session/multi.rs`
- Issue: The flush-failure regression only checked that no new control file appeared. It did not prove the documented contract that an existing checkpoint remains unchanged, nor exercise failure after the writer accepts the barrier.
- Fix: Seed an older 256-byte checkpoint, complete a second piece, fail both sending the barrier and receiving its acknowledgement, and assert that the original file remains byte-for-byte identical. Also assert the persisted bitset/downloaded bytes and tracker stay at the older state while the scheduler has 512 completed bytes.

## Findings (not fixed)

None. No source rewrite, public API change, or out-of-scope refactor was needed during review.

## Reviewed invariants

- Producer chunk bounds and the cache watermark leave sufficient semaphore capacity for progress, including odd/tiny budgets and multiple producers. Cancellation or a closed writer interrupts limiter, budget and channel waits; an acquired but unqueued reservation returns its permits. Enqueued byte accounting has no intervening await.
- The writer preserves expected contiguous offsets across cache drains, isolates lease identities, returns written/discarded/stale payload permits, and sorts global flush blocks by file offset.
- Piece completion follows the writer's lease acknowledgement. Checkpoints freeze bitset/hints before the FIFO sync barrier, and terminal snapshots without a live writer require successful writer shutdown. Single interrupted forwarding saves the durable prefix after successful shutdown.
- Sparse scheduler transitions maintain active/missing counts and the availability bitset. Renewal preserves counts; reclaim merges only that lease's missing range; completed pieces release detailed state. The missing/active range partition makes completion inference valid without a second completed-range collection. Restored completion counts ignore padding and duplicate completion.
- Hickory remains the shared bounded TTL cache. Tests exercise UDP DNS queries, resolver cloning, expiry, and connector reconnection. Final Arc-backed direct/proxy client variants preserve immutable request and proxy-header behavior while avoiding a large enum and costly clones.
- Pool comparisons use actual downloads, count connections/requests and compare every output byte; documented localhost limitations justify retaining the opt-in default. Architecture/tuning docs and transfer-storage specs match the final design. Control-file V1/V2 formats and supported Rust/Python configuration remain unchanged.

## Verification

- Focused reviewer test: PASS — `rtk proxy cargo test -p bytehaul --lib test_persist_multi_control_snapshot_returns_on_flush_failure` (1 passed, 344 filtered).
- Formatting of the edited module: PASS — `rtk proxy rustfmt --edition 2021 --check src/session/multi.rs`.
- Diff whitespace: PASS — `rtk proxy git diff --check`.
- Lint / workspace type-check / broad Rust and Python tests: root session owns these final gates; results are recorded in the task's verification report. This reviewer intentionally did not duplicate concurrent full-suite runs.

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
