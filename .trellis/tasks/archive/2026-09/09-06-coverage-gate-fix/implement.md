# Execution

1. Implement owner A: src/session/single.rs and its tests, storage-error contract notes only if required.
2. Implement owner B: scripts coverage entry/config, Windows helper, workflow and README variants. Verify script failure/exit/report behavior with stubs and real tool CLI.
3. Root: provision isolated Linux build/test environment, run focused regression then full coverage and add meaningful tests for measured gaps. Update specs and findings.
4. Independent check: inspect final diff and regression strength, run relevant functional/clippy/doc checks. Record exact tool/revision/platform/metric/outcome.

## Completed implementation and review
- Single finalization/storage error publication fixed; portable injected flush/close failure and Linux device regression preserve durable checkpoint semantics.
- Shared pinned Linux entry, script regression tests, Windows explicit threshold and CI diagnostics implemented.
- Measured93.33% baseline drove targeted single/multi transfer failure, range-validation and redirect-cache regressions; no production coverage exclusions.
- Manager cache fixtures now fail immediately and deterministically on local403.
- Independent review corrected frame-boundary assumptions; final429 native tests, Clippy, formatting,7 script tests pass.
- Root owns final shared Linux measurement and resource cleanup; validation.md records each distinct infrastructure attempt and final result.
