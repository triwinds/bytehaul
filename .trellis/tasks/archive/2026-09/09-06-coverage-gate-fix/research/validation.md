# Validation record

## Environment
Host: macOS/aarch64, rustc1.96.0. Native tests/clippy/docs checked by independent reviewer.
Linux: disposable local Docker container bytehaul-coverage-0906, Debian12/aarch64 from official rust:1.96.0-slim-bookworm, rustup installed1.98.1 + llvm-tools-preview, Tarpaulin0.37.2 official musl binary. Cargo.lock unchanged; Baseline collection uses CARGO_BUILD_JOBS=1 and pinned Rust bundled LLD22.1.8; final collection uses CARGO_BUILD_JOBS=2 in the dedicated6GB VM. Future CI is Ubuntu24.04/x86_64, so this is the same gate/toolchain on Linux but not an identical distro/architecture.

Initial infrastructure attempt used a readonly source bind with writable target volume. Tarpaulin instrumented build scripts attempted /work/build_rs_cov.profraw and reported readonly errors. Aborted this attempt (exit143) and did not interpret its output as coverage. Retried in a writable isolated /checkout copy with the same source and target volume.

## Local environment retries

- Writable run20260906T133320Z-51f5ea78 at4 jobs: compiler/linker killed (cgroup oom_kill4), no JSON coverage report; shared entry exited1 and correctly called it tooling failure.
- Writable run20260906T133741Z-86f32fca at1 job: GNU ld still killed (oom_kill5). VM total memory~2GB; existing user VM was not restarted or resized.
- Container-only linker selection switched `/usr/local/bin/ld` to pinned Rust bundled LLD22.1.8. Diagnostic retry reused the failed-build target with --skip-clean; those earlier builds never ran tests or produced test profiles. Same source/toolchain/engine/package/all-targets/95%; complete shared fresh run to follow if needed after measured gap repair.
- Shared script later gained distro and CARGO_BUILD_JOBS metadata only (no command semantic changes);7 updated stub regressions passed.

## Results
Native checks:347 unit+66 integration=413, benchmark smoke, doc1, Clippy workspace/all-targets-Dwarnings, rustdoc-Dwarnings all pass. Linux script7 stubs pass. Pending complete Linux gate measurement.

## Actual Linux finding and correction
The LLD diagnostic run built successfully and completed66 integrations, but the library test had350 passes/1failure: Linux /dev/full state was correctly Failed, downloaded was stale0 rather than4 (progress throttling). Exact behavior gap fixed by publishing known final received offset before final sync; portable tests now seed stale0 and assert4. No checkpoint assertion was relaxed. The diagnostic failed test aborted coverage, so it yielded no valid final percentage.

Library suite took382.14seconds with the old fixed-port cache tests. Replaced six test-only network fixtures with ephemeral403 responses; focused native manager26 pass in1.21sec, preserving exact errors/cache counts/default retry policy. Fresh shared Linux gate started after both corrections and metadata update.

## Fresh Linux baseline and targeted regression tests
Run20260906T135935Z-aebd1bb5 used the shared entry with an isolated new build/profile/report directory. All351 Linux library tests and66 integrations passed, plus benchmark smoke and example compilation. The actual gate measured93.33% (3289/3524):59 more covered lines required for95%. It exited1 and preserved JSON/HTML/log/metadata correctly. Reports copied to target/linux-coverage-baseline/.

Measured misses guided new behavior tests for single continuation retries/stops/body length/finalization, multi terminal failure/body length/checkpoint failure, range validation protocol boundaries and redirect resolution/cache expiry. No production exclusions or threshold reduction. Native focused HTTP16 and range16 pass. An initial HTTP-only command inherited the host's unsupported SOCKS proxy and failed at client construction; rerunning with the same six proxy variables removed as the shared gate passed.

Final fresh Linux measurement follows after these tests and independent review.

Final native source checks:363 library +66 integration =429 tests pass, benchmark smoke and example compilation pass. Clippy workspace/all-targets with warnings denied, rustfmt check, script7 subprocess regressions and diff check pass. Independent review reports no unresolved issue. Earlier doc1 and warnings-denied rustdoc checks passed; later changes are test-only in Rust.

## Final-run infrastructure limit
Fresh run20260906T141952Z-0f0c9b24 compiled dependencies but rustc was SIGKILLed building the enlarged library test binary; cgroup oom_kill increased5→6. No tests ran, no percentage was produced; shared entry correctly exited1/toolingfailure. The existing2GB user VM was left unchanged. A separate task-owned Colima profile bytehaul-coverage-0906 (2CPU/6GB, no activecontext switch, no SSHconfig edit, no host mounts) was created for continued actual validation. Tool/source container snapshot is migrated; final run still starts with a fresh build/profile/report directory.

Dedicated VM run20260906T143536Z-d125ad33: same Debian12/aarch64 container/toolchain,2 buildjobs and6GB VM memory. No previous target profiles migrated. SHA256 of all five changed Rust files plus shared script/config matched the native final source after migration. The existing active Docker context remains colima.

## Final measured result
Run20260906T143536Z-d125ad33 completed: **96.59% (3404/3524 lines), exit0**, exceeding the unchanged95% line threshold. Baseline3289→3404 covered lines (+115); denominator3524 unchanged. Single481→539, multi336→368, HTTPworker112→127, rangevalidation65→75. Linux368 library +66 integration =434 tests pass; both Linux-only single/multi writer-failure regressions pass. Benchmark smoke passes; example target compiles/runs0 tests.

Final JSON/HTML/rawlog/metadata copied to target/linux-coverage-final; validated exit0, percentage and434 test count from copied artifacts. verified-source-sha256.json records the changed files matched against the Linux snapshot. Build-failure evidence saved in target/linux-coverage-build-failure. Reports are local ignored artifacts, not committed.

Limits: measured on Debian12/aarch64 with pinned Rust1.98.1/Tarpaulin0.37.2/LLVM/LLD and2 jobs in a dedicated6GB VM. GitHub uses Ubuntu24.04/x86_64; no push or new Actions run was performed. Windows helper was inspected but not executed on Windows.

## Completion
Work committed as3d21ed0. Task-owned container, target volume, migration image and dedicated Colima profile/data removed after report copies were verified. Original default VM/context left intact.
