# Deterministic dynamic split regression

## Change boundary

The failing assertion compared two wall-clock durations and required150ms improvement. Windows measured146ms despite correctly splitting. Only the existing test fixture and its one regression in src/session/multi.rs change; no production scheduler/worker implementation changes.

## New evidence

The ephemeral Warp server reports each requested inclusive byte range together with a private oneshot response-release sender. It awaits that signal before sending any response headers/body. The test must observe every expected request before releasing any response.

- Unsplit case: exactly (0,1023).
- Split case: exactly (0,255), (256,511), (512,767), (768,1023), all received before any response is released. Serial requests cannot satisfy the gate.
- Responses use position-dependent bytes (offset modulo251), so exact1024-byte output detects misplaced ranges and corruption.
- Final progress must be Completed, downloaded1024, total1024; no extra request is accepted.
- A30-second timeout bounds failure/deadlock, not download speed. No fixed sleeps, timing deltas, platform skips, weakened thresholds or coverage exclusions remain.

## Validation

- Initial native focused test passed, followed by10/10 repeated runs; each explicitly ran exactly the one renamed regression.
- Intentional mutation: changed only this test's max_connections4 to1 and shortened the deadlock guard from30s to2s. The regression failed (exit101) after2.04s with `all expected ranges must arrive before any response is released: Elapsed(())`. This demonstrates serialized/unsplit processing cannot satisfy the concurrent-arrival contract.
- Restored the exact pre-mutation test block, including max_connections4 and the final30s guard. The restored focused test passed again (1/1,0.07s). No production code was mutated.
- All cargo checks isolate the six uppercase/lowercase HTTP/HTTPS/ALL proxy environment variables.
- Full native tests and Clippy are owned by the independent reviewer; root handles commit/push and Windows/Actions validation.

