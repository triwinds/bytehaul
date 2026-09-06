# Research: Local and CI coverage parity

- Query: Why can local verification appear to pass while GitHub Actions coverage fails, and what Linux-only failure is visible in the current code?
- Scope: internal; working-tree source, workflow, helper scripts, existing task artifacts; remote logs and git history assigned to root
- Date: 2026-09-06

## Findings

### Verified command and reporting differences

| Surface | Evidence | Meaning |
| --- | --- | --- |
| CI | `.github/workflows/test.yml:87`: `cargo tarpaulin --engine llvm -p bytehaul --all-targets --out Stdout --fail-under 95` | Ubuntu actual threshold gate |
| Local Windows | `scripts/coverage-windows.ps1:78`: llvm-cov `--no-report`; `:79`: llvm-cov `report -p bytehaul`; `:73`: HTML or summary JSON | Helper collects and exports; **no threshold is checked** anywhere in script |
| Scope switch | `scripts/coverage-windows.ps1:4`, `:38` | Defaults all-targets, but user can explicitly choose tests/lib |
| Documentation | `README.md:107`, `docs/README.zh-CN.md:109` use `--workspace` | Commands differ from actual root-package CI gate; Python workspace member exists in `Cargo.toml:22` |
| Prior local validation | `.trellis/tasks/archive/2026-09/09-06-aria2-simplification/research/validation.md:47`, `research/check.md:44` | Explicitly macOS tests/lints/docs; Ubuntu Tarpaulin and platform-specific paths were not executed |

A zero exit status from this Windows helper proves successful tests/report generation, not coverage >=95%. No local passing percentage report was located by filename and content searches in the repository and its archived task/journal documents. This does not prove an earlier result never existed on another machine.

CI uses floating `ubuntu-latest`, `dtolnay/rust-toolchain@stable`, and unversioned `cargo install cargo-tarpaulin` (`test.yml:62,71,84`), so historical runs are not toolchain-identical by contract. Version drift is an uncontrolled variable, **not an established cause** of the observed failure. Cache keys distinguish regular tests from Tarpaulin (`:44` vs `:80`), and no evidence supports claiming cache contamination.

### Different compiled code despite the same all-targets flag

`src/logging.rs:10` explicitly documents Tarpaulin-specific omission of tracing calls, and the five macros at lines 19/29/39/49/60 use `#[cfg(not(tarpaulin))]`. `src/network.rs:128,194,338,347,356` has another five such exclusions. The Windows helper supplies no matching configuration flag. Therefore the two commands are not intrinsically collecting the same source/line universe, even on one platform. Do not equate an LLVM report region/function percentage with the line-based gate without recording the metric; the repository itself does not select a fail-under-lines or any other llvm-cov threshold.

Platform-specific compiled paths also differ:

- `src/storage/file.rs:49`: Linux fallocate and portable fallback; macOS set_len/sync/seek; Windows portable zero fill.
- `src/storage/file.rs:177,191`: portable zero-fill tests are omitted on macOS.
- `src/storage/file.rs:218`: Linux /dev/full preallocation fallback test.
- `src/session/single.rs:1540`: Linux-only writer-failure regression test.

These are verified denominator/test-set differences, but no platform-adjusted coverage percentage has been measured here.

### Concrete Linux-only failure candidate

`test_run_single_connection_reports_writer_failure` (`src/session/single.rs:1542`) writes a four-byte body into `/dev/full`, expects an IO error, then asserts progress Failed, downloaded=4, and a saved control snapshot with downloaded_bytes=4 (`:1578-1584`). It is absent from Windows/macOS runs.

The current completed-body path calls `writer.flush().await?` before setting a terminal progress state (`src/session/single.rs:458`). `SingleWriterRuntime::flush` (`:170`) joins a failed writer and returns its IO error through `close` (`:186`). Thus a disk failure at that boundary can leave progress in the stream state and return before saving control state. This directly explains why a Linux-only assertion can fail even though all macOS tests pass; root must confirm the exact failed assertion from CI logs for revision 1667903, which predates the current tree.

The test expectation of a four-byte durable resume checkpoint after a /dev/full write failure deserves correction rather than blind preservation: socket-received bytes are not durable bytes. `WriterTask::write_block` (`src/storage/writer.rs:200`) uses asynchronous file writes and an atomic high-water mark; `sync_file` (`:193`) flushes before sync. Review the terminal IO error policy and use a deterministic failing-writer fixture on all platforms, asserting the public error/state and never advertising unpersisted resume data. Do not fix this by disabling the Linux test or merely loosening the Failed assertion.

### Candidate uncovered paths (inference, not measured hotspots)

Current network tests cover localhost HTTP, DNS result caching and proxy routing, but `src/network.rs:184-207` includes platform-verifier/native-root fallback and its error conversion, and `:305-309` includes system DNS configuration errors. Without injected failures these environment-dependent paths are plausible uncovered code. Existing tests mostly assert construction of HTTPS/DoH config; no local TLS fixture was found by searching `src/network.rs` tests. The earlier single-connection retry implementation has extensive branches in `src/session/single.rs:215-602`; source growth alone cannot establish which exact lines account for a drop. Use the actual Tarpaulin file/line report before choosing additional tests.

### Related specs and correction direction

- `.trellis/spec/bytehaul-python/backend/index.md`: applicable Rust/Python scope and predevelopment checklist.
- `.trellis/spec/bytehaul-python/backend/testing-and-quality.md:31`: actual CI-equivalent commands; `:71` documents the exact Tarpaulin command.
- Same quality guide requires deterministic failures/temporary local servers and exact public error variants, and distinguishes local shell proxy isolation.
- `.trellis/workflow.md`: findings persisted to active task research and research/implementation role separation.

Concrete improvements: unify the documented package scope; make Windows reports explicitly informational or add an explicit line threshold (still not platform equivalence); save machine-readable per-file coverage plus exact versions and commit in CI even when the threshold fails; run the exact Ubuntu gate at the same revision before claiming local parity; fix the verified Linux functional failure before treating a current coverage job failure as only insufficient percentage.

## External references

No external browsing was performed, per dispatch scope. Tool-specific metric internals and version-specific behavior must be verified by root from authoritative tool docs or installed source before assigning causality.

## Caveats / Not Found

- Research role forbids any git operation; historical diffs for cf7198d/3efcb42, 2d36de2, 4517569, and 1667903 must be supplied/checked by root. No historical commit chronology is claimed from working-tree reads.
- No coverage tool was run, no build output was created, and no source/script files were changed.
- Exact failing CI messages, run percentages, and changed-line attribution belong to root remote-log investigation; this artifact distinguishes source-level mechanism from that evidence.
