# Measured single-transfer coverage gaps

Baseline Linux gate: 3289/3524 lines (93.33%). Scope is test additions in single.rs only: continuation requests returning retryable then permanent errors, stop during continuation backoff, clean EOF shorter/longer than expected transfer bytes, and finalization byte-count mismatch. Reuse existing raw response fixture, production writer and ephemeral Warp routes. Assert exact errors, request Range offsets/counts, disk contents and durable checkpoints. Preserve production code and the final-offset progress fix.

Four new tests added (multiple scenarios per test):
- Continuation receives HTTP 503 then 403, confirms exactly two Range requests at bytes=4-7 and a still-valid 4/8 byte checkpoint/file.
- Continuation backoff observes Pause and Cancel with one request, preserving that same durable prefix.
- A wire-valid body shorter than expected fails with Body/UnexpectedEof; a nonempty body when zero bytes are expected fails with Body/InvalidData before enqueue. Zero expected bytes makes the assertion independent of TCP chunk boundaries. Both assert exact file bytes, received count and Failed state.
- Finalization rejects both actual writer-prefix mismatch and advertised-total mismatch, joins writer and leaves previous control bytes unchanged.

Native proxy-isolated `cargo test -p bytehaul --lib session::single::tests`: 16 passed (0.10 seconds). After the final frame-independence adjustment, rerun also passed all 16 tests (0.19 seconds). `git diff --check -- src/session/single.rs` passed. Root owns Linux measurement; native functional success is not a coverage claim.
