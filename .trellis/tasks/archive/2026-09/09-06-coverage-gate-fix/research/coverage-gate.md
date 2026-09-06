# Shared coverage gate implementation

## Change boundary

The gap lives in coverage invocation and reporting: local scripts and CI could use different tools/scopes, and Windows report success did not check 95%. This change owns scripts/coverage.py, scripts/coverage-config.json, scripts/tests/test_coverage.py, scripts/coverage-windows.ps1, .github/workflows/test.yml, and both root READMEs. It does not change the required percentage or exclude source files.

## Contract

- CI Ubuntu 24.04 x86_64 and local Linux run `python3 scripts/coverage.py --install` (omit --install after setup).
- Shared config pins Rust 1.98.1, Tarpaulin 0.37.2, and 95% lines. Ubuntu's runner version is explicit in the workflow.
- Actual collection is pinned rustup cargo tarpaulin: LLVM, bytehaul package, all targets, locked dependencies, ignore ambient Tarpaulin config, 95% fail-under, Stdout/Json/Html. Proxy variables are removed for collection because tests use localhost; installation retains normal networking configuration.
- Unique target/coverage/build/<run> and target/coverage/reports/<run> prevent profile/report reuse. Tools/revision/platform/architecture/command metadata is written before collection, then updated with outcome. Raw command output is flushed to run.log. Tools/test/gate nonzero exits remain nonzero.
- Human summaries distinguish failed tests (no complete measurement), actual threshold failure, and tooling/collection failure. A successful exit without a fresh JSON report is rejected. Actions always uploads available diagnostics, including failing runs.
- Windows uses its existing llvm-cov helper and adds shared --fail-under-lines95, fresh default report paths, and explicit Windows/scope labeling. Its percentage does not certify Ubuntu.

## Primary-source CLI verification

- https://github.com/xd009642/tarpaulin/blob/0.37.2/src/args.rs: --ignore-config, --locked, --target-dir, --output-dir, multiple --out formats, --fail-under.
- https://github.com/xd009642/tarpaulin/blob/0.37.2/src/lib.rs: report_coverage_with_check calls report_coverage before check_fail_threshold, so reports survive an insufficient percentage without suppressing the gate exit.
- https://github.com/xd009642/tarpaulin/blob/0.37.2/src/report/json.rs: JSON filename is tarpaulin-report.json.
- https://github.com/taiki-e/cargo-llvm-cov/blob/main/docs/cargo-llvm-cov-report.txt: report subcommand accepts --fail-under-lines.
- crates.io API confirmed Tarpaulin0.37.2 published/not yanked. Root confirmed Rust1.98.1 official distribution exists; missing Docker image tag did not justify changing the pin.

## Checks

`python3 -m unittest discover -s scripts/tests -p test_coverage.py`: seven passing subprocess-boundary regressions. Stubbed rustup verifies exact package/engine/target/threshold/config options and proxy isolation; tests assert success metadata, low coverage reports/exit, failed-test classification/101 exit, fresh-run isolation/missing report rejection, version mismatch, and installer exit7 propagation. `git diff --check` passed.

These script tests do not claim actual Rust coverage. Root owns Linux execution and records its measured result. Windows PowerShell execution requires Windows/PowerShell; it was not available in this macOS implementation environment.
