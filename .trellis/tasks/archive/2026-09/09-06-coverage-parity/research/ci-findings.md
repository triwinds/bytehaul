# Coverage discrepancy: verified findings

Investigation date: 2026-09-06. No coverage tool was run locally in this investigation and no product code or coverage threshold was changed.

## Commit and revision boundary

The requested implementation was committed as `f483f37` (fix: simplify download flow and preserve durable checkpoints), then archived/journaled. Nothing was pushed. Remote Actions below tested earlier commits, not f483f37.

## Two distinct failures

| Run | Revision | Result |
| --- | --- | --- |
| [2026-09-06](https://github.com/triwinds/bytehaul/actions/runs/34033292253/job/101486719676) | 1667903 | Coverage collection aborted: 352 passed, 1 failed; no final coverage percentage |
| [2026-08-25](https://github.com/triwinds/bytehaul/actions/runs/32875080449/job/97890963427) | 2d36de2 | 348 unit tests passed; coverage 2975/3140 = 94.75%, below 95% |

The September [ordinary Ubuntu tests](https://github.com/triwinds/bytehaul/actions/runs/34033292253/job/101486719871) failed identically. Windows/macOS passed. The exact failed assertion was `src/session/single.rs:1572`: actual Downloading, expected Failed, in `test_run_single_connection_reports_writer_failure`.

At revision 1667903, single.rs:447 executes `writer.flush().await?` in the Complete branch. A /dev/full IO error returns before marking Failed. The test is Linux-only. Current f483f37 retains this path at single.rs:458 and the Linux-only test. Thus the latest CI failure is a functional error-path defect hidden by local platform selection; running more macOS tests cannot verify this Linux path.

The following test expectations also need careful repair: it expects a control snapshot advertising 4 bytes after /dev/full failed. Received bytes are not proven durable. A correct fix must publish Failed and preserve the previous durable checkpoint, without writing a new snapshot claiming unsaved bytes. Do not disable the test or restore unsafe checkpoint semantics to make it green.

## August coverage shortage

95% of 3140 requires 2983 covered lines; 2975 is short by 8. Principal uncovered files from the actual report:

| File | Covered / total | Uncovered |
| --- | --- | --- |
| src/session/multi.rs | 349 / 433 | 84 |
| src/network.rs | 238 / 263 | 25 |
| src/http/worker.rs | 112 / 132 | 20 |
| src/error.rs | 50 / 63 | 13 |
| src/session/range_validate.rs | 67 / 78 | 11 |

These five files account for 153 of 165 missed lines. Reports also include benches/storage_bench.rs (145/145) and bindings/python/src/lib.rs (0/2), despite the package selector. Removing two binding lines alone would still not reach 95%; changing exclusions is not a demonstrated solution.

Only src/error.rs and src/http/mod.rs changed between last successful master 3efcb42 and 2d36de2 (body transport classification plus regression test). Therefore it is not justified to blame large multi-connection code additions for that historical drop. The May successful run 26452795160 still has successful job metadata but the log endpoint for job 77878089156 returns HTTP 410. Its precise percentage, denominator and tool versions are unavailable. No causal claim about tool upgrades or coverage nondeterminism can be proven from a passing badge alone.

## Local vs CI contract

- CI command: `cargo tarpaulin --engine llvm -p bytehaul --all-targets --out Stdout --fail-under 95`, on Ubuntu.
- Windows helper: cargo-llvm-cov collect/report, no `--fail-under-*`; exit zero does not certify 95%. It allows lib/tests/all-targets scopes, defaults all-targets.
- Current local session validated functional tests, Clippy and docs on macOS; it did not measure coverage. No corresponding prior passing local percentage report was located. A report on another host may exist; this investigation cannot compare its metric/revision without that evidence.
- README commands use `--workspace`, whereas Actions uses `-p bytehaul`.
- Platforms compile different storage paths and tests. Source also contains `cfg(not(tarpaulin))` branches; cargo-llvm-cov does not automatically match that build configuration. Matching the word LLVM does not establish equivalent denominators.
- CI installs floating stable Rust and unpinned Tarpaulin. August used rustc 1.98.0 and Tarpaulin 0.37.2; September used rustc 1.98.1 and Tarpaulin 0.37.2. Version drift is a variable, not a proven explanation.

## Recommended next changes

1. Fix the verified single-transfer storage-error terminal state and its durable-checkpoint assertions; exercise the failure deterministically on every platform, retaining a Linux /dev/full check.
2. Run the actual Linux gate at the same revision; collect its missed-line report before selecting additional meaningful tests for error/cancellation branches.
3. Align documented package scope, label Windows coverage as platform-specific and add an explicit line threshold if it is meant to gate success. Save CI machine-readable reports and tool/revision metadata even when the percentage gate fails.

Raw logs were inspected from /tmp/bytehaul-job-101486719676.log, /tmp/bytehaul-job-101486719871.log and /tmp/bytehaul-job-97890963427.log. These full runner logs are not committed; the public job links and minimum relevant evidence above are retained instead.
