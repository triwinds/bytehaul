---
name: release-version
description: "Update the workspace package version, synchronize README and docs version references, optionally auto-bump the current version, regenerate Cargo.lock, generate crate docs, create a release commit, create a git tag, and push the branch and tag. Use when the user asks to bump a version, auto-increment a version, update version numbers, regenerate Cargo.lock, commit a release, 打 tag, 发布版本, or push release changes."
---

# Release Version

Perform the repository's version bump workflow end-to-end: update the version, synchronize README and docs version references, regenerate the Rust lockfile, generate crate docs, create a focused release commit, create an annotated tag, and push the result.

## Gather Inputs

Accept:
- `version`: optional target semantic version without a leading `v`; if omitted, derive it from the current workspace version by incrementing the last numeric segment with carry, for example `0.1.4 -> 0.1.5`, `0.1.9 -> 0.2.0`, `0.9.9 -> 1.0.0`
- `branch`: branch to push; default to the current branch
- `remote`: git remote to push; default to `origin`
- `tag`: explicit tag name; default to `v<version>`
- `commitMessage`: explicit commit message; default to `release: v<version>`
- `validationCommand`: optional extra validation command to run before commit
- `skipValidation`: if true, skip optional validation beyond lockfile regeneration, lockfile verification, and mandatory crate docs generation

## Repository Facts

- The authoritative crate version lives in the root `Cargo.toml` under `[workspace.package].version`.
- The root crate and `bindings/python/Cargo.toml` both inherit the version with `version.workspace = true`, so do not edit child crate versions directly.
- `bindings/python/pyproject.toml` uses `dynamic = ["version"]`, so do not hardcode a Python package version there.
- Release-related version references in `README.md` and `docs/**` should be kept in sync with the workspace version when they describe the current published package version.
- Regenerate the lockfile from the repository root with `cargo generate-lockfile`.
- Generate crate docs before tagging with `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace`.
- Unless the user provides `version`, derive the release version from the current `[workspace.package].version` using dot-separated decimal carry semantics.

## Execute The Release Workflow

1. Inspect the repository state before editing.
   - Run `git status --short` and require a clean worktree before proceeding.
   - If unrelated changes are present, stop and ask the user whether to stash, commit separately, or abort. Do not include unrelated files in the release commit.
2. Resolve the git target context.
   - Determine the current branch if `branch` is not provided.
   - Use `origin` if `remote` is not provided.
   - If HEAD is detached or the remote does not exist, stop and ask.
3. Resolve release metadata.
   - Read the current version from `[workspace.package].version` in the root `Cargo.toml`.
   - If `version` is provided, use it as-is after validation.
   - If `version` is omitted, compute it from the current version by incrementing the last numeric segment.
   - Apply carry from right to left across every numeric segment: `0.1.4 -> 0.1.5`, `0.1.9 -> 0.2.0`, `0.9.9 -> 1.0.0`.
   - Treat `9` as the rollover point for each segment in this auto-bump mode.
   - Require the current version to be a dot-separated numeric version before applying auto-bump; if it is not, stop and ask for an explicit `version`.
   - Default `tag` to `v<version>`.
   - Default `commitMessage` to `release: v<version>`.
   - Reject versions that already start with `v`; tags may use the `v` prefix, the version field must not.
4. Check for conflicts before editing.
   - Fetch tags from the target remote.
   - Fail if the target tag already exists locally or on the remote.
   - Fail if the branch is behind the remote and would require a pull or rebase before push.
   - Never force-push as part of this workflow.
5. Update the version references.
   - Edit the root `Cargo.toml` and replace `[workspace.package].version` with the resolved target version.
   - Update release-related version references in `README.md` and `docs/**` that describe the current published package version, such as installation snippets or "current version" notes.
   - Do not change unrelated dependency versions or historical examples that are not describing the current release.
   - Do not edit `bindings/python/Cargo.toml` unless the repository structure has changed away from `version.workspace = true`.
6. Regenerate and verify the lockfile.
   - Run `cargo generate-lockfile` from the repository root.
   - Verify lockfile consistency with `cargo metadata --locked --format-version 1`.
   - If `Cargo.lock` does not change, keep going; regeneration still succeeded.
7. Run validation.
   - Minimum validation for this workflow is successful lockfile regeneration plus successful `cargo metadata --locked --format-version 1`.
   - If `validationCommand` is provided, run it.
   - Otherwise, unless `skipValidation` is true, run a lightweight repository validation such as `cargo check --workspace --locked`.
8. Generate crate docs before tagging.
   - Run `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace` from the repository root.
   - Treat crate docs generation as mandatory for this workflow even when `skipValidation` is true.
   - Stop before commit or tag creation if the docs build fails.
9. Review the diff before committing.
   - Confirm the diff is limited to release metadata files, normally `Cargo.toml`, release-related README/docs updates, and optionally `Cargo.lock`.
   - If other files changed unexpectedly, stop and investigate before committing.
10. Create the release commit.
   - Stage only the intended release files.
   - Commit with the resolved commit message.
11. Create the annotated tag.
   - Create an annotated tag for the release commit using the resolved tag name.
   - The tag message should match the tag name unless the user asked for a custom message.
12. Push safely.
   - Push the branch and annotated tag to the target remote.
   - Prefer `git push <remote> <branch> --follow-tags` when the branch is already configured.
   - If the branch has no upstream, use an explicit push that establishes upstream without force.
13. Verify the result.
   - Confirm the branch push succeeded.
   - Confirm the new tag exists on the remote.
   - Confirm the worktree is clean after the push.

## Apply Decision Rules

- Complete the whole workflow in one turn when there are no blockers.
- Ask only when a blocker prevents a safe push, such as a dirty worktree, missing remote, detached HEAD, existing tag, or branch divergence.
- Never force-push.
- Never create the tag before crate docs generation succeeds.
- Never create the tag before the commit succeeds.
- Never push a tag that points to an unpushed or failed commit.
- Never include unrelated files in the release commit.
- If `version` is omitted, derive it deterministically from the current workspace version using the carry rules above.
- If auto-bump cannot be computed safely, stop and ask for an explicit `version`.
- If a step fails after the commit but before the push, report exactly what succeeded and what still needs recovery.

## Report Results

When using this skill, report:
- the target version
- the files changed for the release
- the validation commands that ran and whether they passed
- the commit SHA that was created
- the tag name that was created
- the remote and branch that were pushed
- any blocker or manual follow-up if push or tag publication did not complete

## Reference Prompts

- Use `$release-version` with no `version` argument to auto-bump the current workspace version.
- Use `$release-version` with `version=0.1.5`.
- Use `$release-version` with `version=0.1.5 branch=main remote=origin`.
- Use `$release-version` with `version=0.1.5 tag=bytehaul-v0.1.5 commitMessage="release: 0.1.5"`.
