# Verification — 2026-09-09

Windows x86_64 MSVC; rustc 1.92.0 / cargo 1.92.0. Process-local HTTP_PROXY,
HTTPS_PROXY and ALL_PROXY cleared for loopback tests and final measurements.

- cargo test -p bytehaul --all-targets: exit 0, 450 passed; benchmark smoke targets also passed.
- cargo test -p bytehaul --doc: exit 0, 1 passed.
- cargo clippy --workspace --all-targets -- -D warnings: exit 0.
- RUSTDOCFLAGS=-D warnings cargo doc --no-deps --workspace: exit 0.
- rustfmt --edition 2021 --check on changed Rust files: exit 0.
- git diff --check: exit 0.

Logs retained in target/fast-tail-{tests,doctests,clippy,docs}.log.
No Python runtime tests or Linux coverage measurement were performed; no
coverage percentage or cross-platform test result is claimed.

Independent source review found one missing-observation gap for slots held
across setup/backoff. Fixed with target registration and occupied-slot count
guards; regression models peer permit lifecycle. Second review found no
remaining issues. Ordinary detection thresholds, retry lineage, budget,
validator restrictions, writer ownership and checkpoint format remain intact.

Benchmark: published baseline 3 validated outputs; pilot full comparison 24;
final-source recovery-only comparison 6. Final median Adaptive/Hedging times
10.298/10.295 s. Source hashes match the report after all checks. Public results
and raw CSV are in examples/tail_compare_results.md.

Implementation and verification complete. User authorized the code commit on
2026-09-09; task archival and journal recording follow it. No release performed.
