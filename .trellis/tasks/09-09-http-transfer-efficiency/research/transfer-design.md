# Research: bounded continuous requests and confirmed prefixes

- Query: Smallest robust integration for cross-piece HTTP requests, preserving piece completion, retry lineage, adaptive recovery and confirmed-prefix continuation.
- Scope: internal
- Date: 2026-09-09

## Findings

### Files and existing contracts

- `src/storage/segment.rs:3`: LeaseKey includes piece_id; Segment is one scheduler-owned subrange inside one piece. Do not extend its end across a piece boundary.
- `src/scheduler.rs:247`: assignment removes missing ranges and increments active lease count; `assign_subrange` at line 296 can reserve only within a piece. `complete` at line 329 is authoritative for piece bits; runtime partial completion already exists.
- `src/session/multi.rs:504`: Disabled has its own worker/retry loop. Default Adaptive dispatches into a separate loop, so changing only this path misses the default configuration.
- `src/session/multi.rs:724`: response validation binds start, end and total exactly to the request. A batch needs explicit request bounds independent from a piece lease.
- `src/session/multi/adaptive.rs:512`: slot ownership, lineage, observation registration and outcome handling currently all surround one segment/request.
- `src/session/multi/adaptive.rs:772`: monitoring concurrently polls primary and challenger. Recovery eligibility and budget use segment length; primary forwarding is line 924.
- `src/session/multi/adaptive.rs:422`: recovered lineage lookup keys piece and range; `completed` subtracts completed segment length. A multi-piece request cannot be represented as one current Recovered record.
- `src/storage/writer.rs:84`: BeginLease activates identity; Data remembers contiguous end offset even across cache drains. FlushLease at line 128 flushes and retires the identity. DiscardLease retires identity and drops cache only: it does NOT undo already-written disk bytes.
- `.trellis/spec/bytehaul-python/backend/transfer-storage.md`: flush acknowledgement before completion; snapshot before sync; incomplete dirty pieces must not become durable bits.
- `.trellis/spec/bytehaul-python/backend/slow-transfer.md`: independent fast-tail evidence, occupied-request-slot/observation correspondence, full replacement budget and shared recovery lineage.

### Recommended request representation

Keep Segment unchanged. Introduce an internal bounded request assignment containing an ordered small collection of contiguous Segments plus explicit HTTP start/end. Reserve all constituent leases atomically under the scheduler lock before issuing Range. Only adjacent wholly missing pieces may extend the first segment; stop at a completed piece, partial/touched piece, another owner, byte cap or piece-count cap. Both a byte cap and piece-count cap avoid allocating huge vectors for tiny configured pieces.

Use a conservative cap such as 4 MiB initially, but partition available work so early workers cannot reserve everything. Leave sufficient independent work for configured request slots; scheduler active_lease_count counts pieces, NOT request slots once batching exists, so existing assignment_range_for cannot be reused as a request concurrency estimate without adjustment. Pending recovered work should stay single-piece initially to preserve its lineage/splitting semantics.

Stream each incoming frame by splitting at min(piece end, memory chunk bound). Begin only the current writer lease. At each completed piece boundary: enter WriterBarrier phase, flush its lease, complete it under scheduler lock, notify waiters, then begin next lease. Keep any frame suffix locally across the barrier, bounded by the received frame; do not forward bytes under the wrong LeaseKey. Already completed leases must leave the assignment's remaining queue immediately and must never be reclaimed or progress-rolled-back on subsequent failure.

Validate HTTP headers against whole request bounds, and validate exact body length/EOF before committing the final lease. Earlier pieces may be committed while the body continues, but the last piece must remain incomplete until response framing has been checked; otherwise an overlong response can publish every completion bit before failure is detected. Existing exact probe response matching must remain exact: either consume matching one-piece probe first or decline probe reuse when batch bounds differ.

### Adaptive integration and the final-tail trap

Keep exactly one active observation per occupied request, never one per pre-reserved piece. Observation key may remain a stable request key until completion (even if its original piece completes) or move atomically to the current piece; baseline code only requires map membership and count, not scheduler liveness. Separate request-wide wire/time samples from current-lease forwarded bytes and remaining request bytes.

Do not blindly use original batch length for recovery. A 4 MiB request stalled only on its final 1 MiB would otherwise lose fast-tail eligibility and fail the 1% replacement budget despite three completed pieces. At every boundary, expose remaining request suffix as recovery work; preserve ordinary network history across healthy boundaries but reset fast eligibility evidence appropriately. Budget reservation must cover the actual full replacement suffix. If it contains multiple pieces, reclaim and attach the same SharedLineage to each remaining piece, adapting Recovered accounting to avoid subtracting one piece against another's range. An easier first implementation batches healthy requests but funnels recovery into existing individual piece operations after dropping the response.

