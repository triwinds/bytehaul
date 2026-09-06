# Independent documentation review

Date: 2026-09-07
Scope: README.md, docs/*.md, bindings/python/README.md, and the release documentation checklist in testing-and-quality.md. Reviewed the frozen implementation against current 0.2.1 source. No runtime, dependency, release tag, or workflow changes were made by this reviewer.

## Findings (fixed)

The implementer corrected the independently reported behavior discrepancies in both languages:

- Pause and cancellation can both preserve resumable state when resume is enabled; storage failures cannot promise a new durable checkpoint. Compared with session/single.rs and session/multi.rs writer-close and checkpoint paths.
- Received-byte progress and zero ETA do not prove successful final synchronization or verification. Compared with sampled_progress_update and final single/multi completion paths.
- A worker may receive a subrange of a piece; piece completion is recorded only when the whole piece is complete. Compared with scheduler leases and worker flush acknowledgements.
- Chinese tuning now documents autosave_sync_every = 2 and distinguishes checkpoint evaluation from actual persistence. Defaults and retry limits agree with config.rs.
- Removed claims of per-chunk trace events and channel-wait trace instrumentation that do not exist in production logging/flow code.
- Python constructor documentation includes log_level, scopes it correctly, and aligns current retry/autosave defaults. English now explains wait() consuming the handle. Compared with bindings/python/src/lib.rs constructor, download signatures and DownloadTask methods.
- Post-maturin development tests use uv --no-sync, consistent with the prior observed editable-cache issue and the existing release verification spec.

Reviewer mechanical fixes after source freeze:

- docs/tuning.md and docs/tuning.zh-CN.md: restricted min_split_size claims to fresh downloads and included equality. session/mod.rs selects multi only when total_size > min_split_size; session/resume.rs selects restored multi strategy independently from saved piece_count and max_connections.
- bindings/python/README.md: moved the uv --no-sync explanation from below License to Running tests, adjacent to the command it explains.

Also verified the implementer/main fixes for 0.2.1 version examples, master badge branches, Chinese LICENSE path, progress()/subscribe_progress() return types, historical aria2/benchmark applicability, corrupt-control fallback, single versus multi final control-file cleanup, and the release documentation synchronization checklist. English/Chinese current defaults and material contracts agree.

## Findings (not fixed)

No unresolved correctness findings in the reviewed changes. Broader API-reference expansion (for example general checksum and on_progress entries) was deliberately left outside this focused release-documentation update; the existing documented checksum_sha256 option remains valid. No public interface or runtime changes were recommended.

## Verification

- Markdown whitespace lint: git diff --check passed, including the final mechanical edits.
- Local Markdown links: all 40 relative targets and 2 heading anchors across 13 public documents resolve.
- Fences: all 89 fenced blocks are balanced.
- Python syntax: all 25 Python examples pass ast.parse after removing Markdown list indentation.
- Rust syntax: all 21 Rust examples parse with rustfmt --edition 2021; statement fragments were wrapped in a function for parsing only.
- TOML syntax: both TOML snippets parse with tomllib.
- Shell syntax: all 30 bash/sh snippets pass bash -n; commands were not executed.
- Version/branch scan: no 0.2.0 or branch=main remains in the reviewed public documentation.
- TypeCheck and full tests: not applicable to this Markdown-only change; no runtime tests, full Cargo builds or coverage reruns were performed. Snippet syntax validation is not dependency-aware compilation or an execution claim.
- Limits: 28 external links were inventoried but not fetched by this reviewer; release publication/branch status are covered by the main session. Mermaid/PowerShell/text fences were checked for balanced delimiters, not executed or rendered. The final mechanical changes only moved prose and corrected two sentences, so unchanged snippet parsing was not repeated.
