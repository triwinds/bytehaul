# Repair storage failures and coverage validation

## Request
Fix the verified Linux writer-failure defect and prevent repeated local/CI coverage misunderstandings.

## Acceptance
- Single-transfer storage errors publish Failed; never advance unproven durable resume metadata; preserve typed errors and existing durable checkpoint.
- Meaningful portable regression covers final writer failure, plus Linux /dev/full behavior.
- Local Linux and CI invoke one coverage gate with unchanged 95% line threshold and matching pinned tooling/package/target scope.
- Preserve reports and version/revision metadata on gate failure, and distinguish failed tests from measured insufficient coverage.
- Windows helper explicitly checks a line threshold and identifies its report as platform-specific.
- Execute Linux coverage, fix measured coverage gaps with behavior tests as needed, and report measured outcome honestly.
- Update developer docs/specs so functional checks are never described as coverage success. No push or deployment.
