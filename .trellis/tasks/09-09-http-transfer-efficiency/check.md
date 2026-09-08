# Final verification — 2026-09-09

Windows x86_64, rustc/cargo 1.92.0. Local network test processes cleared inherited
HTTP_PROXY, HTTPS_PROXY and ALL_PROXY. No production proxy/trust changes.

- cargo test -p bytehaul --all-targets: 466 passed on final source, exit 0.
- cargo test -p bytehaul --doc: 1 passed, exit 0.
- cargo clippy --workspace --all-targets -- -D warnings: exit 0.
- RUSTDOCFLAGS=-D warnings cargo doc --no-deps --workspace: exit 0.
- rustfmt --edition 2021 --check on all changed Rust files: exit 0.
- Rebuilt Python extension: uv run --no-sync python -m maturin develop --bindings pyo3.
- uv run --no-sync python -m pytest -q: 116 passed, exit 0.
- git diff --check: exit 0.

The first full suite passed; after a final checked u64-to-usize conversion for
large pieces on 32-bit targets, the full all-target suite was repeated and passed.
No 32-bit/Linux execution or coverage percentage is claimed.

Independent review resolved history duplication, duplicate-byte accounting and
request/lease concurrency confusion, then requested actual same-body slow-tail
and public grouped stop/resume tests. Both gaps are now covered and pass; final
review has no unresolved findings. See review.md.

Final measurements ran with no concurrent builds/tests: 72 HTTP comparisons and
6 batched tail comparisons, every output byte-validated. Final report and raw
CSV are examples/http_efficiency_results.md, http_efficiency_final.csv and
http_efficiency_tail.csv. Source fingerprints are recorded in the report.

HTTP setup100_response25 median: unpooled/unbatched 0.729 s, pooled+batched
0.310 s (57.5% reduction). Strong-validator interrupted transfers avoid 512 KiB
replay in the controlled fixture. TLS tests are network correctness coverage,
not public Downloader HTTPS performance measurements. The pilot is superseded.

User requested commit and push during final validation. Implementation, docs,
specs and checks are complete; work commit precedes task archive/journal commits.
No version bump, package publication or release is included.
