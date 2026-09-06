# Bug analysis: storage failures and coverage parity

## 1. Root cause category
- Cross-layer contract: writer errors returned correctly but terminal progress publication was skipped by early `?` returns.
- Test coverage gap: device-based regression was Linux-only, while local validation ran macOS.
- Implicit assumption: report generation success and functional tests were confused with a coverage threshold gate; tools/platforms/scopes differed.

## 2. Why earlier validation missed it
Earlier native functional checks did not execute the Linux-only branch and did not measure coverage. The first fix correctly published Failed, but its portable fixture started with downloaded=4 and therefore missed throttled stale progress. Actual Linux execution exposed downloaded=0 after EOF; production finalization now publishes the known received offset before the storage barrier, and portable fixtures start at0. CI previously discarded detailed artifacts and floated tool versions, making later comparisons incomplete.

## 3. Prevention mechanisms
- Runtime: finalization owns sync + close + checkpoint deletion + Completed ordering; storage error observers publish Failed with original typed error.
- Tests: portable deterministic flush/close failures assert checkpoint bytes unchanged. Mutation check removes only error observers and must fail; Linux device regression remains.
- Automation: shared pinned Linux entry enforces95%, isolates run data, preserves nonzero exit and emits distinct failure classification.
- Evidence: version/revision/platform/scope/metric and JSON/HTML/log preserved even when threshold fails.

## 4. Systematic expansion
Nearby start/reset/retry-flush/close errors were audited; no new general storage abstraction or format change. Windows gate gets an explicit line threshold but cannot claim Linux equivalence. Native tests and Linux coverage remain separate required evidence.

## 5. Knowledge capture
Contracts added to errors-and-observability.md and testing-and-quality.md. This repository has no Trellis template package to synchronize. Final measured outcomes belong in validation.md after Linux execution.

## 6. Measured gaps and test stability
The first complete current Linux report was93.33%, not the old historical94.75%. Use its uncovered source lines to select observable failure-path regressions: retry/stop, body bounds, durable checkpoint failure and redirect cache expiry. Independent review caught two network fixtures assuming a TCP write maps to one body frame; those now aggregate chunks and retain exact data/offset/lease assertions. The testing spec captures that rule alongside ephemeral fixtures. Native success remains separate from the fresh Linux threshold result.

Final fresh Linux measurement reached96.59% (3404/3524), +115 covered lines with denominator unchanged. Preserve this complete gate/report evidence when claiming success; future code/tool/platform changes still require their own measurement.
