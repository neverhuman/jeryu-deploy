# Merge queue

Status: design, for review before implementation (2026-09-18).

## Problem

Protected branches require linear history, so `merge_pull` only fast-forwards:
a pull request whose head is not a descendant of the base tip gets `409
NonFastForwardRequired`. Every merge therefore turns every other open PR on the
same base into a rebase, a fresh exact-head gate and another review pass. On
2026-09-18 that cost full cycles on jeryu-deploy #50→#52, #23→#26 and
jeryu-web #5, #6 and #14. Nobody wrote new code in those cycles; people and
agents just rebased and waited.

## What the queue does

An approved PR whose only blocker is "not a fast-forward" joins a per-base
queue. The forge then does the rebase, gates the result at its exact sha, and
fast-forwards the base to it. Nobody rebases by hand, and the base still only
ever moves to a commit that passed the gate.

```
approved PR ──enqueue──▶ [queue: base=main] ──build──▶ refs/queue/main/<n> = replay(PR onto main tip)
                                                     │
                              runner gates that sha ─┘──▶ <repo>/required on the queue sha
                                                     │
            success ──▶ CAS fast-forward main to the queue sha ──▶ PR merged at that sha
            failure ──▶ dequeue, comment with the gate log; PR stays open
            conflict ─▶ dequeue, comment "rebase needed: <paths>"; PR stays open
            base moved before landing ──▶ rebuild the entry on the new tip
```

## Invariants (unchanged by the queue)

1. **Exact-sha gating.** The base only fast-forwards, with compare-and-swap, to
   a sha whose `<repo>/required` status is `success`. The queue sha is gated
   like any PR head. No status is carried from the PR head to the queue sha.
2. **Linear history.** The queue sha is the PR's commits replayed on the tip, so
   landing it is a fast-forward (the existing `merge_pull` path).
3. **Same post-update path.** Landing runs `ci_bridge::on_push` and
   `finalize_merge`, exactly as a direct merge does today.

## The one policy change: approval carries across a clean replay

Reviews bind to the PR head sha. The queue sha is a different sha with the same
changes on a newer base. The proposal is that **an approval of the PR head
authorizes its clean replay**. That holds only when:

- the replay applies with no conflict, commit by commit (`git replay`/cherry-pick,
  never `-X ours/theirs`);
- the diff of the queue sha against its base equals the diff of the PR head
  against its merge-base: the same set of paths with the same status (added,
  modified, deleted, renamed, including rename targets), then the same hunks per
  path. A replay that drops, adds or renames a file is refused even if every
  remaining hunk matches;
- no new `changes_requested` review exists on the PR head.

The queue records the pair `(pr_head_sha, queue_sha)` and **who** approved: each
approving review's login and whether it is an automation identity (for example
`pragent`) or a person. The landed merge is therefore traceable to what was
reviewed, and by whom. GitHub's merge queue makes the same trade.

A setting per protected branch decides who may enqueue:

| setting | enqueue needs | fits |
|---|---|---|
| `approval` (default) | any effective approval, bot or human | low-risk repositories with automated review |
| `human-approval` | at least one approval from a non-automation identity | repositories where model review alone is not enough protection |
| `reapprove-queue-sha` | a fresh approval of the queue sha itself | the strictest branches |

## Runner contract (needs the gate-runner owner's agreement)

- `GET /api/v1/merge-queue?state=building` lists entries:
  `{repo, base, number, queue_ref, queue_sha, pr_head_sha, enqueued_at}`.
- The runner gates `queue_sha` from `queue_ref` exactly as it gates a PR head,
  with the same recipe and tree layout. It posts `<repo>/required` on `queue_sha`.
- Queue entries rank ahead of ordinary PR heads: they are one step from landing.

## API

- `POST /api/v1/repos/:id/pulls/:number/queue` enqueues, and is idempotent. It
  requires the PR to be approved with no changes requested; `pr-redteam` or a
  human calls it instead of merging.
- `DELETE …/queue` dequeues. `GET /api/v1/repos/:id/merge-queue` shows the queue.
- Merging a PR that already fast-forwards still merges directly. The queue is
  only needed when the base has moved.
- `GET /api/v1/repos/:id/pulls/:number/merge-attempt` answers the last merge or
  enqueue attempt on the PR (`result`, HTTP `status`, forge `code` and
  `message`, `actor`, `at`), a `blockedReason` line when it was refused, who
  approved the head, and a `grantGap` when the merge identity
  (`JERYU_MERGE_IDENTITY`, default `jain-merge-bot`) has no write grant on the
  repository. The forge records every answer, including the auth gate's 403,
  so a refusal no longer lives only in the merger's journal. `/runners` joins
  the same record onto the reviewer's last approval (`lastActivity.mergeAttempt`)
  and lists `mergeGrantGaps` for the repositories it is reviewing.

