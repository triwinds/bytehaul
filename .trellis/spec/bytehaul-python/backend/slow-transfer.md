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
- Performance budget is `min(total / 100, 16 MiB)` conservatively reserved by full replacement range, ten-second global cooldown, at most two actions per shared lineage. Recovered children retain ordinary retry count and elapsed-time history.
- Stop/drop producers before FIFO discard acknowledgement, then reclaim or renew the lease. Never let old producer data enter a new lease. Roll back ineffective progress; only completed, flushed pieces enter durable state. Preserve checkpoint capture-before-sync.
- Hedge only a final range no larger than 1 MiB, with a spare slot, strong ETag and no conflicting user conditional headers. At most one challenger; primary plus challenger stay within max_connections. Stage the challenger to a temporary file; only the winner writes the authoritative file, through the existing bounded writer path. Temporary data and losing futures must be released on every exit.
- Optional malformed/transient challenger failures leave the primary alive. Actual identity changes and 412 fail the download. Retry-After blocks further performance actions. No duplicated progress; wire/duplicate/reserved-budget counters are diagnostics.
- Terminal worker failure aborts and drains remaining worker handles before final writer/checkpoint handling. Preserve the underlying writer error when available.

## 4. Validation & Error Matrix

| Input/event | Result |
| --- | --- |
| Unknown Python mode | ConfigError |
| Absolute limit zero | Configuration error |
| Policy duration zero, nonfinite, negative, greater than 86400 seconds, or unrepresentable | Configuration error, never panic |
| Weak/missing ETag or conflicting condition | No hedge |
| Insufficient whole-range budget | No performance action |
| Candidate malformed Range/body | Discard candidate; primary continues |
| Candidate changed ETag, including changed total | Identity mismatch failure |
| Pause/cancel or writer failure | Stop all producers; no unproven checkpoint completion |

## 5. Good / Base / Bad Cases

Good: a persistently slow final piece is recovered under budget; exact output and progress remain correct. Base: Disabled retains the existing loop; all-slow or explicitly limited downloads avoid performance retries. Bad: renewing a lease while a detached producer still runs, counting duplicate bytes as progress, or resetting ordinary retry history after recovery.

## 6. Tests Required

- Policy tests use controlled observations for grace/window/blocked phases, all-slow evidence, cooldown, budget and shared lineage.
- `tests/m12_slow_transfer.rs`: deterministic gated responses, exact Range/output checks, hedge failure/identity matrix, stop cleanup, small memory budgets. Do not gate CI on wall-clock speedup.
- Default-policy tail tests must observe a replacement before releasing the original for both adaptive modes, with no absolute floor or shortened public options. Controlled observation tests prove early detection, transient/reset/near-completion guards, ordinary evidence preservation, missing/stale/immature/blocked/all-slow peer suppression, idle-slot and work-exhaustion requirements.
- Python tests cover both APIs, old positional signatures, defaults, valid modes and invalid values; rebuild the extension before pytest.
- `examples/slow_transfer_compare.rs` is a local diagnostic, with shortened thresholds; save CSV and distinguish discarded probe overhead from recovery traffic. Do not extrapolate its latency to real origins.
- `examples/tail_compare.rs` ports the 128 MiB loopback fixture with production defaults and optional aria2 comparison. Run using this workspace crate, retain CSV plus trigger diagnostics, and distinguish published-version measurements from modified-source measurements.

## 7. Wrong vs Correct

Wrong: validate candidate Range first and ignore its error before checking a returned different ETag. Correct: identify explicit version changes before classifying optional protocol failures.

Wrong: notify idle workers while still holding the released request permit. Correct: drop the permit before notifying, so awakened workers can acquire it.
