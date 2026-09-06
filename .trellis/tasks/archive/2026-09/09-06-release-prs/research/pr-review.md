# Dependency PR assessment for 0.2.1

Read-only assessment on 2026-09-06 before the release metadata update. Reviewed all five PR diffs, current manifests/lockfile and call sites, retained Actions check metadata, and upstream published crate source. No PR was commented on, closed or merged by this reviewer.

## Recommended disposition

| PR | Disposition for the patch release | Evidence |
| --- | --- | --- |
| [#7 bincode 3.0.0](https://github.com/triwinds/bytehaul/pull/7) | Close the automated bump; retain bincode 1.3.3. Any serialization migration is separate work. | The published bincode 3.0.0 `src/lib.rs` contains only `compile_error!("https://xkcd.com/2347/");`; it cannot build. Current control persistence calls the bincode 1 `serialize`/`deserialize` APIs and preserves V1/V2 resume compatibility. |
| [#9 warp 0.4.2](https://github.com/triwinds/bytehaul/pull/9) | Close the manifest-only bump and defer test-server migration. | The PR changes `warp = "0.3"` to `"0.4"` without features or fixture changes. Warp 0.4.2 defaults to no features; serving requires `server`, and its server API uses async `bind`/`incoming` instead of `bind_ephemeral`. Current source/tests contain 68 `bind_ephemeral` call sites. |
| [#16 PyO3 0.28.3](https://github.com/triwinds/bytehaul/pull/16) | Close the manifest-only bump and defer the binding migration. | Current binding code uses `Python::with_gil`, `py.allow_threads`, and the root `PyObject` alias. Published 0.28.3 exposes `Python::attach`/`detach`; the old methods and root alias are absent. The PR changes no binding source. Migration must preserve GIL release, callback/lifecycle behavior and abi3 packaging, then validate the built Python extension. |
| [#17 rand 0.9.2](https://github.com/triwinds/bytehaul/pull/17) | Viable: incorporate the manifest upgrade to rand 0.9 in the release branch, resolving against the current lockfile, then mark the stale PR incorporated/superseded after current checks pass. | Rand is dev-only and has no direct source/test/benchmark/example call sites. Production retry jitter uses fastrand. Historical PR CI is entirely green. Current lock already contains rand 0.9.4; reuse it rather than importing the old PR's 0.9.2 lock entry or unrelated old lockfile state. |
| [#18 libc 0.2.185](https://github.com/triwinds/bytehaul/pull/18) | Close as superseded; no dependency change needed. | Its only change is libc 0.2.184 to 0.2.185 in Cargo.lock. The current lockfile already resolves libc 0.2.186. |

## Concrete compatibility evidence

- `src/storage/control.rs:116,180,185` uses bincode 1 serialization/deserialization; its persisted control format and legacy reads must not be changed as a side effect of a dependency bump. The [published bincode 3.0.0 crate](https://static.crates.io/crates/bincode/bincode-3.0.0.crate) was downloaded and inspected in memory; its unconditional compile error is decisive even without historical build logs.
- The [published warp 0.4.2 crate](https://static.crates.io/crates/warp/warp-0.4.2.crate) declares `default = []` and a `server` feature in Cargo.toml. Its `src/server.rs` has async `bind` and `incoming`, with no `bind_ephemeral`. Current examples of the affected pattern include `tests/m1_basic.rs:12` and `src/manager.rs:377`. This is a broad fixture migration, not a working one-line bump.
- Current PyO3 examples are `bindings/python/src/lib.rs:443` (`PyObject` callback), `:449` (`with_gil`), `:467` and `:716` (`allow_threads`), plus `bindings/python/src/lib_tests.rs:20`. The [published PyO3 0.28.3 crate](https://static.crates.io/crates/pyo3/pyo3-0.28.3.crate) provides `attach` and `detach` in `src/marker.rs`, and its bundled migration guide documents the renames and alias deprecation. The old methods do not exist in that version's marker implementation. No source migration is included in PR16.
- The rand PR changes only its dev-dependency requirement and the root package's lockfile dependency reference. Current pre-release lock versions were rand 0.8.6 and 0.9.4, so accepting the requirement update does not require a new randomness API migration in this codebase.
- [Merged PR12](https://github.com/triwinds/bytehaul/pull/12) explicitly deferred both bincode 1→3 and warp 0.3→0.4 for migration work; keeping them out of this repair release preserves that decision.

## Historical checks and limits

Retained PR check metadata shows bincode failures across all jobs; warp Rust/coverage failures with Python passing; PyO3 Rust/Python failures with coverage passing; and rand/libc checks passing. These are checks of old PR revisions, not validation of the current release worktree.

Attempts to retrieve detailed logs for runs 24192032050 (bincode), 24079971827 (warp), and 24192031787 (PyO3) all returned HTTP410. Therefore this assessment does not claim to have observed their original compiler errors. The compatibility conclusions above rely on current repository code and the actual published dependency source.

No dependency experiment, checkout, source edit, merge, comment or CI rerun was performed for this assessment. Current release tests and package validation are separate gates owned by the main session and implementer/reviewer.
