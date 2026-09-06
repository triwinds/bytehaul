# Journal - triwinds (Part 1)

> AI development session journal
> Started: 2026-08-26

---


## Session 1: Bootstrap Trellis project guidelines

**Date**: 2026-08-26
**Task**: Bootstrap Trellis project guidelines
**Package**: bytehaul-python
**Branch**: `fix/retry-body-transport-errors`

### Summary

Initialized Trellis tooling and project-specific Rust/Python guidelines, formatted the workspace, and verified Rust plus Python quality gates.

### Git Commits

| Hash | Message |
|------|---------|
| `7d78e6a` | (see git log) |
| `8455075` | (see git log) |

### Status

[OK] **Completed**


## Session 2: Single-connection retry

**Date**: 2026-08-26
**Task**: Single-connection retry
**Package**: bytehaul-python
**Branch**: `fix/retry-body-transport-errors`

### Summary

统一单连接与多连接重试预算及等抖动退避；单连接响应体中断时安全续传或从零重启；补充测试、文档和错误可观测性规范。

### Git Commits

| Hash | Message |
|------|---------|
| `4517569` | (see git log) |
| `b1e2901` | (see git log) |

### Status

[OK] **Completed**


## Session 3: Simplify download internals and preserve checkpoint durability

**Date**: 2026-09-06
**Task**: Simplify download internals and preserve checkpoint durability
**Package**: bytehaul-python
**Branch**: `fix/retry-body-transport-errors`

### Summary

Committed aria2 review fixes: bounded cancellable forwarding, frozen pre-sync checkpoints, sparse scheduler, sequential lease cache, shared clients and Hickory DNS cache. 411 Rust tests, 42 Python tests, Clippy and docs passed; pooling remains opt-in.

### Git Commits

| Hash | Message |
|------|---------|
| `f483f37` | (see git log) |

### Status

[OK] **Completed**


## Session 4: Diagnose local and CI coverage mismatch

**Date**: 2026-09-06
**Task**: Diagnose local and CI coverage mismatch
**Package**: bytehaul-python
**Branch**: `fix/retry-body-transport-errors`

### Summary

Committed aria2 simplification. Verified August Actions coverage 94.75% (2975/3140; 8 lines short); September coverage aborted on Linux-only /dev/full progress-state assertion, also failing ordinary Ubuntu tests. Windows helper has no coverage threshold; local macOS verification did not measure coverage. Recorded exact evidence and scoped follow-up in archived coverage-parity research. No push.

### Git Commits

| Hash | Message |
|------|---------|
| `f483f37` | (see git log) |

### Status

[OK] **Completed**


## Session 5: Fix storage failures and reproducible Linux coverage

**Date**: 2026-09-06
**Task**: Fix storage failures and reproducible Linux coverage
**Package**: bytehaul-python
**Branch**: `fix/retry-body-transport-errors`

### Summary

Fixed terminal storage error progress while preserving durable checkpoints. Unified pinned Linux coverage entry, added meaningful measured-gap regressions, isolated profiles and retained CI diagnostics. Fresh Linux gate passed 96.59% (3404/3524), 434 tests; native 429 tests and quality checks passed. Documented platform limits and prevention contracts; cleaned task-owned Docker/Colima resources. No push.

### Git Commits

| Hash | Message |
|------|---------|
| `3d21ed0` | (see git log) |

### Status

[OK] **Completed**


## Session 6: Replace flaky dynamic split timing assertion

**Date**: 2026-09-06
**Task**: Replace flaky dynamic split timing assertion
**Package**: bytehaul-python
**Branch**: `fix/retry-body-transport-errors`

### Summary

Windows CI failed a 150ms speedup assertion despite a 146ms improvement. Replaced elapsed-time comparison with gated exact subrange arrivals, patterned output and Completed progress. Ten focused repetitions passed; serial configuration mutation failed as expected; restored test, full429 native tests, benchmarks, Clippy and formatting passed. Captured deterministic concurrency testing rule. Push and remote Actions verification follow in the task conversation.

### Git Commits

| Hash | Message |
|------|---------|
| `d25f8f3` | (see git log) |

### Status

[OK] **Completed**


## Session 7: Merge repair branch and publish 0.2.1

**Date**: 2026-09-07
**Task**: Merge repair branch and publish 0.2.1
**Package**: bytehaul-python
**Branch**: `master`

### Summary

Reviewed five dependency PRs: closed incompatible bincode/warp/PyO3 bumps and superseded libc; incorporated rand 0.9.4. Prepared shared 0.2.1 versions and excluded internal workflow files from package. Native 429 Rust and 42 Python tests plus Clippy/docs/package audits passed. PR20 merged to existing master after both CI runs passed, coverage 96.59% (3404/3524). Tagged merge commit and created GitHub release; crates.io and PyPI workflows succeeded and registry APIs verified version 0.2.1 with four wheels and one sdist.

### Git Commits

| Hash | Message |
|------|---------|
| `654f783` | (see git log) |
| `5ebda09` | (see git log) |

### Status

[OK] **Completed**


## Session 8: Synchronize public guides for 0.2.1

**Date**: 2026-09-07
**Task**: Synchronize public guides for 0.2.1
**Package**: bytehaul-python
**Branch**: `master`

### Summary

Updated README and English/Chinese guides for 0.2.1 installation, master badges and release links. Corrected received vs durable progress, storage/cleanup, pause/cancel, Python lifecycle, fresh split strategy and Windows coverage descriptions; marked historical design/benchmarks. Independent docs checks passed: 40 local links, 2 anchors, 25 Python/21 Rust/2 TOML/30 shell snippets. No runtime change; no full runtime test rerun.

### Git Commits

| Hash | Message |
|------|---------|
| `HEAD~1` | (see git log) |

### Status

[OK] **Completed**
