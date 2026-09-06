# Public documentation aligned with 0.2.1

## Boundary

Changed public Markdown only: README.md, bindings/python/README.md and the11 docs/*.md files. No runtime, manifest, dependency or release-tag edits. Correct sections were retained; current behavior was checked against source and mirrored in English/Chinese.

## Changes and evidence

- Root READMEs now use0.2.1 installation examples, master-branch test badges and the publishedv0.2.1 release link. Fixed the Chinese README's relative LICENSE link.
- Python guides distinguish installation from PyPI from source-build requirements, document0.2.1, keep post-maturin tests on `uv run --no-sync`, and align task-handle lifecycle and constructorlog_level wording with bindings/python/src/lib.rs. Added compact retry-duration/autosave options relevant to durability; did not expand the entire API reference.
- Advanced/Python/architecture docs explain UI received bytes versus durable checkpoint bytes, retry rollback, full displayed bytes with final storage failure, and why ETA0 cannot establish success. Evidence: src/progress.rs:33 and single.rs complete_single_transfer (received count beforeflush; Completed afterflush/close/requiredcleanup).
- Completion cleanup distinction remains explicit: single cleanup failure is an error; multi.rs control deletion is best effort. Network failures may preserve a newly confirmed terminal snapshot; docs never promise saving unconfirmed bytes after writer failure. Caller success is determined by wait(), including configured checksum validation.
- Architecture correctly places bounded-channel/writer/lease-cache forwarding and flush acknowledgement before scheduler completion; progress() returns a snapshot while subscribe_progress() returns a watch receiver (src/manager.rs313/318).
- Pause and cancel both end the task and attempt a resumable checkpoint when enabled; storage failure cannot guarantee a new save (single/multi stop branches).
- Tuning describes workers fetching disjoint subranges of the same piece, autosave tick batching/default2, and old-checkpoint preservation on writerfailure. Chinese now includes the missing autosave setting. April2026 benchmark numbers remain intact but clearly historical, predating0.2.1.
- Troubleshooting corrects corrupt-control behavior (resume.rs130-154 ignores failed loads and starts fresh), explains terminal storage errors, distinguishes Windows cargo-llvm-cov95% from Linux Tarpaulin's different denominator, and removes claims of nonexistent production per-chunk trace events.
- aria2 design document now marks every baseline/plan section from 校准后的结论 onward historical, including oldVERSION1/piececache references. The0.2.1 summary and architecture/release links identify current behavior; historical details were preserved.

## Checks

- `git diff --check` passed after removing one newly introduced Markdown trailing-space hard break.
- Checked all13 public Markdown files: balanced fenced-code blocks, no stale branch=main test badge or bytehaul0.2.0 Cargo install example.
- All25 Python fenced examples parse with ast.parse after dedenting nested-list code blocks; no examples were executed.
- Root independently owns link verification and the checker owns final source/document consistency review. No full runtime suite was run for this docs-only change.
