# Independent review

## Findings (fixed)

- File: `scripts/tests/test_coverage.py`
- Issue: the subprocess fixtures inherited `GITHUB_STEP_SUMMARY` from Actions. Running the six synthetic gate cases in CI would append fake coverage successes/failures and temporary report paths into the real job summary.
- Fix: isolate that environment variable in the test harness and add a regression asserting an existing Actions summary remains unchanged. Seven script tests now pass.

- File: `src/session/multi.rs` (new coverage fixtures).
- Issue: the short/oversized body fixture and writer-discard fixture initially assumed a localhost body arrived in one data chunk. Legal TCP fragmentation could violate their assertions without a production defect.
- Fix: reported to the owning implementer, who changed both fixtures to aggregate data, verify contiguous offsets/exact forwarded bytes and counters, and reject any flush or surplus bytes. The oversized case accepts any valid prefix up to the segment limit. A transient compilation error in the revised test diagnostic (`WriterCommand` has no Debug) was also fixed by the owner by removing debug formatting, without changing the production type.

## Findings (not fixed)

No additional code defect found in the completed single-transfer storage fix or coverage workflow/scripts/README changes. No design change or public-interface change is required by this review.

Actual Linux coverage measurement and any resulting gap tests are owned by the root session and must be recorded separately; native checks below do not certify the Linux 95% gate. Windows PowerShell execution is unavailable on this macOS host; CLI review and implementation source verification are recorded in `coverage-gate.md`.

## Review evidence

- Read the task PRD/design/implementation instructions and all check.jsonl references.
- Verified every changed writer start/flush/reset/close call publishes the original typed storage error's terminal state. Successful finalization cannot publish Completed or delete the previous control file until flush and close succeed.
- The portable flush/close fixtures call the production finalizer, assert the exact IO error, Failed state/cleared ETA, joined writer, and both absent/existing checkpoint cases. Reused the implementer's mutation evidence: removing the error observers makes both regressions fail. The Linux `/dev/full` test remains enabled and now rejects a checkpoint at the unproven received offset.
- Reviewed shared pinned configuration, package/all-targets/LLVM/95% command, locked dependencies, proxy isolation, fresh report/profile paths, retained failing exits and Actions artifact upload. No threshold reduction, source exclusion or platform test skip added.
- Error and testing specs mirror the implemented durability and coverage contracts; English/Chinese README entry points agree.

## Verification

Host: macOS, native Rust 1.96.0. These checks use the current uncommitted worktree; they are functional/lint checks, not a pinned Linux coverage measurement.

- Lint/type-check: `cargo clippy --workspace --all-targets -- -D warnings` passed, including the Python binding crate.
- Documentation: `RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --workspace` passed.
- Doc tests: `cargo test -p bytehaul --doc` passed (1).
- Python script checks: `python3 -m unittest discover -s scripts/tests -p test_coverage.py` passed (7); `python3 -m py_compile scripts/coverage.py scripts/tests/test_coverage.py` passed.
- Diff whitespace: `git diff --check` passed.
- Full Rust all-targets tests: proxy-isolated `cargo test -p bytehaul --all-targets` passed (347 unit + 66 integration = 413), with all benchmark smoke executions passing and the example target compiling successfully.

## Final follow-up review

Reviewed the subsequent final-offset progress fix, six manager configuration/cache test fixtures, and coverage OS/build-job metadata additions. No additional defect found; no follow-up edits were necessary.

- The finalizer publishes the known received prefix before flushing, so a throttled small body reports 4 rather than stale 0 on a writer error. This progress update does not alter durable checkpoint contents or publish Completed: those operations still follow successful flush and writer join. Both portable failure fixtures now start progress at 0 and assert 4 while preserving the old durable offset of 1.
- Manager tests use ephemeral localhost HTTP 403 fixtures and per-test temporary paths, preserve all cache-count invariants, and now assert an exact nonretryable status for every attempt. Proxy tests use the local server as the proxy for a reserved `.invalid` origin. No production retry policy changed.
- OS release and configured build jobs are captured in metadata, with subprocess tests covering OS metadata and absent os-release fallback. Existing threshold/report/exit semantics are unchanged.

Final native verification on the reviewed worktree:

- Proxy-isolated `cargo test -p bytehaul --all-targets`: passed again (347 unit + 66 integration = 413), all benchmark smoke executions passed, example target compiled. Unit execution took 2.47 seconds after the fixture changes versus 173.97 seconds in the preceding full unit run; compilation time is excluded and this is a test-runtime observation, not a product-performance benchmark.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed again.
- Coverage script subprocess regressions: 7 passed again, including updated metadata assertions.
- `git diff --check`: passed.
- Unchanged documentation/doc-test results above were reused, as requested.

These native Rust 1.96.0 macOS checks still do not establish Linux coverage. The root session owns the fresh pinned Linux run and its actual percentage; record OS, architecture, tool versions, and any local linker/build-job differences when presenting that evidence. Windows PowerShell execution remains unverified on this host. No commits were made by the reviewer.

## Review after measured coverage gaps

The root session obtained a valid Linux baseline of 3289/3524 lines (93.33%) and assigned behavior tests for observed gaps. Reviewed all subsequent test additions in `http/worker.rs`, `session/range_validate.rs`, `session/single.rs`, and `session/multi.rs`. This batch adds tests only; it does not alter production branches, coverage exclusions, or the 95% threshold.

- HTTP tests assert resolved relative URL and exact request counts, rejected loops with an empty cache, and refreshed cached redirects after both 403 and 404.
- Range tests assert identity encoding/known total requirements, invalid representation boundaries, exact error status/message and retryability, and numeric versus invalid Retry-After handling.
- Single-transfer tests drive real HTTP bodies and writer files: continuation Range offsets/counts, 503 then 403, pause/cancel, precise received/file/control prefixes, typed short/overlong-body errors, and final writer/advertised length mismatches without changing checkpoints.
- Multi-transfer tests assert a preserved completed prefix after permanent HTTP failure, short/oversized body counters and queued bytes, stop before lease acquisition, lease reclamation after invalid metadata/discard failure, retryable checkpoint persistence after filesystem repair, and Linux writer failure preserving the previous checkpoint.
- Two chunk-boundary assumptions and the transient test diagnostic compilation issue were corrected by the owner after review (see Findings fixed). Final assertions remain exact for bytes, offsets, counters and state while allowing legitimate network fragmentation. No unresolved issue remains in this batch.

Final checks after the owner confirmed all source edits were frozen:

- Proxy-isolated `cargo test -p bytehaul --all-targets`: passed, **363 unit + 66 integration = 429**, all benchmark smoke executions passed, example target compiled. The unit portion took 2.89 seconds; build time was separate.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed after the diagnostic fix.
- `cargo fmt --all -- --check`: passed on the final source.
- Coverage script subprocess tests: 7 passed; `git diff --check` passed.
- Previously passing documentation/doc-test checks reused because these additions only change test modules.

The final Linux percentage is still root-owned and must come from the fresh shared gate. These native checks do not verify the additional Linux-only multi-writer fixture, the CI Ubuntu/x86_64 platform, or Windows PowerShell execution.

Root final Linux shared gate:96.59% (3404/3524), exit0,434 tests pass. Full provenance and platform limits in validation.md.
