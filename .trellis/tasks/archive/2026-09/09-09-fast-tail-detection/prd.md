# Fast tail detection

## Goal
Reduce avoidable slow-tail waiting in known-size multi-connection Range downloads. The user approved implementation after the report analysis and prioritized recommendation.

## Requirements
- R1: Use short sustained network evidence to recover a small trailing range when useful work is exhausted, a connection slot is idle, and recent healthy evidence supports a faster replacement.
- R2: Preserve ordinary-stage configured detection behavior, Disabled mode, user speed-cap suppression, collective-slowdown protection, benefit estimation, recovery budget, cooldown, shared retry lineage, validator rules and authoritative writer ownership.
- R3: Suppress accelerated recovery for missing/stale healthy evidence, immature or blocked live peers, transient dips and local backpressure. Tail evidence must reset when eligibility disappears.
- R4: Keep public option signatures/default values unchanged; document the accelerated tail exception.

## Acceptance
- Controlled observation tests distinguish fast tail recovery from the ordinary 5-second window plus 15-second sustained gate, and cover guard conditions/reset behavior.
- Public tests prove actual replacement arrival with defaults and exact output/progress, for Adaptive and Hedging, without CI performance-ratio assertions.
- Reproducible loopback comparison uses this workspace crate; retain CSV and timing results without WAN claims.
- Appropriate Rust tests, Clippy and docs pass.

## Out of scope
Prefix continuation, storage/checkpoint changes, small-file budget changes, cross-piece HTTP ranges and releases.
