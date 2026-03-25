---
name: plan-driven-implementation
description: Implement work from a markdown plan file instead of an ad hoc request. Use when Codex should read a plan document, complete every remaining phase or a requested phase, keep the same plan file synchronized with actual progress, validate each completed phase with evidence, and optionally create separate commits for completed phases.
---

# Plan-Driven Implementation

Implement work from a markdown plan file and keep the plan synchronized with the real repository state.

## Gather Inputs

Pass the plan file path at invocation time. Do not hardcode a specific plan file in this skill.

Require:
- `plan`: path to a markdown plan file in the current workspace

Accept:
- `phase`: implement only a specific phase, milestone, or checklist item
- `commitMessage`: provide a commit message format or prefix
- `stopBeforeCommit`: if true, update the plan and code but do not create a commit

## Execute The Plan

1. Open the file provided in `plan`.
2. Parse the plan structure and identify phases, milestones, checklists, dependencies, and completion criteria.
3. If `phase` is provided, constrain all work to that scope and complete every checklist item and exit criterion inside it. Otherwise, treat the requested scope as every remaining incomplete phase in the plan, starting from the first incomplete phase.
4. Confirm what code changes and validation steps are required for the current phase before editing.
5. Implement the current phase fully before moving on. Do not stop after one phase when additional phases remain in the requested scope, unless a real blocker is documented in the plan.
6. Update the same plan file immediately after implementation:
   - mark the completed phase or checklist items
   - add a short factual note when it helps, such as touched files or validation that now covers the phase
7. Run the smallest relevant validation for that phase, capture the actual commands or checks used, and do not rely on unverified reasoning.
8. Create a separate commit for the completed phase unless `stopBeforeCommit` is true.
9. Repeat phase-by-phase until the requested scope is complete. When `phase` is not provided, this means the plan should end with no remaining incomplete phases.
10. Before declaring the requested scope complete, run a final verification sweep across the full requested scope and record the evidence.

## Apply Decision Rules

- Ask only about ambiguity that blocks implementation.
- Treat the plan as intent when the plan and code disagree, but inspect the code before changing anything.
- Update the plan first when a phase is already implemented in code but not marked complete.
- Split an oversized phase into safe internal steps if needed, but keep plan updates aligned with the original phase heading.
- When `phase` is not provided, default to finishing the full remaining plan, not just the next phase.
- Never mark a phase complete while any checklist item, sub-step, dependency, or exit criterion inside that phase remains incomplete.
- Fix validation failures before marking a phase complete or creating a commit.
- Never mark a phase complete before the relevant code and validation are done.
- Never say the work is complete without concrete validation evidence. "Should work" is not enough.
- After each implementation, proactively inspect nearby code, same-module patterns, impacted dependencies, and obvious boundary cases before closing the phase.
- Verify the happy path yourself before asking the user to validate it.

## Validate With Evidence

Adopt a close-the-loop mindset inspired by `tanweai/pua`: no evidence, no completion.

- Use real validation that matches the change, such as `cargo test`, `cargo build`, `uv run --project bindings/python pytest`, an example invocation, or another direct execution path.
- Record the exact validation commands or checks and whether they passed, failed, or were blocked.
- After each completed phase, run a proactive self-check:
  - verify the implemented behavior directly
  - check the same file or module for similar issues
  - inspect upstream and downstream impact
  - think through boundary cases and obvious regressions
- Before final completion, perform a full requested-scope verification pass, not just per-phase spot checks.
- If validation is blocked by the environment, missing credentials, or an external dependency, record the blocker in the plan and keep the affected phase incomplete unless the user explicitly accepts the gap.

## Check Completion

Treat a phase as complete only when all of the following are true:
- every checklist item, subtask, dependency, and exit criterion inside that phase is satisfied
- the planned code change for that phase exists
- the plan file reflects the new status accurately
- relevant tests, checks, or targeted validation have been run
- a commit was created for the phase, unless the user explicitly disabled commits

Treat the requested task as complete only when all of the following are true:
- every phase in the requested scope is complete; if `phase` was not provided, every phase in the plan is complete
- a final verification sweep across the requested scope has been run and its evidence is available
- the plan file status matches the repository state
- any blockers or deferred items are explicitly called out

## Report Results

When using this skill, report:
- which phase is being implemented
- which plan file is being used
- when the plan file was updated
- what validation was run
- what evidence was collected for phase-level and final verification
- what proactive post-implementation checks were performed
- which commit was created for that phase, or why no commit was made

## Reference Prompts

- Use `$plan-driven-implementation` with `plan=docs/private/rust-download-library-plan.md` and finish every remaining phase in the plan.
- Use `$plan-driven-implementation` with `plan=docs/private/python-pyo3-binding-plan.md` and only execute phase 2.
- Use `$plan-driven-implementation` with `plan=docs/private/logging-plan.md`, update the plan after each completed stage, and commit each stage separately.
