# Independent adaptive engine review

Scope: read-only review of `src/session/multi/adaptive.rs`, its `multi.rs` integration, scheduler ownership, HTTP error construction and stop handling. Core edits remain with the engine agent. Line numbers refer to the reviewed work-in-progress source and may move. No broad builds or runtime tests performed in this audit.

## Findings requiring engine fixes

### P1 — Writer closure can leave detached idle workers alive

- Evidence: `multi.rs:256-263` intentionally excludes `ChannelClosed` from `download_error`, but now breaks immediately for any adaptive worker error. The subsequent worker abort/drain at `multi.rs:283` is conditional on `download_error.is_some()`.
- Scenario: at the download tail, several adaptive workers are idle; writer failure closes its channel; the active worker returns `ChannelClosed`; the supervisor exits its loop without aborting/draining the remaining workers. Dropping their `JoinHandle`s detaches them. Idle workers at `adaptive.rs:239-240` wait only for coordinator notification or stop. A dropped stop sender does not terminate them (`flow.rs:78`, `wait_for_stop` deliberately becomes pending).
- Required correction: abort and drain remaining workers after any adaptive worker failure, while still allowing the writer's underlying storage error to remain the public failure. Alternatively enforce unconditional drain at the supervisor's terminal boundary.
- Regression: induce writer failure with idle workers and assert every worker terminates before the session returns.

### P2 — Malformed challenger responses are mistaken for object version changes

- Evidence: `adaptive.rs:346` considers every `ResumeMismatch` fatal to the primary. `checked_response` calls `validate_segment_response` at `adaptive.rs:392`, which uses that same error for wrong Content-Range/encoding. `stage` also returns `ResumeMismatch` for an overlong challenger body at `adaptive.rs:470`.
- Scenario: primary has a valid response and continues progressing; challenger returns a malformed range or excess payload with the unchanged ETag. The candidate cannot commit, but the current handler aborts the entire download rather than allowing the healthy primary to finish.
- Required correction: distinguish actual object validator changes/412 from candidate protocol failures. Discard malformed candidates and preserve the primary; fail the session for established identity changes.
- Regression: valid primary plus wrong-range, encoded or overlong challenger; assert only primary completes and no candidate bytes become authoritative.

## Additional delivery gap

- `Coordinator::{wire, duplicate}` (`adaptive.rs:111-112`) are updated but never loaded, logged or surfaced. The per-attempt debug log at `adaptive.rs:355-360` reports only primary counters. Consequently the planned separate actual duplicate-byte diagnostic is currently unavailable, including on challenger failure/cancellation. Emit a terminal structured summary or equivalent internal diagnostic, preserving the existing public progress API. This is observability work, not evidence of incorrect file contents.

## Reviewed invariants with no additional defect established

- Worker assignment and reclaim use scheduler-then-coordinator lock ordering; recovered ranges are queued before scheduler unlock, so ordinary assignment cannot steal the reclaim between those operations.
- Recovered children find the original shared lineage; retries and recovery counts are not freshly allocated per child. Completed child lengths retire the record only after all original bytes complete.
- `run_attempt` owns both producers. Returning drops the losing producer before the caller sends the FIFO discard barrier; a staged winner then renews the lease, copies through the shared memory budget, and awaits flush before scheduler completion.
- Existing `HttpWorker::send_range` constructs 429/503 errors with Retry-After before `checked_response`; its passing `None` to range validation does not itself lose the header.
- The recently fixed `blocked_since` reset and slot release-before-notify paths were reviewed in their updated form and are not findings.

## Verification status

Read-only source audit only. Root/engine agents own runtime regressions, full lint/type checking and the final quality gate. Findings were sent to root before this document was written.

## Final review closure

All findings resolved, confirmed by independent reviewer: producer abort/drain, optional candidate error classification, ETag check ordering, diagnostic publication, Retry-After pending queue gating and captured-deadline wakeup. The last regression uses paused Tokio time to prove wakeup without notification after deadline expiration. Final static review reports no blocking issues.