Only hedge a remaining suffix <= 1 MiB that maps to a single current lease initially. Existing stage/copy path assumes one Segment. Hedge remains gated by strong validator and no conflicting conditional headers. Do not let a challenger start while a piece flush changes current authoritative state.

### Confirmed-prefix continuation

The forwarded counter counts queued bytes, not writer-confirmed bytes. After primary futures have been dropped, FlushLease acknowledgement can confirm all enqueued contiguous bytes and retire the old lease. Then scheduler needs a new atomic operation such as retain_prefix_and_renew(key, confirmed_end, worker), validating old start <= end <= old end, recording the prefix as runtime-completed, issuing a fresh identity for only the suffix, and updating active/missing counts. Do not call ordinary complete(old key), which would mark the whole old range done. A retain-prefix-and-reclaim variant can expose the suffix for splitting.

No new on-disk checkpoint format is necessary: partial bytes remain dirty and only whole completed pieces enter the synced bitset. After restart the existing format can conservatively redownload that partial piece. Avoid progress rollback for confirmed prefixes; roll back only discarded bytes. On writer failure there is no confirmation and no successful scheduler mutation.

Require a usable strong ETag with If-Match for prefix reuse, including ordinary retry and Adaptive modes; currently automatic with_validator is applied only to AdaptiveWithHedging. Missing/weak validators or user conditional conflicts should retain existing discard/full-range retry. Identity mismatch and 412 remain fatal. Preserve the same RetryState across suffix retries and the same recovery-action count across children. Avoid committing a prefix from a response known to violate range identity/framing.

### Focused validation

1. Scheduler contiguous batch does not cross complete/active/partial holes; byte/count caps and independent worker opportunities; stale leases and count invariants.
2. One body frame crosses multiple piece boundaries under tiny memory budget: exact output and progress.
3. Failure after one complete piece plus partial next piece: first stays complete; retry Range excludes it, same retry budget; suffix only after writer acknowledgement if prefix reuse enabled.
4. Pause/cancel during boundary flush and prefix handoff: no stale producer data, no unproven checkpoint bit, correct resume.
5. Incorrect Content-Range, changed validator, short and overlong bodies; final piece withheld until EOF validation.
6. Default tail benchmark with batching: slow final 1 MiB still triggers fast recovery and bounded duplicates; all-slow, speed-cap and missing-observation guards still apply.
7. Count actual TCP connections for pool tests and Range requests for batching tests. A Connection: close fixture cannot demonstrate pool reuse.

## Caveats / Not Found

- No external references were needed for this internal integration review; aria2 1.37.0 research is already in the parent conversation.
- Cross-piece batching touches both worker loops, monitoring identity and recovered accounting. It is not a safe one-line Range enlargement.
- Prefix reuse changes a published spec's discard/rollback recovery contract and needs explicit spec revision plus scheduler/writer tests.
- Recommendations are a design review, not verified implementation or measured speedup. Only the files above were inspected; no tests were run.

## Implemented engine interface

- `DownloadSpec.request_batch_size` is a byte cap, default 0. Scheduler `extend_batch`
  returns additional whole-piece leases only (64 total cap), preserving the first
  exact probe response instead of expanding/reissuing it.
- Adaptive engine retains `RequestStream { body, buffered, wire, consumed, expected }`
  between per-piece attempts under a single connection slot. The same observation
  moves between lease keys; completed request history is recorded once, at final
  completion. Per-piece `forwarded` resets but network wire/time sampling does not.
- Disabled with nonzero batch cap uses that engine with performance actions disabled;
  Disabled with zero cap retains its original worker/retry loop.
- On failed/reclaimed batch, cancel the body, release untouched reserved leases,
  and handle only the current piece through existing retry/recovery lineage. Already
  completed pieces stay completed. A fresh suffix attempt does not reset RetryState.
- `settle_prefix` retires writer access with existing FIFO `FlushLease`, then calls
  `SchedulerState::retain_prefix` for strict interior progress. Caller immediately
  renews/reclaims before further production. No new writer command/checkpoint format.
- Prefix retention applies to allowed retries or adaptive reclaim under strong
  validator, not malformed identity/framing, exhausted retries, stop, or staged
  hedge winner. Missing returned ETag is accepted on ordinary/adaptive primaries
  protected by If-Match; an explicitly different ETag or412 remains fatal. Existing
  hedge strict ETag policy remains.
- Discard diagnostics use unforwarded read-ahead plus discarded current-piece bytes;
  successful prior pieces and retained prefixes do not count as duplicate bytes.
- Integration coverage is in `src/session/multi/transfer_tests.rs`; existing default
  tail tests now require suffix takeover for Adaptive and full-range staged hedge.
