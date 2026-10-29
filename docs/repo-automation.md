# What runs on a repository

A repository page used to describe the code and nothing that acts on it. The
checks lived under the GitHub-shaped Actions edge, the required contexts under
branch protection, the reviewer and the merger under `/runners`, the mirror
inside the reconcile loop, and the grants behind the admin routes. Nobody could
say, from the forge, what happens when a pull request merges.

`GET /api/v1/repos/:id/automation` answers that in one request. It is listed in
the route index (`GET /api/v1`) like every other `/api/v1` route, needs the same
read access as the rest of the repository, and reads only facts the forge
already holds.

## The answer

```json
{
  "repo": "acme/widget-www",
  "defaultBranch": "main",
  "checks": [
    { "name": "jankurai/proof", "required": true, "state": "reported",
      "lastConclusion": "success", "lastRunAt": "…", "lastHeadSha": "…",
      "detailsUrl": "…" },
    { "name": "widget-www/required", "required": true, "state": "missing" }
  ],
  "requiredContexts": ["jankurai/proof", "widget-www/required"],
  "actors": [
    { "kind": "reviewer", "identity": "review-bot",
      "grant": { "required": "write", "present": true, "held": "write" } },
    { "kind": "merger", "identity": "merge-bot",
      "grant": { "required": "write", "present": false,
                 "warning": "merge-bot has no write grant on acme/widget-www; its merges answer 403" } },
    { "kind": "gate-runner", "identity": "buildhost2/slot0", "state": "online",
      "lastRun": { "conclusion": "success", "sha": "…", "pr": 31, "at": "…" } },
    { "kind": "deployer", "identity": "buildhost1/publish", "state": "online",
      "lastRun": { "conclusion": "deployed", "target": "edge-pages", "sha": "…", "at": "…" } }
  ],
  "mirrors": [
    { "target": "https://github.com/acme-oss/widget-www", "direction": "push",
      "refs": ["refs/heads/main", "refs/tags/*"], "state": "behind",
      "behind": true, "forgeHead": "…", "lastPushedSha": "…",
      "lastPushedAt": "…", "lastCheckedAt": "…", "lastError": "…" }
  ],
  "grants": [{ "login": "dana", "access": "read", "grantedBy": "…", "grantedAt": "…" }],
  "grantsVisible": true,
  "warnings": ["merge-bot has no write grant on acme/widget-www; its merges answer 403"]
}
```

`checks` carries one row per check name the repository has ever reported, newest
run per name, required contexts first. A required context that has never
reported gets a row of its own with `state: "missing"` — the case that used to be
invisible until a merge sat waiting for a check nobody runs.

`actors` names the reviewer (`JERYU_REVIEW_IDENTITY`, a site setting) and the
merger (`JERYU_MERGE_IDENTITY`, a site setting) with the grant each
needs. An identity that is not an account on this forge is not listed: there is
no actor to warn about. An identity that is listed without its grant gets a
`warning`, repeated at the top level, because its merges answer 403 and nothing
else on the page would say so.

`mirrors` reads the push target from the split manifest
([the GitHub mirror](github-mirror.md)) and the state from the reconcile loop,
falling back to the `jeryu/github-mirror` check trail when the loop has not run
since the last restart. `lastPushedSha` is the sha the target holds.

`grants` is listed only for a caller who may administer the repository, which is
core's own rule for reading them; `grantsVisible` says which case a reader is
looking at, so an empty list never reads as "nobody has access".

## External actors

A gate runner, a reviewer and a host deploy timer are all outside the forge, so
they report themselves through the heartbeat they already use,
`POST /api/v1/runners/heartbeat`. A deploy timer sends the `deploy` label and
names its target:

```json
{
  "runnerId": "buildhost1/publish",
  "host": "buildhost1",
  "slot": 0,
  "labels": ["deploy"],
  "intervalSeconds": 300,
  "last": {
    "repo": "acme/widget-www",
    "sha": "1f0c9a4b2d7e6f5a8c3b1d0e9f8a7b6c5d4e3f21",
    "recipe": "publish main",
    "target": "edge-pages",
    "conclusion": "deployed",
    "seconds": 42,
    "finishedAt": "2026-09-30T12:00:00Z"
  }
}
```

