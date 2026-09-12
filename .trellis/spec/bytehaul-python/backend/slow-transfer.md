# Adaptive slow-transfer recovery

## 1. Scope / Trigger

Read before changing `src/session/multi/adaptive.rs`, Range scheduling, staged challengers, or the Rust/Python policy surface. Applies only to known-size multi-connection Range downloads. Single-stream and Range fallback keep their existing behavior.

## 2. Signatures

`SlowTransferMode::{Disabled, Adaptive, AdaptiveWithHedging}` is exported from the crate. `DownloadSpec` setters/getters: `slow_transfer_mode`, `low_speed_limit` (optional u64 bytes/sec), `low_speed_duration`, `slow_start_grace`, `slow_sample_window` (Duration).

Python `download` and `Downloader.download` (`PyDownloader::download`) append the five matching optional arguments after all existing positional parameters. Mode strings are case-insensitive `disabled`, `adaptive`, `adaptive_with_hedging`. Shared `build_download_spec` applies them; do not add download policy to `PyDownloader` construction.

## 3. Contracts

- Default Adaptive; no absolute floor; duration/grace/window defaults 15/5/5 seconds. Hedging requires explicit opt-in.
- Sample network reading time, excluding headers, rate limiting, memory/channel waits and writer barriers. Clear stale slow evidence after a blocked sampling window. Suppress performance actions when an explicit nonzero download speed limit is configured.
- Compare sustained speed against healthy peer/history samples with grace, benefit estimation and all-slow suppression. Do not replace ordinary timeout/retry policy.
- Small trailing ranges (at most 1 MiB) have independent fast samples and sustained evidence. Require no scheduler-available or pending recovered work and a spare slot. Cap configured window/grace/duration at 1/1/2 seconds; require a recent healthy relative baseline, never an absolute floor alone. For this path, the target must be registered and occupied slots must match registered observations (including setup/backoff gaps); every live peer must have a mature reading sample. Preserve collective-slowdown suppression. Reset fast sustained evidence when eligibility/baseline disappears; clear short samples after a short-window local block without clearing ordinary samples early. Ordinary detection retains its configured thresholds. `fast_tail` in recovery logs identifies fast-path eligibility at selection.
- Performance budget remains `min(total / 100, 16 MiB)`, with ten-second global cooldown and at most two actions per shared lineage. For cancel-before-resume protected by a strong validator, reserve application-consumed read-ahead (`wire_bytes - cumulative_enqueued_bytes`); the necessary suffix is not extra traffic. Without safe prefix retention, also reserve the replayed range. Snapshot and drop the producer synchronously before another body poll. Hedge still reserves its full range and requires exact Content-Length without Transfer-Encoding before staging.
- `reserved_extra_bytes` is in-flight and `consumed_extra_bytes` is monotonic. A reservation guard settles exactly once on success, failure, cancellation or writer error. On confirmed prefix handoff, charge read-ahead plus forwarded bytes not retained; otherwise charge the conservative bound. Hedge settlement charges losing staged bytes or discarded primary bytes after writer acknowledgement. Zero-cost actions still consume count/cooldown. These are application body accounting bounds, excluding unread Hyper/socket buffers and origin bytes already transmitted.
- Fast cancel-before-resume can act within a batch, with the same exhausted-work, spare-slot, healthy-history and all-slow guards. Only hedge requires the final batch piece. Release queued leases after producer stop and FIFO prefix acknowledgement; preserve shared ordinary retry count, elapsed budget, Retry-After and performance lineage for all released pieces. Keep speed history across pieces of the same request; a replacement starts fresh request observations.
- Stop/drop producers before FIFO writer acknowledgement, then reclaim or renew the lease. With protected strong ETag and a nonempty proper prefix, flush and retain that prefix in scheduler runtime state before transferring only the suffix; otherwise discard and roll back ineffective progress. Never reuse a retired writer identity. Only whole completed, flushed pieces enter durable state. Preserve checkpoint capture-before-sync; see continuous-requests.md for handoff details.
- Hedge only a final range no larger than 1 MiB, with a spare slot, strong ETag and no conflicting user conditional headers. At most one challenger; primary plus challenger stay within max_connections. Stage the challenger to a temporary file; only the winner writes the authoritative file, through the existing bounded writer path. Temporary data and losing futures must be released on every exit.
- If a full-range hedge reservation fails specifically for budget, release its tentative spare permit and try protected cancel/resume under its own conservative cost bound. Recheck every reservation guard; other refusal reasons do not trigger fallback. Record `hedge_budget` separately and count only the eventual successful action. If both costs exceed budget, issue neither request and change no action/lineage/cost counters.
- Optional malformed/transient challenger failures leave the primary alive. Actual identity changes and 412 fail the download. Retry-After blocks further performance actions. No duplicated progress; wire/duplicate/reserved-budget counters are diagnostics.
- Request diagnostics use `RequestDiagnostics` response extensions: one ID per client invocation/redirect hop, retained across batch pieces. Hyper-internal transparent resends are not separately counted. HTTP counter summaries are per worker clone group (probe and multi workers may emit separate summaries with the same download ID), and successful HTTP status counts precede identity/Range validation. Do not call these TCP request counts or validated body completions.
- Recovery decision counts count monitor evaluations, not distinct failed requests; log only reason/eligibility transitions. Reading, header, local backpressure and writer-barrier durations have separate ownership. Challenger setup, staged copy and winner flush have separate timings linked to its request ID. Staged challenger bytes are provisional duplicate diagnostics until ownership settles; body-frame counters exclude unconsumed transport buffering and TCP/TLS overhead.
- Terminal worker failure aborts and drains remaining worker handles before final writer/checkpoint handling, including `Disabled` with `request_batch_size=0`. Waiting for peers to finish allows an exhausted, reclaimed range to acquire a fresh retry budget. Preserve the underlying writer error when available.