## Scope of the first cut

- One entry building per base at a time (no batching or speculation yet).
  Throughput is then one gate per landed PR, which is what happens today minus
  the human rebase round trips.
- Entries persist in the forge database (append-only history, like deployments)
  so a restart resumes the queue.
- The Releases views can show "queued / building / landing" per PR.

## Failure handling

A failed queue gate is retried **once** on the same queue sha. That absorbs the
environment flakes seen on 2026-09-18 (jeryu-deploy#18 failed two exact-head
gates before passing). Both attempts' logs and conclusions are recorded on the
entry, so a real failure is never hidden behind the retry. A second failure
dequeues the PR with a comment that links both logs.

## Review record

1. Approval across a clean replay: accepted by the `pr-redteam` owner for a
   diff-identical replay, since its mechanical holds judge only the diff. That
   acceptance came with the approver-identity record and the `human-approval`
   branch setting above, and with path-level equality.
2. Failed gate: retry once, recording both logs (above).
3. Transport: polling `GET /api/v1/merge-queue` is enough for `pr-redteam`,
   which only enqueues. The runner owner's preference is still open.

## Implementation note: where queue state lives

The first cut stores queue state in each repository, not in jeryu-core:
`refs/queue/<base>/<n>` holds the queue commit, and `refs/queue-meta/<base>/<n>`
points at a JSON blob (PR head, base, approvers, attempts, state). The forge
keeps an in-memory index rebuilt from these refs on first use, so a restart
resumes the queue without a core schema change or a core release. Clients
cannot push either namespace: `git-receive-pack` refuses them with 403.

Entries are advanced by a worker every 10 seconds: a moved PR head dequeues, a
moved base rebuilds, a failed gate rebuilds once (a fresh commit) and then
fails, and a green queue commit lands through the same compare-and-swap
fast-forward as a direct merge. If a repository declares no required
contexts, the queue commit needs every context reported on it to be green,
and at least one.

Endpoints: `POST`/`DELETE /api/v1/repos/:id/pulls/:n/queue`,
`GET /api/v1/repos/:id/merge-queue`, `GET /api/v1/merge-queue?state=building|all|landed|failed|dequeued`.

Moving the store into jeryu-core (for history queries and the PR journey view)
is a follow-up.

## Gate latency: head gate vs queue gate (2026-09-19)

Every change is gated twice today: once at the PR head, once at the queue
commit. The queue gate is the authoritative one.

### What enqueue requires today

`POST …/pulls/:n/queue` runs `pulls::queue_gate`, which is the full merge
passport on the PR head: every required context **green on the head**, the
required approvals, no changes requested, no unresolved threads. `advance`
checks the same passport again on every tick. Landing (`land_queued`) goes
through jeryu-core `evaluate_merge_readiness(pr_head)`, and that also checks
required statuses on the **head** sha, not on the queue sha. `pr-redteam` asks
for the queue only after a direct merge answers 409 "linear history", and it
only tries a merge when `mergeable.can_merge` is true. So it also waits for the
head gate.

### Proposal: enqueue on approval (not implemented, needs a decision)

For branches whose required contexts are gated on the queue commit, allow
enqueue on approval alone and drop the green-head requirement. The queue gate
stays mandatory, and the landing rule does not change: only a green gate on the
exact queue sha lands. Put it behind a per-branch switch that is off by default.
Doing this needs three changes that belong to other owners:

1. **jeryu-core**: a readiness check for queue landing that evaluates required
   contexts on the queue sha and reviews on the PR head. Today core refuses to
   land a PR whose head has no green required status. jeryu-deploy consumes core
   by a split tag, so a later todo has to bump the tag after this change.
2. **pr-redteam** (jeryu-ci-runner): enqueue on approval without waiting for
   `can_merge`, when the switch is on for the branch.
3. **Gate runner**: stop gating PR heads on those branches, or rank them below
   queue commits. Otherwise the head gate still uses runner capacity.

### Fast-forward PRs: no second gate

If the PR head already contains the base tip, `replay` returns the head sha
itself as the queue commit. It is not a copy with the same tree. It is the same
commit, so the "queue gate" reads the result the runner already posted on that
sha. Same tree, same gate inputs, same runner source, and nothing is gated
again (`a_fast_forward_pr_reuses_its_head_result_for_the_same_commit`).
`pr-redteam` does not queue such a PR anyway, because a direct merge succeeds.
A replay onto a moved base is a new sha and never borrows the head's result,
even when the trees would match (`a_replay_never_borrows_the_head_result`).
Reusing a result across different shas or gate inputs stays out of scope. The
formal reviewer rejected it ("exact source authority", jain-deploy#55 v1).
