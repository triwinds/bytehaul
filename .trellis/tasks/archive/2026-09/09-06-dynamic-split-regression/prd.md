# Deterministic dynamic split regression

Windows job101506376704 of run34040546169 failed at multi.rs:1920: unsplit466.0892ms, split320.0947ms; the improvement146ms missed a150ms wall-clock threshold. All other jobs including95%coverage passed. User explicitly requests repair and prior push intent persists.

Acceptance: replace this flaky performance comparison with deterministic end-to-end evidence of exact subranges, concurrent requests and full correct output. Keep split and unsplit cases, assert completion/progress. Use a generous timeout only as deadlock guard. No product code, timing threshold loosening, platform skip or coverage exclusions. Keep scope to test helper/test in src/session/multi.rs and testing guideline. Verify focused tests repeatedly plus full tests/Clippy; mutation must show no split/serial processing cannot pass. Commit and push, inspect Actions results.