## 4. Validation & Error Matrix

| Input/event | Result |
| --- | --- |
| Unknown Python mode | ConfigError |
| Absolute limit zero | Configuration error |
| Policy duration zero, nonfinite, negative, greater than 86400 seconds, or unrepresentable | Configuration error, never panic |
| Weak/missing ETag or conflicting condition | No hedge |
| Insufficient conservative extra-body budget | No performance action; full-range hedge can remain blocked while protected cancel/resume is allowed |
| Candidate malformed Range/body | Discard candidate; primary continues |
| Candidate changed ETag, including changed total | Identity mismatch failure |
| Pause/cancel or writer failure | Stop all producers; no unproven checkpoint completion |

## 5. Good / Base / Bad Cases

Good: a persistently slow final piece is recovered under budget; exact output and progress remain correct. Base: Disabled suppresses performance actions, including with opt-in batching; all-slow or explicitly limited downloads avoid performance retries. Bad: renewing a lease while a detached producer still runs, counting duplicate bytes as progress, or resetting ordinary retry history after recovery.

## 6. Tests Required

- Policy tests use controlled observations for grace/window/blocked phases, all-slow evidence, cooldown, budget and shared lineage.
- Diagnostics tests cover request IDs/counters across redirects and clones, exclusive phase timing, and the unchanged 644,694-byte budget refusing a 1 MiB hedge while admitting protected cancel/resume under its read-ahead bound. Test zero/partial prefixes, cumulative enqueue across pieces, reservation settlement on every exit and lineage after queued lease release. The `http_efficiency_compare -- 1 - tail` fixture separates impairment of the first/middle/last batch piece from probe headers, Range headers, all-slow and local pressure controls; retain exact byte validation and do not turn measured elapsed times into CI thresholds.
- Actual MemoryBudget exhaustion must hold an HTTP response in MemoryBlocked without recovery or another request, then cancel promptly. A gated released-batch test must prove one shared retry budget and Retry-After across different pieces.
- `tests/m12_slow_transfer.rs`: deterministic gated responses, exact Range/output checks, hedge failure/identity matrix, stop cleanup, small memory budgets. Do not gate CI on wall-clock speedup.
- Default-policy tail tests must observe a replacement before releasing the original for both adaptive modes, with no absolute floor or shortened public options. Controlled observation tests prove early detection, transient/reset/near-completion guards, ordinary evidence preservation, missing/stale/immature/blocked/all-slow peer suppression, idle-slot and work-exhaustion requirements.
- Python tests cover both APIs, old positional signatures, defaults, valid modes and invalid values; rebuild the extension before pytest.
- `examples/slow_transfer_compare.rs` is a local diagnostic, with shortened thresholds; save CSV and distinguish discarded probe overhead from recovery traffic. Do not extrapolate its latency to real origins.
- `examples/tail_compare.rs` ports the 128 MiB loopback fixture with production defaults and optional aria2 comparison. Run using this workspace crate, retain CSV plus trigger diagnostics, and distinguish published-version measurements from modified-source measurements.

## 7. Wrong vs Correct

Wrong: validate candidate Range first and ignore its error before checking a returned different ETag. Correct: identify explicit version changes before classifying optional protocol failures.

Wrong: notify idle workers while still holding the released request permit. Correct: drop the permit before notifying, so awakened workers can acquire it.
