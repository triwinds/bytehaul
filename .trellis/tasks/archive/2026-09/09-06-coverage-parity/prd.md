# Diagnose local and GitHub coverage mismatch

## User request
Commit the already validated aria2 simplification, then investigate why local coverage can pass but GitHub Actions coverage does not.

## Known facts
- Work committed as f483f37; archived and journaled separately, no push requested.
- Latest failed run 34033292253 uses 1667903 (before this session changes). Ubuntu ordinary tests and coverage fail; Windows/macOS/Python pass.
- Earlier run 32875080449 uses 2d36de2; only coverage fails.
- Current CI uses Ubuntu, unpinned stable Rust and cargo-tarpaulin, `--engine llvm -p bytehaul --all-targets --fail-under 95`.
- Windows helper uses cargo-llvm-cov with independent target directory and report export; README incorrectly shows --workspace compared to workflow -p bytehaul.
- Most recent local work ran tests/lints/docs, not a coverage tool; no historical passing coverage report found yet on this host.

## Acceptance
Identify exact failed log messages and percentages where available; compare like-for-like tool, revision, platform, target scope and metric. Clearly distinguish verified causes from remaining uncertainty. Record concrete corrective steps or scoped fixes if needed without reducing threshold or disguising uncovered code. No deployment or remote push.

## Outcome
Investigation acceptance met; see research/ci-findings.md and research/local-analysis.md. Exact CI percentages and the Linux-only functional failure were verified from downloaded job logs and matching source revision. No corresponding local passing percentage report was found, so an exact same-revision percentage delta remains unproven. Corrective implementation is listed as follow-up; no code changes or push were made in this investigation.
