# Shiftwork development and final review implementation plan

Status: Proposed
Date: 2026-10-15
Decision: [ADR 0014](adr/0014-separate-development-completion-from-final-review.md)

Build a reliable development loop around the current worker first, then move
newly enrolled work to task-owned draft PRs whose readiness means the task is
complete. Keep `pr-redteam` as the independent final reviewer and retain the
forge's existing protected merge path. Each phase below is separately useful
and can land without enabling later phases.

## Current flow and concrete gaps

This baseline is source behavior as of 2026-10-15: Todoq `35be6f2`,
`jeryu-ci-runner` `ac1e39f`, and `jeryu-deploy` `9e170d0`. Source availability
does not establish which runner version a remote host has installed; the pilot
must record installed identities as well.

| Component | Current behavior | Change needed |
| --- | --- | --- |
| Todoq `todoq/todoq/worker.py`, `_attempt`, `_gate`, `_sh` | Agent returns once; driver stamps provenance and gates touched repositories. First failed gate returns `retry`. | Feed failed checks back into a bounded continuation, preserving the same claim and work. |
| Todoq `_record`, `completion.py` | Incomplete work and failed attempts count toward the attempt limit. The gate-failure return drops the current result's unmet list from the outcome. | Preserve completion evidence on every outcome and distinguish progress, repair and repeated failure. |
| Todoq `_prepare`, `_land`, `_land_repo` | Private work lands on a shift or batch target; unlanded work is preserved on rescue branches. | Add an opt-in task PR publication mode; retain rescue behavior on every exit. |
| Todoq `shiftpr.py`, `shiftrefresh.py` | Live shifts are drafts; closed shifts can be readied. Human draft holds are recognized. | Task readiness must use acceptance and check evidence, not the shift clock. Keep legacy shift handling separate. |
| Runner `ops/pr-gate/bin/pr-gate-runner.sh` | Selects open PR heads, runs repository gates, retains receipts and publishes protected results. Its candidate loop has no draft exclusion. | Prove draft execution end to end; add bounded scheduling and explicit development-phase attribution. |
| Runner `ops/pr-redteam/pr-redteam` | Skips drafts, reviews an exact head, consumes quality proof, and merges eligible approvals through the forge. Optional fix filing targets selected authors; current main's repair text describes a replacement draft PR. | Dispatch structured repair to the existing managed task and branch; review only completed managed candidates. |
| Deploy `web/pulls.rs`, `web/regate.rs`, `web/merge_queue.rs` | Owns draft/ready actions, regate requests and merge queue integration. The documented regate request currently rejects drafts. | Support authorized draft regating and conditional managed readiness without loosening merge checks. |
| Deploy Flow and Web surfaces | Show work, checks, review, merge and delivery, with parallel checks and review. | Show development repair, review repair, waits and delivery handoffs as distinct recorded states. |

The local Todoq `family.toml` gate is currently a command body, while the
repository and protected runner have their own entry points. Enrollment must
identify any differences before treating their results as equivalent. A local
`todoq/gate` diagnostic check is not a protected required context.

