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
`review-bot`) or a person. The landed merge is therefore traceable to what was
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
  `state` is `building` (the default), `landed`, `failed`, `dequeued` or `all`;
  anything else answers `422 invalid_query` naming those, because a runner
  polling a misspelled state would read an empty queue as nothing to gate.
  The listing is paged like every other `/api/v1` collection
  (`docs/pagination.md`): `limit` (alias `per_page`) and `page`, with `total`
  and a `page` object carrying `has_more`. A runner that gets `has_more` walks
  the rest of the pages before it decides the queue is drained.
- The runner gates `queue_sha` from `queue_ref` exactly as it gates a PR head,
  with the same recipe and tree layout. It posts `<repo>/required` on `queue_sha`.
- Queue entries rank ahead of ordinary PR heads: they are one step from landing.

## API

- `POST /api/v1/repos/:id/pulls/:number/queue` enqueues, and is idempotent. It
  requires the PR to be approved with no changes requested; `pr-redteam` or a
  human calls it instead of merging.
- `DELETE …/queue` dequeues. `GET /api/v1/repos/:id/merge-queue` shows the
  queue, paged the same way (`limit`/`per_page`, `page`).
- Merging a PR that already fast-forwards still merges directly. The queue is
  only needed when the base has moved.
- While the queue is building an entry on a linear-history base, a direct
  merge onto that base is refused with `409` ("requires linear history, and the
  merge queue is landing #N"), even a clean fast-forward. Moving the base under
  a running queue gate would make that entry rebuild and gate again (19 of 186
  entries did, 2026-09-19 to 10-15). pr-redteam reads the "linear history"
  refusal and enqueues, so the PR lands after the entries ahead of it.
- `GET /api/v1/repos/:id/pulls/:number/merge-attempt` answers the last merge or
  enqueue attempt on the PR (`result`, HTTP `status`, forge `code` and
  `message`, `actor`, `at`), a `blockedReason` line when it was refused, who
  approved the head, and a `grantGap` when the merge identity
  (`JERYU_MERGE_IDENTITY`) has no write grant on the
  repository. The forge records every answer, including the auth gate's 403,
  so a refusal no longer lives only in the merger's journal. `/runners` joins
  the same record onto the reviewer's last approval (`lastActivity.mergeAttempt`)
  and lists `mergeGrantGaps` for the repositories it is reviewing.

### A refused enqueue is stored, not only answered

A refusal (`queue_conflict`, `queue_merge_commits`, `queue_mismatch`,
`queue_git_error`) builds no queue commit, so the entry is written as
`dequeued` with `refusal_code` set to the forge code before the `409` goes out.
The entry also carries the two facts a replacement pull request is built from: the
base commit the replay was refused against (`base_sha`, which no attempt recorded
because none was built) and, for a conflict, `conflict_paths` — the paths the
replay conflicted in, the same ones the reason line names in prose, as data, so a
reader preparing the replacement does not parse that sentence. Up to 64 paths; a
conflict wider than that is a rebase nobody reads a path list for.
pr-redteam reads this entry and files the replacement work as a rework task
(`REDTEAM_QUEUE_REWORK`), so a conflict no longer waits for a person.
The attention inbox walks stored entries, and without the record the refusal
lived only in the response and the `queue.refused` event: ten minutes later the
inbox called the pull request "ready to merge" and offered a merge the forge
refuses for the same reason. It now raises `queue_refused` naming the one step
per code — a replacement pull request from the base with the commits
cherry-picked for a conflict or merge commits, a new head for a diff the replay
did not reproduce. An entry still `building` after 30 minutes raises
`queue_stuck`, and while an entry is building no `pr_ready_to_merge` is raised
for that pull request: the queue is what merges it.

## Stacks: one gate can land several PRs

Entries queued on the same base form a stack, first queued at the bottom. The
bottom entry is replayed onto the base tip; each entry above it is replayed
onto the queue commit of the entry below (`after` names that entry). Every
entry's queue commit therefore holds the base plus every PR up to and
including its own, and its gate proves exactly that.

```
main ── q#1 ── q#2 ── q#3          q#n = refs/queue/main/<n>
        (#1)   (#1+#2) (#1+#2+#3)
```

Each tick, per stack:

- **Restack.** Entries whose parent is no longer the base tip or the queue
  commit below them are rebuilt on it, bottom up. A rebuild for that reason
  marks the replaced attempt `superseded`, which counts against nobody's retry.
- **Land.** The highest entry whose queue commit is green on the union of the
  required contexts of itself and every entry below it lands together with
  them (a pull request can require more than the base does, such as
  `jankurai/proof`): the base moves once, by compare-and-swap, straight to that
  entry's queue commit, and each PR is recorded merged at its own queue
  commit (an ancestor of the new tip). An entry landed this way carries
  `landed_with` (the entry whose gate covered it) and an attempt conclusion
  `covered by #N`, or `failure, covered by #N` when its own gate was red. The base never rests on a commit without its own green gate.
  A green top lands an entry below it even if that entry's own gate was red:
  the top commit holds that change and passed with it.
- **Fail.** With nothing green, a red bottom entry is retried or failed
  (below), and the entries above it are restacked onto what remains. A red
  gate higher up is not charged to its entry: that commit carries every
  change below it, so it waits until the entries below it have a verdict.
- **Retry.** A landing that could not start for a reason no pull request
  caused (a ref that did not resolve, a lost ref lock) is tried again on the
  next tick; an entry still building after 30 minutes raises `queue_stuck`.
  If the base moved but the forge could not record a member's merge, that
  member fails with a reason naming the base commit its change is on.

A pull request that conflicts with the queue commit of an entry ahead of it,
but replays cleanly onto the base tip, is refused as `queue_conflict_queued`:
its branch is fine, and it can be queued again once that entry lands or
leaves.

When the bottom entry lands first, the entry above it already sits on the new
tip, so its queue commit and its running gate stand: no rebuild, no second
gate. Before stacks, every entry was built on the tip, and each landing made
every other queued entry rebuild and gate again (19 of 186 entries,
2026-09-19 to 10-15). The gate runner needs no change: it gates every building
entry's `queue_sha`, as before.

Entries persist in the forge database (append-only history, like deployments)
so a restart resumes the queue, and the stack order (`enqueued_at`, then
number) with it.

## Failure handling

A failed queue gate is retried **once** as a fresh commit on the same parent.
That absorbs the environment flakes seen on 2026-09-18 (jeryu-deploy#18 failed
two exact-head gates before passing). Both attempts' conclusions are recorded
on the entry, so a real failure is never hidden behind the retry. A second red
gate fails the entry; only the entry's own red gates count toward that, never
a rebuild because the base or the queue below it moved.

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

`GET /api/v1/repos/:id/merge-queue` also gives each building entry
`gate_started_at`: the earliest status or check run on its queue commit among
the base's required contexts (any context when it requires none), posted at or
after `enqueued_at`, or `null` while no gate runner has taken the entry. Reports
from before the enqueue never count: a PR already on the base tip is queued as
its own head, which carries the PR's earlier CI. The all-repository listing
omits the field, because the runner polls that listing on every tick and it
stays an index read.

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

### Drafts

A draft pull request cannot be queued, and the refusal says so on the pull
request's own timeline as one `pr.skipped` event per head
(`detail.automation = "merge-queue"`, `detail.skipped = "draft"`) rather than
only in the response body. `pr-redteam` reports its own draft skip the same
way. Marking a draft ready for review is the author's or an admin's move:
`POST /api/v1/repos/{id}/pulls/{number}/ready` (and `/draft` back again). The
`gh`-compatible `PATCH /api/v3/repos/{owner}/{repo}/pulls/{number}` with
`{"draft": false}` does the same thing and lands on the same event.

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
formal reviewer rejected it ("exact source authority").

## The direct merge may not replay past the gate (2026-09-28)

`merge_ready_pull_git` replays a diverged head onto a linear-history base so a
mergeable PR still lands. That replay is a new sha, and the gate ran on the PR
head, so on 2026-09-28 jeryu-web #63 and #64 and jeryu-core #17 put a commit on
main with no `<repo>/required` at all: auto-pin, which needs a green status on
exactly the web main commit, logged "gate is pending; waiting" forever, and the
crate commits to be tagged carried no gate evidence. It took an empty pull
request whose head was main (jeryu-web #65, jeryu-core #18) to get that exact
commit gated.

Invariant 1 above therefore binds the direct merge too. When the base declares
required contexts and the replay's sha does not carry them green, the merge is
refused with `409` naming what is missing, and the pull request goes through the
queue, which builds the replay, has it gated at its exact sha, and only then
fast-forwards the base. A base that declares no required contexts has no gate to
wait for, and that path is unchanged.

## The head branch is deleted after a merge (`delete_branch_on_merge`)

Every path that records a real merge (a direct merge, a head already in the
base, a base seeded at the head, and a queue landing) then deletes the pull
request's head branch, the way GitHub's `delete_branch_on_merge` does. Merged
branches used to stay until somebody swept them by hand, and nothing did.

It is on for every repository unless an administrator turns it off, in GitHub's
spelling: `PATCH /repos/{owner}/{repo}` with `{"delete_branch_on_merge": false}`
(so `gh repo edit --delete-branch-on-merge=false` works), read back on
`GET /repos/{owner}/{repo}`. The value is kept per repository id in
`shift.sqlite` (migration `0013_repo_merge_settings.sql`).

The branch is kept, and the reason reported, when its head lives in another
repository, when it is the base or the default branch, when it has branch
protection, when any pull request in the repository (in any state) merges into
it, which marks a long-lived line such as `release/5.0` or `develop` so a
back-merge out of it keeps it, or when another open pull request uses it as its
head. An open pull request in another repository is reported without its name.

Before the delete, the merged head is pinned at `refs/pull/<n>/head`, the ref
GitHub keeps for every pull request. A queue landing onto a moved base, or a
linear-history direct merge, lands a replay, so the gated head commit is not in
the base's history; the pin keeps that commit, and the pull request's diff,
reachable. A pin that cannot be written keeps the branch.

The delete is a compare-and-swap against the head that merged and goes through
the same ref policy as `git push --delete`, so a push made after the merge, or
a branch the server's protected-ref rules cover, survives. A direct merge's
response says what happened in `head_branch` (`{"name", "deleted", "reason"}`);
a queue landing logs a kept branch.

Automatic deletion writes a requested audit receipt before changing the branch,
then a completed or failed receipt naming the PR, ref and gated commit. An audit
request that cannot be stored keeps the branch. This cleanup affects the forge;
mirror branch deletion is a separate operation and is not attempted here. The
cross-repository safety scan keeps the branch if it exceeds 1,024 repositories
or 4,096 pull requests rather than deleting on incomplete evidence.
