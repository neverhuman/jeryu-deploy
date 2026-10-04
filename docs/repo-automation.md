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
