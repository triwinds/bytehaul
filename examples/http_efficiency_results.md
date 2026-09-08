# HTTP transfer efficiency comparison

Measured 2026-09-09 on Windows x86_64, rustc 1.92.0 / cargo 1.92.0, dev profile. Based on `1c187ed` plus this task. The crate version remains 0.2.2; these changes are not a published release.

## Delivered behavior

- Opt-in `request_batch_size` groups contiguous piece leases into bounded HTTP requests (byte cap plus 64 leases). Default zero keeps grouping off; piece/checkpoint granularity is unchanged.
- Existing experimental idle pooling remains independently opt-in with default zero.
- A protected strong ETag enables writer-confirmed prefix continuation for ordinary retries and adaptive reassignment. Missing/weak validators retain replay; incomplete pieces are still incomplete across process restarts.
- Rust and both Python download entry points expose request batching. Idle-pool configuration remains Rust-only.

## Controlled HTTP fixture

16 MiB deterministic body, HTTP/1.1, 4 workers, 1 MiB pieces, 4 MiB batch cap when enabled, 4 idle connections when pooling is enabled. Fresh temporary output, no file allocation/resume/rate cap; retries limited to 2 with 1–5 ms backoff. Three rounds rotate policy order. Every output is compared byte by byte. No builds or tests ran concurrently with measurements. Proxy environment variables were cleared.

`setup100_response25` delays each accepted connection by 100 ms and each response by 25 ms. `response100` delays each response by 100 ms. These are application-level synthetic delays, not real WAN RTT or TLS-handshake measurements. `close_setup100` additionally sends Connection: close. `disconnect_*` closes the first response crossing offset 8.5 MiB, after delivering exactly that prefix, with 25 ms response delay.

Elapsed time covers the download, excluding client construction, compilation and output validation. Server-body bytes count successful body writes, excluding HTTP headers; they are not packet-capture wire totals. Unaligned Range starts can arise from dynamic splitting as well as suffix recovery, so the CSV column is not a retry counter.

Raw data: [72 HTTP results](http_efficiency_final.csv). Medians in seconds:

| Scenario | No pool/batch | Pool only | Batch only | Pool + batch |
| --- | ---: | ---: | ---: | ---: |
| local | 0.096 | 0.062 | 0.073 | 0.076 |
| setup100_response25 | 0.729 | 0.387 | 0.471 | 0.310 |
| response100 | 0.634 | 0.561 | 0.398 | 0.386 |
| close_setup100 | 0.629 | 0.680 | 0.400 | 0.437 |
| disconnect_weak | 0.236 | 0.212 | 0.153 | 0.151 |
| disconnect_strong | 0.260 | 0.214 | 0.167 | 0.163 |

In setup100_response25, combined options reduced the measured median by **57.5%**. Request/connection medians for that scenario:

| Policy | Requests | Connections |
| --- | ---: | ---: |
| unpooled | 17 | 17 |
| pooled | 16 | 4 |
| batched | 9 | 9 |
| pooled_batched | 8 | 4 |

All strong-validator disconnection runs wrote exactly 16,777,216 server body bytes; all weak-validator runs wrote 17,301,504. Retaining the confirmed prefix avoided **524,288 bytes (512 KiB)** of replay in this fixture. This demonstrates saved traffic; at loopback body speeds, it does not guarantee lower elapsed time.

Pooling alone cannot remove per-request response latency and cannot reuse a connection closed by the origin. Combining options was not uniformly best in the local or Connection: close cases. Defaults therefore remain opt-in while deployments measure their own origins.

## Slow-tail check with 4 MiB requests

[Six tail results](http_efficiency_tail.csv) use the existing 128 MiB tail fixture, default detection durations and a 4 MiB batch cap. The first response reaching the final MiB is throttled. All six outputs were byte-validated.

| Mode | Median seconds | Median requests |
| --- | ---: | ---: |
| adaptive | 10.727 | 40 |
| hedging | 10.688 | 38 |

The previous unbatched fast-tail report measured 10.298/10.295 seconds. The new grouped measurements remain near that scale, but do not demonstrate a further tail-latency improvement. They are separate runs, not a statistical regression threshold. The gated m12 regression additionally forces the slow piece to be the suffix of an already active multi-piece response, so detection across piece boundaries is explicitly tested.

## TLS and verification

Three hermetic network-layer tests use a generated local CA and scoped private client roots: two fully consumed Range responses reuse one TLS connection; Connection: close requires two; an untrusted certificate yields typed UnknownIssuer before any HTTP request. No system trust store or production certificate checks were changed. Public Downloader HTTPS performance was not measured.

- `cargo test -p bytehaul --all-targets`: 466 passed on final source; benchmark smoke targets also passed.
- `cargo test -p bytehaul --doc`: 1 passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- Strict workspace rustdoc and changed Rust file formatting: passed.
- Rebuilt Python extension with maturin; `uv run --no-sync python -m pytest -q`: 116 passed.
- Independent source/test review: no unresolved findings.
- No Linux execution or coverage percentage is claimed. Logs are retained under `target/http-efficiency-*`.

## Reproduction

```powershell
$env:HTTP_PROXY=$null
$env:HTTPS_PROXY=$null
$env:ALL_PROXY=$null
cargo run --example http_efficiency_compare -- 3
cargo run --example tail_compare -- - 3 recovery-only 4194304
```

The exploratory pilot predates probe-reuse and request-aware splitting fixes and is retained only under target/. The published CSV above is from the final implementation.

## Measured source SHA-256

- `src/config.rs`: `37cab924f4d3b7305723c5ac4041c795bfc76b699381583b7af7363ff75d4c93`
- `src/scheduler.rs`: `a179c16d8dd5d8327c39005b1fac0b47ed6cb136e7d82584e8dfd19043af0bc8`
- `src/session/multi.rs`: `2210b6599c248998233918ca4987f93d631750b8c119e02d5a146c82e9329665`
- `src/session/multi/adaptive.rs`: `8d8d62b0aceb8dfa2edfeec72222ea6fbbc36c8f9d8661163dd3977d317ca245`
- `examples/http_efficiency_compare.rs`: `635c5bf3700cfcf9da11db46d26fc711aa167f099f666fefab04104faf261b99`
- `examples/tail_compare.rs`: `241052a7438aff8f73d0f5d499f1ca4a4742fb62e728dbb6c93ff1f828f0a446`
