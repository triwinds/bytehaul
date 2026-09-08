# Transfer flow control and durable checkpoints

## 1. Scope / Trigger

Read this contract before changing worker-to-writer flow, cache accounting, scheduler lease state, or multi-worker checkpoint persistence. The 2026-09-06 review identified circular budget waits and a snapshot-after-sync race.

## 2. Signatures

- `session::flow::MemoryBudget::new(bytes)` sets the semaphore, maximum forwarded chunk and cache watermark together.
- `MemoryBudget::forward(data, offset, lease_key, write_tx, cancel_rx, speed_limit, sent)` forwards bounded chunks; `sent` is called only for enqueued bytes, including partial progress on interruption.
- `WriterCommand::{BeginLease, Data, FlushLease, DiscardLease, FlushAll}` carries data and FIFO acknowledgements.
- `persist_multi_control_snapshot(reason, write_tx, tracker, ctx)` freezes completed state before awaiting the writer barrier.
- `SchedulerState::{assign_to_with_split, renew, complete, reclaim, control_hints}` owns lease identities and sparse runtime state.

## 3. Contracts

- Maximum forwarded chunk is `min(ceil(budget / 2), u32::MAX)` and flush watermark is `max(floor(budget / 2), 1)`. Below the watermark enough budget remains for a maximum chunk. Do not change one without the other.
- The semaphore counts queued/cached payload bytes, not total process memory or HTTP/TLS receive buffers. Account only actual enqueues. Failed or cancelled acquisition/reservation returns its permits; the writer returns permits for written, discarded or stale data.
- Rate-limit, budget and channel waits observe stop signals and writer closure. Dropping a handle alone does not cancel a transfer.
- Cache data is isolated by lease identity and contiguous within each buffered entry. The writer retains the next expected offset for active leases across drains, so gaps/overlaps must not be silently accepted even after a watermark flush. Bulk drains sort by file offset, not lease ID. Flushed bytes remain dirty until the full piece is completed and captured by a synced checkpoint.
- A stopped attempt may flush a protected contiguous prefix and retain it in runtime scheduler state via `retain_prefix`, then renew/reclaim only its suffix. FlushLease retires writer access and confirms enqueued bytes, not whole-piece completion or fsync. No producer may enqueue through the old identity afterward; incomplete pieces remain incomplete across resume. See continuous-requests.md.
- Worker completion follows its lease flush acknowledgement. A multi-worker checkpoint freezes bitset and hints under the scheduler lock, releases the lock, awaits `FlushAll(sync_data = true)`, then writes the frozen snapshot atomically. It must not reread completion state after that barrier.
- A terminal snapshot without a live writer is allowed only after successful final writer sync. Writer failures must not publish unproven completion state.
- Detailed scheduler state is allocated for touched incomplete pieces and removed on full completion. Active/missing counts update with each transition; diagnostic hints inspect sparse state. Completed bits remain authoritative on resume. V1/V2 file compatibility stays intact.
- HTTP connector resolution uses Hickory's bounded TTL cache. Do not add a parallel unbounded DNS-answer map or serial preflight DNS lookup outside the request lifecycle.

## 4. Validation & Error Matrix

| Condition | Behavior |
| --- | --- |
| Tiny/non-divisible positive budget | Split forwarded chunks; finish with exact output bytes |
| Pause/cancel while blocked in forwarding | Prompt typed `Paused`/`Cancelled`, preserve valid durable resume state |
| Writer channel closes while budget is exhausted | `ChannelClosed`, without waiting forever for permits |
| Gap/overlap in buffered lease | Internal error; no silent overlapping-byte acceptance |
| Stale lease data | Discard and return budget; do not affect current lease |
| Piece completes after snapshot capture | Exclude from that checkpoint, retain for next checkpoint |
| Flush/sync fails | Do not replace control file with unproven state |

## 5. Good / Base / Bad Cases

- Good: budget 100, chunk bound 50, watermark 50; the writer flushes and releases permits even when server frames are much larger.
- Base: budget 1 forwards and writes one byte at a time; inefficient but makes progress.
- Bad: budget and watermark both 100, cached bytes 60 and next chunk 60; producer and writer wait on one another.
- Bad: sync completes, a new lease flush completes, then a newly read bitset includes that lease without a later sync.

## 6. Tests Required

- `tests/m11_flow_control.rs`: exact bytes for single/multi budgets 1, 100 and 4097 with a one-slot channel and autosave disabled; prompt pause/cancel during a limiter wait; successful subsequent resume.
- Flow unit tests: pause and writer closure while permits or channel capacity are unavailable, partial enqueue byte accounting, and no leaked permits.
- Multi checkpoint tests: inject completion while the flush acknowledgement is pending; saved bitset must equal the pre-barrier snapshot. Flush failure must leave the previous checkpoint unchanged.
- Scheduler tests: renewal/reclaim do not drift incremental counts; completed states are released; sparse hints remain correct for partial pieces; stale leases never complete a piece.
- Cache tests: contiguous bytes preserved, gap/overlap rejected without mutation, renewed leases sorted by offset, discard isolation and stale permit return.

## 7. Wrong vs Correct

Wrong:

```rust
flush_all_and_wait(write_tx, true).await?;
let bits = scheduler.lock().snapshot_bitset();
```

Correct:

```rust
let bits = scheduler.lock().snapshot_bitset();
flush_all_and_wait(write_tx, true).await?;
// Persist these captured bits; later completions belong to the next checkpoint.
```

Wrong: acquiring an entire unbounded body frame before forwarding it. Correct: use the shared budget geometry and cancellable forwarding helper for both transfer modes.
