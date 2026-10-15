# 0014. Separate development completion from final review

Status: Proposed
Date: 2026-10-15
Supersedes: none
Superseded-by: none

## Context

Shiftwork already separates coding, local gates, hosted required checks,
independent review and protected merge. Todoq lands passing work on a shared
shift branch. A live shift's pull request stays draft, then becomes eligible
for review after the shift ends. `pr-redteam` skips drafts by default and
reviews an exact head independently of the author.

The development feedback loop ends too early. A driver gate failure returns
the task to the queue and consumes an attempt rather than returning the failure
to the coding agent within the attempt. Useful incomplete work also consumes
the attempt budget. Partial work can reach a shift PR while the task still has
unmet requirements; the end of a shift does not establish task completion.

Work item `20261014-052023-b4deca` exposed both problems. A partial first attempt
and a second attempt with a missing migration registry entry exhausted its
two-attempt allowance. The agent's command reported success despite failed
tests in its log; the driver's independent gate correctly refused the change.
The second implementation also left requirements unfinished. Repairing the
test failure alone would not have completed the task.

## Decision

Use two logical gates: development completion and independent final review.
They reuse repository checks and the existing forge merge authority.

The development controller owns a bounded implement, check and repair loop.
It preserves the work, full unmet-requirement list and gate evidence across
continuations. A gate failure returns actionable evidence to the worker before
consuming another whole-task attempt. Cost, elapsed time, repeated failures,
lease ownership and continuation counts bound the loop independently.

A task is ready only when its scoped acceptance criteria have evidence, no
implementation requirement remains unmet, and its required checks pass on the
candidate being submitted. Local focused checks help iteration; they do not
mint protected required results. Missing deployment or activation is recorded
as a dependent delivery step when it belongs outside the worker's authority.
It is never silently counted as completed delivery.

For newly enrolled work, use one automation-owned draft PR per task per
repository, linked by a change-set id across repositories. The controller owns
publication and readiness; the coding agent does not receive merge, deployment
or check-publishing authority. Existing shared shift PRs finish under their
existing policy during migration. A worker never rewrites another author's
branch or changes a human's deliberate draft or ready state.

`pr-redteam` remains the independent final reviewer. It examines requirements,
correctness, security, failure behavior, test adequacy and effects outside the
intended scope. It consumes trustworthy check evidence rather than duplicating
every build. Actionable findings return to the same task and PR for repair;
owner-only decisions become explicit, scoped questions. Review approval never
serves as an implicit waiver of a failed required check.

Repositories own canonical check definitions; the runner records the exact
candidate, checked policy and inputs, commands, results and evidence. Gate
definition changes require review and cannot silently weaken the checks that
judge their own change. Only authorized runner evidence counts toward branch
protection. Reuse requires matching input identity and producer authority.

The forge's merge passport and queue remain the final enforcement boundary.
They require applicable review and required checks for the exact merge path.
A changed head invalidates its prior approval; replay onto a changed base
requires the existing queue checks. This does not remove a queue gate or
serialize every check behind review. ADRs 0003 and 0013 continue to govern
runner placement and truthful representation of parallel checks and review.

## Consequences

Small implementation failures become ordinary development iterations, while
final review spends its attention on completed work. Separating completion
from merge and deployment prevents a green test suite from being treated as
proof of the entire request.

Task PRs increase PR count and require durable ownership, publication recovery,
readiness evidence and concurrency control. Cross-repository change sets need
ordered dependencies and compatible intermediate releases; several PRs cannot
be merged atomically merely because they share an id. Draft builds need bounded
scheduling so active editing does not starve merge-queue work.

The rollout starts with reliable gate results and bounded repair in the current
shift flow. Task PRs and automatic readiness follow only after their state and
recovery contracts are tested. Implementation order, acceptance tests and
rollout controls are in the [implementation plan](../shiftwork-gates-plan.md).
