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
  against its merge-base, path by path and hunk by hunk;
- no new `changes_requested` review exists on the PR head.

The queue records the pair `(pr_head_sha, queue_sha)` and the approving reviews
on the PR, so the landed merge is traceable to what was reviewed. GitHub's merge
queue makes the same trade. If reviewers want stricter behaviour (re-approval of
the queue sha), that is a setting per protected branch, off by default.

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

## Scope of the first cut

- One entry building per base at a time (no batching or speculation yet).
  Throughput is then one gate per landed PR, which is what happens today minus
  the human rebase round trips.
- Entries persist in the forge database (append-only history, like deployments)
  so a restart resumes the queue.
- The Releases views can show "queued / building / landing" per PR.

## Open questions for review

1. Is carrying approval across a clean replay acceptable for protected `main`,
   or should the default require re-approval of the queue sha?
2. Should a failed queue gate dequeue immediately, or retry once to absorb a
   known flake?
3. Does the runner prefer polling `GET /api/v1/merge-queue` or a webhook event?
