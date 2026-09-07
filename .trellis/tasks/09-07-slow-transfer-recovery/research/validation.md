# Implementation validation

## Status

Implementation and all required quality gates passed; user confirmed the proposed commit and archive plan.

## Native checks

Rust all-target suite, adaptive unit tests, m12 integration matrix, workspace Clippy with warnings denied, doc test and warnings-denied rustdoc passed. Native all-target rerun after the Retry-After queue fix passed. The later captured-deadline regression passed with paused Tokio time; latest full Linux run covers that final revision. Final rebuilt Python extension: 104 tests passed (8.62 s); final workspace Clippy and formatting/diff checks passed.

Independent review fixed producer cleanup on ChannelClosed, classification of optional malformed candidates versus identity changes, ETag-before-Range ordering, diagnostic publication, and Retry-After gating of recovered work. No remote origin or production service was contacted.

## Local benchmark

macOS/aarch64, release binary, localhost HTTP/1.1, 4 MiB, 128 pieces, four workers, rotating mode order, three rounds. Grace/window/duration shortened to 250/250/500 ms; tail absolute floor 32 KiB/s. Exact bytes verified each run. CSV at `target/slow-transfer-validation/benchmark.csv`.

| Scenario | Disabled median/max ms | Adaptive median/max ms | Hedging median/max ms |
| --- | --- | --- | --- |
| Fast | 10.066 / 23.360 | 13.436 / 20.359 | 11.872 / 18.812 |
| Slow tail | 6714.048 / 6747.065 | 775.114 / 824.456 | 822.952 / 876.540 |
| All slow | 1555.427 / 1555.594 | 1558.699 / 1563.780 | 1554.773 / 1557.456 |
| Limited | 3017.764 / 3018.148 | 3009.013 / 3009.324 | 3007.399 / 3011.293 |

Only three samples: report maximum instead of claiming statistically meaningful tail percentiles. CSV includes tail duration, requested overhead, request count and server-handler peak. Cancelled initial probes contribute overhead even in Disabled; server handler counts include cancellation propagation and are not active client concurrency. Extra-tail attempts distinguish recovery traffic. Client request slots and actual diagnostic wire/duplicate counts are enforced/recorded in the engine.

A separate one-round run of all twelve combinations: 22.21 s wall, 0.58 s user, 0.86 s system, maximum RSS 16,121,856 bytes. This includes the in-process server and all modes, not per-mode memory attribution. Raw `resources.csv`/`resources.txt` are local artifacts. Challenger temporary-file bound is 1 MiB, fixture candidate 32 KiB; memory-limit fixture 4097 bytes refers to payload accounting, not total process RSS.

These are controlled-case results, not verification of the user's real origin or universal WAN performance. The final Retry-After correction does not affect these response-success-only scenarios.

## Linux environment

Task-owned Colima profile with 2 CPUs and 6 GiB RAM; existing default context/VM unchanged. Debian 12/aarch64, pinned Rust 1.98.1, Tarpaulin 0.37.2 LLVM engine, two build jobs, bundled LLD. Writable isolated source copy; no old coverage profiles. CI uses Ubuntu 24.04/x86_64, so architecture/distro differ. Cargo download proxy is build-only; shared script removes proxy environment from tests.

First actual Linux run `20260907T052100Z-51a2fa07`: **95.97% (3973/4140), exit 0**, all tests pass (375 library + 73 integration). Initial cargo metadata fetch was interrupted to release a stalled direct-download cache lock; Tarpaulin subsequently built and collected the full report successfully. A fresh final run is started with cached dependencies and the final captured-deadline fix. 53 source/config files SHA256-match the isolated Linux snapshot; hashes saved locally.

## Final measured result

Fresh run `20260907T052736Z-dc1f5053`: **95.90% (3975/4145), exit 0**. All 449 Rust tests passed, including the final captured-deadline regression; benchmark smoke and example targets passed. Threshold remains 95%, no source exclusions were added. Full JSON/HTML/raw log/metadata copied to `target/slow-transfer-validation/linux-final`. Final source SHA256 evidence is beside the reports. Latest Python 104 tests, workspace Clippy, formatting, diff checks, doc test and rustdoc all pass.

Task-owned containers, default-VM preparation image/volume and dedicated Colima profile/data removed after report verification. Existing Docker context remains `colima`. No commit/push/archive performed before workflow confirmation.
