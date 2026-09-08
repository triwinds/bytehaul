# Independent review

First pass, 2026-09-09. Implementation and tests are still being written. No code
mutations or cargo commands were run, per root coordination request.

## Findings resolved in final source delta

1. `src/session/multi/adaptive.rs`, `Outcome::Staged`: hedge winner accounting adds
   `observation.wire`, now cumulative across a batched request. Previously completed
   pieces incorrectly become duplicate bytes when its final-piece hedge wins.
   Count only discarded primary bytes; include explicitly tracked discarded
   read-ahead if diagnostic counters promise all received duplication.
2. `finish_observation`: successful intermediate piece boundaries append the
   cumulative request average repeatedly while reusing the same Observation.
   One request can manufacture the two healthy reference samples required by
   baseline selection and overweight independent peers up to 64 times. Publish
   history once per completed request, or deduplicate by request identity.
3. `SchedulerState::assignment_range_for` still uses active piece leases as
   request concurrency. One request reserving three remaining pieces plus one
   available large piece suppresses splitting that available piece even with
   three idle HTTP slots. Consider request-aware assignment while preserving
   the existing normal single-piece scheduler API.

All three fixes are confirmed in the final source delta: intermediate boundaries
remove/rekey without publishing history; RequestStream tracks consumed bytes and
only discarded read-ahead/current bytes enter hedge duplication; adaptive
assignment invokes the request-aware scheduler helper with occupied slots minus
the seeking request. Scheduler tests verify the concrete three-reserved-leases
case and minimum segment limits. Fixes were made by the active owners.

## Final delta test requests — resolved

- The new m12 batch parameter alone does not prove slow detection within a
  continuous body: its fixture gates only requests beginning in the slow piece,
  while a request spanning that boundary is sent fast. Add overlap-aware prefix
  forwarding and prove the original Range starts before the slow piece.
- Existing public pause/cancel tests use default batch zero. Parameterize a
  grouped public stop/resume path; helper task abort coverage is useful but does
  not exercise public StopSignal handling across multiple reserved leases.

Confirmed final test changes: m12 gates overlapping ranges, forwards their
healthy prefix before dripping the slow constituent, and asserts original.start
is before the slow piece with original.end exactly at its end. Both recovery
modes verify suffix/full-hedge replacement and exact final output. m11 now
observes a Range larger than its 16 KiB piece before invoking public pause or
cancel, then resumes and checks exact bytes. Repeated suffix failures explicitly
assert two requests with max_retries=1 across all modes and grouping on/off.
No unresolved test-coverage finding remains from this review.

## Inspected invariants

- Prefix handoff drops the primary future/body before FIFO writer flush, records
  only runtime prefix state, then renews/reclaims the suffix. Whole-piece durable
  bits remain incomplete until all missing/active subranges finish.
- Header validation uses the original whole-request end; final lease completion
  follows EOF/length validation. Intermediate completed pieces survive failures.
- Adaptive register/remove still brackets each writer setup/barrier and retains
  the occupied-slot correspondence guard, suppressing fast detection in gaps.
- Rate-limit, memory and channel waits retain outer stop/writer-close selection.
- Strong validator conditional reuse rejects positively changed ETags and 412;
  ordinary missing response ETag remains allowed under If-Match, unlike strict
  hedging. This is a deliberate compatibility distinction to document/test.
- Rust builder/getter and both Python entry points expose the same opt-in byte
  cap; default remains zero, independent of experimental idle pooling.

## Final source verification

Reviewed transfer_tests, prefix_tests and TLS tests. They cover byte-exact grouped
frames under budgets 1/19, resumed holes, suffix-only retry across all modes and
batch on/off, repeated suffix failures sharing retry limits, 412 identity change,
durable first-piece preservation, and withholding the malformed response's final
piece until EOF. FIFO acknowledgement is gated for confirmation/failure/abort.
TLS tests validate real one/two connection counts and typed UnknownIssuer with
private test roots. Python option reaches both APIs with observable grouped
Range assertions. Continuous-request spec and Rust/Python default agree.

No unresolved source correctness defect found in this delta. Recovered attempts
remain unbatched, so record lookup/subtraction cannot cross pieces; initial batch
lineage has no recovered record until its current piece is actually reclaimed.
RetryState remains shared through current-piece suffix renewal. Dropped queued
leases have never contributed effective progress.

Source/test review signoff: no unresolved findings. Root reported focused m11,
m12 (8 tests) and multi-worker tests passing; final full checks remain root-owned.
Root owns final Rust/Python/lint/doc checks. No cargo
commands were run by this reviewer to avoid interference with measurements; this
review claims no independently executed test result or coverage measurement.
