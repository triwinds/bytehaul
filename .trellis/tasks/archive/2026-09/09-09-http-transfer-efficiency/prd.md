# Reduce HTTP request overhead and preserve recovery progress

## Goal

Reduce repeated connection/request overhead and preserve useful progress during
recovery, following the mechanisms researched in the preceding session. User
approved the proposed implementation order with “改吧” on 2026-09-09.

## Requirements

- R1: Measure existing idle pooling with controlled connection/request latency,
  exact bytes and connection/request counts; exercise TLS without changing
  system trust or weakening product certificate validation.
- R2: Support bounded contiguous HTTP requests spanning multiple pieces while
  preserving per-piece completion and durable checkpoint granularity.
- R3: Continue interrupted/adaptively reclaimed ranges from a writer-confirmed
  prefix when object identity is protected; retain retry and resource limits.
- R4: Preserve strict Range validation, single-stream fallback, explicit
  overrides, accurate progress, stop behavior and V1/V2 resume compatibility.
- R5: Document activation/default choices with measurements and limitations.

## Acceptance Criteria

- [ ] Reproducible CSV isolates pooling, request latency and connection failures.
- [ ] Tests observe fewer requests than pieces, exact output, boundary-crossing
  frames, completed pieces on mid-request failure and resumed holes.
- [ ] Tests observe suffix-only retry/recovery, reject changed objects/stale
  writes and verify progress and pause/cancel safety.
- [ ] Full Rust tests, doctest, workspace Clippy and strict rustdoc pass.
- [ ] Relevant option mirrors/docs agree; no unsupported WAN/coverage claims.

## Notes

Scope is single-source known-size HTTP Range downloading, fixtures and docs.
Multiple mirrors, dynamic file concurrency, new protocols, publishing and
checkpoint-format changes are excluded.
