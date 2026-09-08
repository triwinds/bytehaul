# Implementation

- [x] Implement bounded fast observation and eligibility, preserving ordinary behavior.
- [x] Add deterministic unit coverage and public default-policy takeover regression tests.
- [x] Port supplied benchmark to this workspace and record baseline/modified comparisons.
- [x] Update Rust and English/Chinese policy docs and slow-transfer spec.
- [x] Run focused recovery tests; all-target Rust tests and doctests; workspace Clippy and rustdoc with warnings denied.
- [x] Review scope, diffs, results and record remaining limitations.

Authorization: user said “改吧” following the concrete research recommendation. No further planning approval needed per session authorization.

Review found and fixed occupied slots with missing observations during writer setup/backoff; second read-only review confirmed regression coverage and no remaining findings. Benchmark details and CSV are in examples/tail_compare_results.md. Final Adaptive/Hedging medians: 10.298/10.295 s; fresh published Hedging baseline: 28.106 s. Prefix continuation and budget changes remain deferred as planned. No commit or release performed.
