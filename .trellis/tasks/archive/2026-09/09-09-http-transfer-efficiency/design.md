# Design

Network/client configuration owns pooling, scheduler owns disjoint leases,
multi-worker orchestration owns HTTP grouping, writer owns confirmation.

Do not enlarge a single-piece Segment across pieces. Reserve a bounded
contiguous batch of leases under the scheduler lock and slice response frames
at lease boundaries. Flush and complete individual leases, leaving the final
lease incomplete until response EOF validation. Retry only uncompleted ranges.

Keep one adaptive observation per active request (not reserved piece). Preserve
the occupied-slot correspondence guard and use the current remaining range for
tail benefit/eligibility. Keep strong validator requirements and bounded hedges.

For prefix handoff, stop the producer, await a FIFO writer confirmation and
revoke the old lease before renewing/reclaiming only the suffix. Runtime prefix
progress does not introduce new durable bits; partial pieces remain incomplete
across process restarts. Preserve retry counters/elapsed time and exact progress.

Expected owners: scheduler.rs, storage/writer.rs, session/mod.rs,
session/multi.rs and adaptive.rs. Configuration files only for deliberate option
or default changes. Fixtures belong in examples/tests; final contracts in specs.
No system trust-store edits or copying aria2 code.

Final activation: `request_batch_size` is an opt-in u64 byte cap, default zero,
plus an internal 64-lease cap. Idle pooling retains its existing zero default.
The retained response is consumed across per-piece attempt loops, with a single
request observation and separate current-piece forwarded count. Complete
requests contribute one historical sample; completed batch bytes never count as
duplicate traffic when a later piece is recovered. Request-aware splitting must
use occupied request slots instead of reserved lease count.

Strong ETag automatically adds If-Match for protected primary requests. A
compliant successful primary can omit a repeated ETag; a positively different
ETag or 412 is fatal. Hedging retains the earlier stricter response ETag rule.
This follows [RFC 9110 If-Match](https://www.rfc-editor.org/rfc/rfc9110.html#section-13.1.1).

TLS validation uses private network clients with generated local trust roots;
it is correctness coverage, not public Downloader HTTPS performance measurement.