The motivating run is [work item 20261014-052023-b4deca](https://git.jeryu.ai/work/20261014-052023-b4deca).
Its second attempt failed two migration registry tests after adding
`0022_pull_owner_decisions.sql`. It also reported unmet functionality. The
regression case must demonstrate both repair of that defect and refusal to
declare the unfinished feature ready.

## Responsibilities and evidence

| Owner | Responsibility | Must not grant |
| --- | --- | --- |
| Repository | Canonical required gate, focused checks and acceptance fixtures | A candidate's unilateral removal of required checks |
| Todoq controller | Task claim, worker continuation, publication, readiness and repair budgets | Self-approval, protected check publication or production deployment by the coding agent |
| Gate runner | Execute checks and publish independently attributable results | Review approval or merge waivers |
| `pr-redteam` | Independent semantic review, structured findings, scoped decision requests | A green verdict inferred from an agent summary |
| Deploy and core forge policy | Permissions, durable transition evidence, merge passport and queue | Merge based on a stale head or caller-supplied success |
| Release controllers | Pinning, staging, activation and delivery verification | Declaring task delivery from merge time alone |

Use the existing runner receipt and input-identity machinery. Extend it where
necessary rather than introduce a second trusted evidence publisher. A gate
receipt needs repository and head, relevant base and dependency revisions,
policy/recipe identity, tool/environment identity, producer, run id, start/end,
per-step exit codes, conclusion and retained log references. Timeout, crash,
missing steps and malformed output are failures or infrastructure errors,
never success. Focused-check receipts explicitly identify their limited scope.

Each task stores an acceptance revision and criteria with evidence references,
the latest complete unmet list, per-repository branch/PR/head bindings, current
claim generation, phase, counters and deadlines. Readiness refers to a frozen
candidate and acceptance revision. Code changes or changed acceptance criteria
invalidate it. PR prose and the coding agent's JSON are inputs to evaluate,
not authority to publish a protected result.

## Lifecycle

The phases below are proposed controller phases, not new values to insert into
existing wire enums without versioning their consumers.

| Phase | Exit condition | Recovery or hold |
| --- | --- | --- |
| Developing | Candidate committed; checks requested | Preserve edits and claim checkpoint on interruption |
| Checking | Required scope passes for the candidate | Code failure returns to development; infrastructure failure uses bounded backoff |
| Ready | All scoped criteria evidenced, no implementation unmet, current required results, ownership unchanged | Missing evidence stays draft; human hold remains untouched |
| Reviewing | Independent verdict on the submitted head | Actionable findings return to repair; owner decisions wait explicitly |
| Repairing review findings | Findings addressed and development checks pass again | Same task, PR and branch; cap repeated findings and total spend |
| Merge eligible | Forge passport and applicable queue evidence authorize the current merge | Head/base/policy movement triggers the existing invalidation and recheck path |
| Merged | Verified merge recorded | Delivery steps continue separately until their own evidence exists |

Checks may execute while final review is running when current policy permits;
both must satisfy the merge authority. This is two logical gates, not a demand
to turn the whole Flow graph into a serial pipeline. Existing queue replay
rules and their required checks remain in force.

## Implementation sequence

### Phase 1 Return reliable failures to the current worker

Owner: Todoq. Touch `worker.py`, `cli.py`, `completion.py`, worker/session
prompts and the corresponding existing tests.

1. Extract one gate execution interface used by the driver and a worker-facing
   command. Resolve enrolled gate policy outside candidate control and report
   per-step results. Legacy command bodies run as explicit child processes;
   do not infer a whole gate's status from a trailing `echo`, `tail` or later
   successful command. Migrate enrolled command bodies to canonical repository
   entry points with an explicit required-step contract.
2. Carry the current summary, full unmet list and acceptance evidence through
   success, gate failure, timeout and blocked outcomes. Keep detailed evidence
   in durable records; a shortened UI note must not truncate the source record.
3. On a driver gate failure, retain all touched repositories and invoke a
   repair continuation with the failed receipt, failure digest and full log
   access. Resume the session when supported; otherwise start a continuation
   against the retained checkout and checkpoint. Revalidate claim ownership
   before continuation and before any publication.
4. After repair, collect changes and provenance again. Invalidate evidence for
   every changed repository and relevant dependency; run required gates and
   audits before landing. A repair can change more than the originally failed
   repository. No previously passing result may be reused after its inputs move.
5. Add separate persisted repair and continuation limits. Suggested pilot
   limits are two gate-repair continuations, four review-repair rounds, and a
   stop after the same normalized failure recurs without evidence of progress.
   Existing task spend and elapsed-time bounds remain ceilings. Counter resets
   require an explicit action; restarts and model switches do not reset them.

Acceptance: a fixture where an early command fails and a later command succeeds
reports failure; the missing-migration-row case repairs within one task attempt;
its unrelated unmet criteria remain present and prevent completion. Prove loss
of lease, crash/restart and repair that breaks a previously passing repository.
No PR topology or merge authority changes in this phase.

### Phase 2 Persist task progress and completion separately

Owners: Todoq and Deploy's Shift read model. Extend Todoq records compatibly,
then `crates/jeryu-api/src/web/shift/` and relevant tests before UI consumers.

Store the acceptance revision, completion evidence, publication bindings,
continuation counters and blocked reason as versioned fields. Keep existing
task status values readable during rollout. Distinguish implementation ready,
review approved, merged and delivered rather than making `done` imply all four.

Incomplete progress may continue within its limits; it does not automatically
consume the same counter as a repeated implementation failure. Missing repo
enrollment, credentials or deployment authority is an explicit dependency or
handoff. These conditions must not cause repeated model runs with identical
inputs. Outages receive bounded backoff and eventually a visible operator hold.

Acceptance: an old task still reads and schedules correctly; new state survives
restart; changed acceptance invalidates readiness; passing tests with unmet
criteria cannot mark a task ready; dependency handoffs remain visible until
their evidence arrives. Compare state with the parent task so splitting work
cannot conceal unfinished acceptance criteria.

### Phase 3 Add task-owned draft PR publication

Owners: Todoq; Deploy for conditional PR state transitions. Introduce a new
opt-in landing policy rather than reinterpret existing `shifts` records.

Create one stable branch and draft PR per task per affected repository. Bind
them durably to the task and change set. The controller may publish committed
work that fails development checks to its owned draft branch so the failure
and progress are visible; it may not land that work on the shared shift target
or protected main. Open the PR once useful committed work exists and update it
through compare-and-swap publication against the last observed head.

Reuse the draft-origin and human-hold rules in `shiftpr.py`. Add a conditional
managed-ready operation binding expected head, acceptance revision and current
required evidence. A concurrent push or human state change must refuse the
transition. The ordinary human-ready path stays available. Never use an author
login alone as proof that automation owns a human-authored branch.

Reconcile uncertain API/push outcomes before retrying: a restart after branch
publication or PR creation must recover the same PR. An externally moved
branch, closed PR or merged PR stops publication and raises a typed conflict;
do not overwrite it or push into a branch whose PR has merged. Use approved
recovery policy for replacement PRs rather than guessing ownership.

Cross-repository tasks carry a set of candidate heads and explicit dependency
order. Require compatible intermediate states and each repository's own
checks/review. Where one repository consumes an immutable release of another,
stage the provider merge/release and consumer pin work as linked tasks. Never
claim that a change-set id gives atomic multi-repository merge.

Acceptance: repeated runs and uncertain responses create one PR; an unfinished
task remains draft after its original shift ends; a completed task need not
wait for shift end; human holds and concurrent writers are preserved; existing
shift PRs are neither rewritten nor silently enrolled.

### Phase 4 Connect draft checks to final review and repair

Owners: `jeryu-ci-runner` (`ops/pr-gate/`, `ops/pr-redteam/`), Todoq dispatch and
publication, and Deploy (`web/regate.rs`, `web/pulls.rs`).

Exercise the existing runner's open-PR selection with draft fixtures before
changing selection. Add authorized draft regate support to the API, which
currently refuses it. Debounce rapid pushes, deduplicate by candidate/input
identity, cancel superseded work where safe, and reserve capacity for merge
queue and ready work. Report waiting separately from active execution.

Keep protected result publication with the runner. Matching local check
evidence is diagnostic unless it was produced through that trusted execution
boundary. Review consumes existing authoritative results; reuse is permitted
only when all relevant inputs and producer authority match. A new queue replay
commit retains its own required proof obligation.

Replace repair prose with a validated dispatch contract: task/change-set id,
repository, PR, expected head, branch ownership, acceptance revision, structured
findings, finding fingerprint and round. The controller rechecks the live PR
before editing and pushing. Deduplicate uncertain dispatches. For managed PRs,
wait for completion evidence before `pr-redteam` reviews; ordinary PRs keep
their existing policy. Retain the default draft skip and scope any explicit
early-review override as advisory rather than readiness or merge permission.

Actionable review findings resume the same work loop. Only controller-owned PR
state can be returned to draft automatically; a human-readied PR uses a repair
hold without reversing the human's action. Required checks and new-head review
must finish before eligibility is restored. An owner answer records only the
exact decision and head it authorizes; it does not waive unrelated findings or
required checks. Existing audited override policy remains a separate authority.

Acceptance: no automatic final review of incomplete managed work; one repair
dispatch per findings/head; same PR repaired without force-push; new code gets
fresh review; repeated findings stop at the configured bound; malformed review
output, stale approvals and review/check self-authorization cannot merge.

### Phase 5 Expose phases and prove delivery in a pilot

Owners: Deploy's pipeline/Flow/Shift projections, `jeryu-web`, and the operator
release path. Add durable events at the controller boundaries, not inferred
from prose or a timer's existence. Version event payloads and keep old readers
working while producers and consumers roll out.

Show the task, PR, checks tab and Runners: developing, waiting for checks,
checking, repairing with round and failure, ready, reviewing, waiting for owner,
merge queue and merged. Preserve both the latest unmet list and failed-check
evidence. Link continuation histories so restarts do not erase spend or waiting.
Extend existing Flow records and ADR 0013's parallel model; do not rebuild its
history or label a wait as active work.

Pilot one enrolled task in one repository first. Prove build failure, repair,
completion, independent review, review repair, fresh checks and protected merge
without manual requeue. Then prove a linked multi-repository case. Use an
isolated integration forge for deterministic failure injection before an
operator enables the production pilot. Deployment and activation remain on
the existing release path, with explicit receipts when the task requires them.

## Rollout and rollback

Land Phase 1 behind a disabled-by-default repair setting, run its offline suite,
and enable a bounded pilot through the normal reviewed installation path. Do
not have an unattended worker edit its running driver or host service settings.
Introduce phase/state schemas before their producers. Enable task PR mode only
for new tasks after Phase 3; inventory open shift PRs and finish them through
the existing shift path. Add review dispatch only after ownership, readiness
and idempotency tests pass.

Rollback disables new task enrollment and automatic repair/readiness. Keep
records, PRs, branches, receipts and spend; allow active claims to checkpoint
and drain. Existing protected checks and final review continue. Never roll back
by marking unfinished tasks complete or removing their failing contexts.

Before rollout, record required gate definitions and installed driver/runner/
reviewer versions for the pilot. Measure first-failure-to-repair time, tasks
blocked after fixable failures, repeated findings, owner interventions, check
reuse and total cost per completed task. More attempts alone is not success.

## Completion criteria for this program

The migration-registry incident is recoverable without a human releasing the
task, and remaining requirements still prevent premature readiness. A complete
managed task advances through draft checks, independent review and protected
merge on one PR; review findings return to that same work without losing state.
No worker can manufacture required evidence, approve its own work, override a
human hold or change production services. Delivery is complete only when the
task's required deployment evidence exists.

Suggested implementation PR order is Phase 1 gate execution, Phase 1 repair,
Phase 2 state/read model, Phase 3 publication, Phase 3 readiness, Phase 4 draft
regating/scheduling, Phase 4 review dispatch, then Phase 5 visibility and pilot
evidence. Each PR needs its phase's negative tests plus its repository's full
required gate. The pilot limits and task-PR enrollment scope are operator
settings to settle before activation, not reasons to delay the first repair
and evidence changes.
