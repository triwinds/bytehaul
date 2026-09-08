# Continuous Range requests and prefix handoff

## 1. Scope / Trigger

Read when changing multi-piece HTTP grouping, partial retry/recovery, request
observation lifetime or the `request_batch_size` Rust/Python option.

## 2. Signatures

- `DownloadSpec::{request_batch_size, get_request_batch_size}` use u64 bytes;
  default zero disables grouping. Python appends `request_batch_size=None` to
  both download functions and translates through `build_download_spec`.
- `SchedulerState::extend_batch(first, worker_id, max_connections, byte_cap)`
  returns additional contiguous piece leases, excluding the first lease.
- `SchedulerState::retain_prefix(key, confirmed_end)` shrinks the active range;
  the caller must retire writer ownership first, then renew or reclaim it.
- `multi::settle_prefix` uses the FIFO `FlushLease` acknowledgement for retained
  bytes or `DiscardLease` for replay. Never confirm from a wire counter alone.

## 3. Contracts

- Segment and LeaseKey remain single-piece identities. Request grouping reserves
  several distinct leases under one scheduler lock. Bound by bytes and at most
  64 leases, stop at completed/active/touched pieces, leave independent work for
  other workers. A cap below a piece does not split that piece.
- Split received frames at lease boundaries and route through existing bounded
  forwarding. Flush/complete each piece before moving to its next lease. The
  final piece additionally requires response EOF/length validation.
- Preserve one slot and one observation per live request, including across
  piece boundaries. Reserved leases are not additional requests. Network sample
  lifetime and current-piece forwarded progress have different lifetimes.
- Errors release uncompleted reserved leases and drop the old body before retry
  or recovery. Completed pieces are never replayed or progress-rolled-back by a
  later piece's error. Ordinary retry limits and recovery lineage remain bounded.
- Only strong ETag with compatible conditional headers enables automatic
  If-Match/prefix reuse. Explicit validator changes and 412 are fatal. Do not
  retain data from an identity/framing-invalid response.
- Stop producer first; confirm enqueued contiguous bytes with FlushLease; shrink
  scheduler range; renew or reclaim suffix. Never enqueue more with retired key.
  Runtime partial completion does not create a whole-piece durable bit. V1/V2
  checkpoints continue to replay incomplete pieces after restart.
- Pooling and grouping are independent, opt-in controls. Connection: close
  prevents reuse. TLS tests use private scoped roots, never system trust edits.

## 4. Validation & Error Matrix

| Event | Required behavior |
| --- | --- |
| Zero batch bytes | No grouped requests |
| Huge cap / tiny pieces | At most 64 leases per request |
| Completed/active/partial hole | Stop aggregation before hole |
| Frame crosses piece end | Split payload; confirm current lease first |
| Late request truncation | Preserve completed pieces; retry remaining work |
| Confirmed partial prefix | New request starts at suffix, no double progress |
| Missing/weak validator / conflicting condition | Whole-range replay |
| Writer failure / stale lease | No confirmed prefix or new completion bit |
| Pause/cancel | Stop producers and preserve checkpoint barrier ordering |

## 5. Good / Base / Bad Cases

Good: 1 MiB pieces in a 4 MiB request complete separately; a failed third piece
does not replay the first two. Base: cap zero retains single-lease requests.
Bad: extending one Segment.end beyond its piece or marking all leases complete
before validating response length.

## 6. Tests Required

Scheduler tests cover continuity, byte/count caps, peer work, prefix lifecycle,
stale keys and counters. Public integration tests verify exact requested ranges,
bytes, progress, interruption, identity, body boundaries and resumed holes.
Retain default fast-tail regressions when grouped requests reach their last
piece. Python tests cover appended signature and both APIs. TLS tests prove
actual connection count and UnknownIssuer without weakening production trust.
Diagnostic CSV separates wall-clock measurements from CI behavior assertions.

## 7. Wrong vs Correct

Wrong: use all bytes ever read on a multi-piece request as current lease progress.
Correct: retain cumulative speed history but reset per-piece forwarded count.

Wrong: scheduler.complete(old_key) after flushing a partial attempt.
Correct: retain_prefix(old_key, confirmed_end), then renew/reclaim suffix; mark a
piece complete only when all its ranges have actually completed.
