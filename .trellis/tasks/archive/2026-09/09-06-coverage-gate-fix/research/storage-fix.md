# Single-transfer storage failure repair

## Changes

Only `src/session/single.rs` changed by this implementer.

- `run_single_with_retry` now observes errors returned from writer start, retry flush, reset and close, publishing the matching terminal progress before preserving the original typed error. A writer IO error overriding a pause/cancel now also overrides its progress state to Failed.
- The existing successful-attempt finalization was extracted into private `complete_single_transfer`. It marks flush/close/deletion failures, verifies the durable byte count, and only then deletes the resume control file and publishes Completed.
- No terminal snapshot is written after a failed writer sync/close. Existing snapshots remain untouched.
- Two portable regressions inject an IO failure at the final flush barrier and after a successful flush during writer shutdown. Each exercises both absent and existing control files, asserts the exact original error, Failed state, cleared ETA, joined/closed writer and unchanged prior snapshot bytes (durable offset 1 versus received/written offset 4).
- The Linux `/dev/full` regression remains; its unsafe expectation of a 4-byte checkpoint was corrected to require no newly created checkpoint.

## Checks

- Native macOS proxy-isolated `cargo test -p bytehaul --lib session::single::tests`: 12 passed.
- Mutation verification: temporarily removed only finalizer error observers, retaining tests. Both new tests failed with `Downloading != Failed` (exit 101), then the fixed source was restored in a finally block.
- Restored fixed source, native macOS proxy-isolated `cargo test -p bytehaul --lib session::`: 104 passed.

## Boundaries and follow-ups

No commits, public APIs, storage formats or CI files changed. Root owns Linux `/dev/full` execution, measured coverage and remaining checks. Test helper exercises the production finalizer directly; existing successful end-to-end test and Linux device test cover its orchestration call site.