`conclusion` is `deployed`, `failed` or `skipped`; `target` is mandatory with
the label, and refused without it, so a deploy row never shows a target for a
pass that deployed nothing. A deploy timer holds no gate slot and emits no
pipeline events: what it did is a deployment, not a gate. Who may report is the
runner rule (`JERYU_RUNNER_REPORTERS`, a site setting, plus any forge
admin).

The jankurai audit runner reports the same way with the `jankurai-audit` label
([pipeline events](pipeline-events.md#the-jankurai-audit-runner)); a repository
it is auditing, or last audited, lists it as an actor of kind `jankurai-audit`
whose `lastRun.conclusion` is `scored`, `tool-failed`, `refused` or `failed`.

A deployer that writes the forge's own deployment trail
(`POST /repos/{owner}/{repo}/deployments` and its statuses) needs no heartbeat:
each environment's newest deployment is listed as a deployer too.

## Re-gating a head

The gate runner reuses a terminal result whose inputs are identical and holds a
failure until an input changes, so a head that failed for a reason outside its
own sources had no way back to the gate: the attention inbox said "fix or re-run
it" while nothing re-ran anything.

`POST /api/v1/repos/:id/pulls/:number/regate` records the ask against the pull
request's current head (write access to the repository; `409 not_open` for a
draft, a closed or a merged pull request) and answers `202` with it:

```json
{
  "repo": "acme/widgets",
  "number": 7,
  "head_sha": "9c1f7b3d5e4a2c8b6d0f9e8a7b6c5d4e3f2a1b09",
  "requested_at": "2026-10-04T09:00:00Z",
  "requested_by": "dana"
}
```

`GET /api/v1/gate-regate?state=pending` is what the runner reads each tick
(`ops/pr-gate/bin/pr-gate-runner.sh` in jeryu-ci-runner): one request per pull
request, kept only while the pull request is open and its head is still the one
the request names, because a head that moved is gated for being new. The runner
records the `requested_at` it honoured, so one request is one re-gate and asking
again records a newer one. `state=all` answers what was asked regardless.

The store is in memory: a request lives for one runner tick, and a forge that
restarts in between should be asked again rather than replay an older ask. It is
the attention inbox's `action.api` for a `pr_checks_failing` item whose failing
check is one the base requires.

## A pull request with nothing left to land

A pull request whose change has already reached its base branch by another
route has nothing to merge. The inbox reads that one fact from git, by the
tests in `crates/jeryu-api/src/web/pulls/landed.rs`: the head is already in the
base's history, or merging it would leave the base's tree exactly as it is, as
after a squash or a cherry-pick. It raises `pr_already_landed`
(`docs/pipeline-events.md`), with its live-head checks and its cache, and keeps
gate-definition changes visible. The row carries the close call,
`POST /api/v1/repos/:id/pulls/:number/close`. That route comments the commit
the work landed as and then closes the pull request, and refuses anything with
work left to land, so the one click is the whole step and nothing else can be
closed through it.

Which commit to name is a second question, asked only once something is being
closed: a head already in the base's history landed as itself, and one whose
content is there arrived as another commit, found by patch identity
(`git patch-id`, how a cherry-pick is recognised across a rewritten sha). A
squash or a reworded rebase leaves no twin, and then the base's own tip is
named, because that is the state the work is in.

Separately, release automation can close an open `auto/pin-web-*` bump it
opened in this repository, as recorded in its ownership ledger, when every commit is upstream by patch identity
(`git cherry` answers `-`) **and** merging its head produces exactly main's
tree. It comments the landing commit before closing; a refused comment or
close leaves the request open. Only a pull targeting this repository's main
is eligible. After the comment, the timer re-reads its number, open state,
source repository, branch, head and base; an unknown or changed identity is
left open. It also rechecks main's tip against the landing proof.

The current API has no conditional close. These client checks narrow the
window but do not atomically bind the identity to the later PATCH: a source
or main change after the final read can still race it. A retarget in the
fixture models caller data; the current update API has no base-retarget
field. Stronger write fencing needs a separate server contract.
Branch retention follows the existing prune
policy on a later tick.

A head whose patch is upstream but which removes something main has fails
the tree check. A head with no commits of its own is not a cleanup candidate.
This cleanup does not establish a gate result or approval on another head.

## Starting a gate on the push

A gate runner slot used to learn about work only when its own timer fired, so
every gate waited up to a tick after the head existed -- twice for a pull
request queued after its base moved, because the queue commit is a second head.
The forge knows the moment a gateable head appears, so it says so.

`GET /api/v1/gate-wanted?after=<seq>&wait=<secs>` is the signal
(`ops/pr-gate/bin/pr-gate-wake.sh` in jeryu-ci-runner reads it):

```json
{
  "seq": 41,
  "signals": [
    {
      "seq": 41,
      "repo": "acme/widgets",
      "ref_name": "refs/heads/dana/feature-1",
      "sha": "5b8e1d9c7a6f4e3b2d1c0a9f8e7d6c5b4a392817",
      "source": "push",
      "at": "2026-10-08T09:00:00Z"
    }
  ]
}
```

One signal is recorded per branch a push moved (`source: "push"`, naming the
ref), per merge-queue commit the forge built (`"queue"`, naming the pull
request) and per re-gate asked for (`"regate"`). `after` asks for signals above
a sequence, and `wait` holds the request open for up to 60 seconds while there
are none, so a push reaches an idle slot in about as long as the push took.
A reader is handed signals only for repositories it may read, while `seq` is
always the forge's own, so the next poll does not re-walk what this one skipped.

The signal is a hint, never an instruction: it says a head appeared, not that it
is gateable, and the runner still chooses, claims and gates heads exactly as it
does from its timer. That is what keeps the timer a sufficient fallback -- a
signal nobody read costs one tick of latency and nothing else -- and why the
store is in memory and bounded: a forge that restarts hands out a smaller
sequence, which a runner treats as a resync rather than a backlog of work. It
needs no credential of its own; the runner reads it as the identity it already
heartbeats and gates with.

## Starting a review on the push

The security/correctness reviewer (`ops/pr-redteam` in jeryu-ci-runner) learned
about a head the same way the gate used to: from its own five-minute pass, with
a thirty-second helper re-listing every open pull request to guess what was new.
A review does not depend on the gate's result, so that latency was paid twice --
once before the review started, and again before the computed result was
published when the gate went green.

`GET /api/v1/review-wanted?after=<seq>&wait=<secs>` is the reviewer's signal
(`pr-redteam watch` reads it):

```json
{
  "seq": 12,
  "signals": [
    {
      "seq": 12,
      "repo": "acme/widgets",
      "number": 41,
      "head_sha": "5b8e1d9c7a6f4e3b2d1c0a9f8e7d6c5b4a392817",
      "base_ref": "main",
      "draft": false,
      "kind": "pull.synchronized",
      "at": "2026-10-29T09:00:00Z"
    }
  ]
}
```

One signal is recorded per pull request opened (`kind: "pull.opened"`, however
it was opened: the GitHub-compatible edge, an agent session, a workcell export,
a shift or a contribution import) and per open pull request whose head a push
moved (`"pull.synchronized"`). `after` and `wait` work exactly as they do for
`gate-wanted`, and a reader is handed signals only for repositories it may read
while `seq` stays the forge's own.

The signal is a hint, never an instruction: it says a head appeared, not that it
may be reviewed, approved or landed. The reviewer still applies its own skip,
budget and quality-gate rules, and an approval still requires the repository's
required contexts to be green before anything merges. The reviewer's pass
remains the fallback for a signal nobody read, which is why this store is in
memory and bounded like `gate-wanted`'s.
