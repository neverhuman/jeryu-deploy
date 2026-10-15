# Flow: decisions, station history, and verified delivery

The live view is the family's delivery journey, product by product: every
configured repository of a product flows through its own configured
review/gate route into the product's shared convergence point, its release and
production ("Family, product, repository" below). Review and required checks
run in parallel and join before merge. A pin opens a new pull request in the
convergence repository; that PR passes its own review, checks, and merge. Staging produces a release
that intentionally waits for activation authorization. ADR 0013 supersedes
the original linear-map design below.

Click a station for its purpose, trigger, success/failure/hold paths, reporting
services, current work and daily UTC transition bars. Click a bar segment to
filter evidence; inspect an item for its recorded journey and release receipts.
`GET /api/v1/flow/history` accepts `family`, `repo`, `station`, `days=7|28|90`,
`day=YYYY-MM-DD`, `state`, `lot`, and `offset`. It is admin-only. Each page holds
at most 100 transitions. Daily totals use the entire filtered window, not the
page. States are arrived, started, passed, rework, held, released and failed.
Days include today and the preceding days; today is incomplete. Counts are
transitions, not unique items. Changing day/state does not change bar totals.

Migration 0020 keeps transitions and release receipts permanently. Historical
arrival/start/exit times are recovered from visits; detailed hold and runner
coverage starts at `detailed_since`. Missing metadata stays unknown. Review
holds stay at Review; runner errors retry Gate; code failures return to Work.
Shift/Stage have no measured start until a producer records one, so their
wait is no longer called active processing.

On Flow reads, retained staging and production deployment events are reconciled
in batches of at most 32. Each immutable Git ancestry walk is capped at 100,000
commits. Missing objects and revisions outside that bound stay unresolved.
Only known merged PR revisions in the shipped ancestry close pending visits;
not every PR merged before the deployment. The exact deploy lock supplies the
web bundle revision; other lock entries are not assumed shipped. Successful
reads and per-lot receipts make replay idempotent. No runtime database deletion
or destructive rebuild is required. Rollback uses the release's database backup.

`repository_wip` supplements the existing report with recorded open visits by
repository/station. Parallel station counts overlap. The headline no longer
claims a proven bottleneck from incomplete historical estimates.

## Family, product, repository

Flow defaults to the whole family's delivery journey. The response carries a
`topology` object the page draws from, derived from configuration and recorded
evidence only -- never from a repository's name:

```jsonc
"topology": {
  "families": [ { "family": "jeryu", "products": 1, "repos": 8,
                  "work": { "active_lots": 5, "changes": 2, "pull_requests": 2,
                            "pin_pull_requests": 1, "release_artifacts": 0 } } ],
  "products": [
    { "id": "jeryu", "label": "jeryu", "family": "jeryu",
      "source": "family_default",          // or "declared"
      "integration": "jeryu/jeryu-deploy", // where contributions converge
      "release": { "repo": "jeryu/jeryu-deploy", "source": "integration_repo",
                   "stations": ["stage", "deploy"] },
      "repos": [
        { "repo": "jeryu/jeryu-web", "role": "source", "shared": false,
          "pins_into": ["jeryu/jeryu-deploy"], "pin_source": "pin_lock",
          "route": { "kind": "automated", "source": "branch_protection",
                     "approvals": 1, "required_checks": ["jeryu-web/required"] },
          "active_lots": 1, "no_current_work": false }
      ],
      "work": { "active_lots": 2, "changes": 0, "pull_requests": 1,
                "pin_pull_requests": 1, "release_artifacts": 0 } }
  ],
  "unscoped_active_lots": 0,
  "unknown": [ { "scope": "studio", "field": "integration_repo",
                 "reason": "no_pin_lock_or_declaration" } ],
  "families_truncated": false
}
```

Where each part comes from:

- **Contributing repositories** are the hosted repositories of the family, by
  the same rule the repository list's `?family=` filter uses. A configured
  repository with no work in flight is listed with `no_current_work`, so the
  journey is the configured one rather than only what is moving today.
- **Products** are the `[[product]]` tables of the split manifests the server
  was given (`--split-manifest`). **A family that declares none gets exactly
  one product, holding every configured repository**: most families are one
  product, and nothing has to be configured for them. `source` says which case
  a reader is looking at.

  ```toml
  repo_family = "veox-ai-split"       # either spelling of the family key

  [[product]]
  id = "portal"
  label = "Veox portal"
  repos = ["veox-ai/veox-portal", "veox-ai/veox-portal-deploy"]
  integration_repo = "veox-ai/veox-portal-deploy"  # optional; wins over the lock
  release_repo = "veox-ai/veox-portal-deploy"      # optional; the integration repo by default
  pin_lock = "jeryu-split.lock.toml"               # optional; this is the default

  [[repo]]                            # a repository may name its own products
  name = "veox-shared"
  jeryu_slug = "veox-ai/veox-shared"
  product = ["portal", "studio"]      # in two products: a shared dependency
  ```

  A repository listed by more than one product of the family is `shared`. It
  appears in each product it serves, and the family summary counts it, and its
  work, once. A declared repository the forge does not host, and a hosted
  repository no product declares, are both reported in `unknown` instead of
  being filed somewhere plausible.
- **The convergence point** (`integration`) is the product's
  `integration_repo` when it declares one, otherwise the member repository
  whose default branch holds a pin lock naming other members. The real Jeryu
  chain is therefore read out of `jeryu-split.lock.toml` on
  `jeryu-deploy`'s default branch: whichever repositories that lock pins flow
  into it, and each repository's `pins_into`/`pin_source` says so per
  repository. A lock that cannot be read leaves `pin_source: "unknown"` and an
  `unknown` note; it is never read as "pins nowhere".
- **The route** of a repository is its default branch's protection rule:
  `approvals` and `required_checks` as configured. No rule means nothing is
  required, which is reported as `kind: "no_gate"` with
  `source: "no_protection_rule"`, not as unknown. Approvals with no required
  check is `human_only`. Unknown configuration is not a bypass, and a
  pass-through route is shown as the configured fact it is.
- **Work** is counted per distinct lot, once, in one bucket by its furthest
  recorded evidence: `release_artifacts` (a lot at Stage or Deploy), then
  `pin_pull_requests` (a lot at Pin, or one bound to a pull request of the
  convergence repository), then `pull_requests`, then `changes`. A lot at
  parallel stations, or recorded in several repositories, is still one lot in a
  product total and one in the family total. A lot whose visits name no
  repository belongs to no product and is counted in `unscoped_active_lots`.
  Only current work counts: a visit the closure overlay excludes (a closed,
  unmerged pull request, an already merged upstream visit) is not in flight, and
  closed, replaced and abandoned work stays in `closures`.

The page draws one lane per product: the contributing repositories with their
configured routes, the focused repository's own pull request cycle, the shared
convergence point, and the release and production stations of the product's
release route. Clicking a station keeps the family, product and repository
scope in the address, so the station's work, daily bars and item histories stay
scoped to what was clicked. Picking a repository narrows to the products that
deliver it and keeps the repository drilldown; the whole product is one click
away again.

### Extending the dimensions

The topology is deliberately one dimension per configured fact, so a further
dimension is a field, not a new shape:

- A further **item kind** (a mechanical rebase or recheck, a pin bump, a
  logical batch of pull requests) is one more bucket in `work`, assigned by the
  same rule: one lot, one bucket, by its furthest recorded evidence. Adding one
  needs an event the forge already stores to recognise it by; until that
  evidence exists the lot stays in the bucket its recorded visits support.
- A further **per-repository fact** (risk or dispatch class, merge-conflict
  rework, batching policy) is one more field on a `repos[]` entry, read from
  the same configuration sources, with an `unknown` note when the source is
  silent.

No dimension is derived from a repository's name, and none is back-filled over
history the ledger does not hold: a metric with no recorded evidence is absent
with a reason, never estimated.

Configuration reads (the manifests, each repository's pin lock through one
bounded `git show`, and its branch rule) are reused for at most 60 seconds per
family scope, at most eight scopes at a time, so a Flow read does not re-run a
git command per repository every refresh. A change to configuration therefore
appears within that minute.

Migration and compatibility: `topology` is additive. A server that does not
report it leaves the page on recorded visits alone -- the repositories
`repository_wip` names, each one's requirements read from its own branch rule,
and no claimed chain between them. No manifest change is required for a
one-product family; declaring `[[product]]` tables only becomes necessary for a
family that hosts several products, such as veox-ai. Nothing here infers that
work was delivered: a release artifact and a deployment receipt remain the only
delivery evidence.

## Original design and compatibility fields


Where work sits on the delivery line, how long it waits at each step, where it
loops back, and which step is the bottleneck. Decision record:
[ADR 0004](adr/0004-flow-shows-where-work-sits-and-backs-up.md). Status: the
flow ledger and `GET /api/v1/flow` are built (build order step 1); the layout
route and the page are not.

The page is `/flow` with two modes from one model: **Live** (what is where now,
and how it has flowed over a window) and **Layout** (the same stations with the
runners that serve them). It sits beside Activity (the raw event stream),
Runners (one runner at a time), Yield (what the work cost) and the work trace
(one piece of work at a time).

## Vocabulary

| Term | Meaning here |
|---|---|
| **lot** | one piece of work moving down the line: a todo, or a pull request no todo carries. A todo and the pull request that lands it are one lot (the `Todo:` trailer join that `GET /api/v1/trace` already makes) |
| **station** | one step of the route, served by one kind of tool |
| **tool** | what does the work at a station: worker slots, the reviewer, gate-runner slots, the merge pass or merge queue, the release timers, a person |
| **queue time** | how long a lot waits at a station before its tool starts on it |
| **process time** | how long the tool works on it |
| **cycle time** | filed to deployed (or to the furthest station the lot reached) |
| **X-factor** | cycle time divided by the sum of process times: how many times longer the work took than the work itself. 1.0 means no waiting |
| **rework** | a lot sent back to an earlier station: a red gate, a held review, a refused or failed queue replay, a retried or killed attempt |
| **hold** | a lot that cannot move until a person acts (`blocked`, `handoff`, `needs_human`) |
| **WIP** | lots at a station now, split waiting, active and held |

## Stations

The route groups the twelve trace stages into the steps a person reasons about.
The order is fixed; a lot that never passes a station (a repository that is not
pinned anywhere skips Pin) is `not_applicable` there and is left out of its
numbers.

| # | Station | Trace stages | Enters when | Starts when | Leaves when | Tool (runner label) | Rework back to |
|---|---|---|---|---|---|---|---|
| 1 | Queue | `filed` | `todo.filed` | `todo.claimed` | `todo.claimed` | worker slots (shift heartbeats) | — |
| 2 | Work | `claimed`, `done` | `todo.claimed` | `worker.stage` → agent | `todo.attempt_finished` `done` | worker slots | Queue on `retry`, `killed`, `ratelimit`, `retarget` |
| 3 | Shift | `shift` | landed on a shift branch | — | `pr.opened` for the shift | shift PR opener | — |
| 4 | Review | `review` | `pr.opened` / new head | `review.started` | `review.finished` / `pr.review` | reviewer (`redteam`) | Work on `hold` |
| 5 | Gate | `gate` | head needs its required context | `gate.started` | `gate.finished` | gate slots (no label) | Work on `failure` |
| 6 | Merge | `queue`, `merged` | approved and green | `queue.enqueued` or merge call | `pr.merged` / `queue.landed` | merge pass, merge queue | Gate on `queue.building`; Review/Work on `queue.failed`, `queue.refused` |
| 7 | Pin | `pinned` | merged, consumer pin behind | `pin.bump_opened` | the pin reaches the work | `auto-pin` (`automation`) | Pin on `pin.bump_failed` |
| 8 | Stage | `staged` | pinned (or merged when self-releasing) | — | `release.staged` carries the work | `auto-stage` (`automation`) | Stage on `release.stage_failed` |
| 9 | Deploy | `deployed` | staged | `deploy.created` | `deploy.status` `success` carries the work | a person (`deploy`) | Stage on `failure`, `error` |

Shift and Stage have no "Starts when": their tool is a timer that is already
running, so the visit starts when the lot arrives, its queue time is zero and
the whole wait is process time. The alternative — both unknown — would take
every ETA down the line with them.

Known gaps, shown as unknown until closed:

- **Gate queue time.** Nothing records when a head starts waiting for a slot,
  so Gate's queue time is measured from Review's `pr.opened` until a
  `gate.queued` event exists (gate runners emitting it is a separate change).
- **Intake.** There is no ADR, feature or task object before a todo is filed.
  The route starts at Queue; intake stations are added when those objects exist.
- **Documentation.** Product and internal docs land as ordinary pull requests;
  a docs station needs a way to tie them to the work they document. Later.

## Metrics, per station, over a window (7, 28 or 90 days) and now

| Metric | Definition |
|---|---|
| `wip` | `{waiting, active, held}` lots at the station now |
| `oldest_wait_s` | the longest a lot now at the station has been there |
| `queue_s` | `{p50, p90}` of entered → started, over lots that started in the window |
| `process_s` | `{p50, p90}` of started → left |
| `throughput_per_hour` | lots that left forward in the window ÷ hours in the window |
| `rework` | `[{to, count, rate}]`: lots sent back, to which station, as a share of lots that left |
| `first_pass_rate` | share of lots that left forward without ever going round a rework loop at this station |
| `capacity` | `{tools: [runnerId], reporting, busy, expected}` from heartbeats; `expected` is site configuration and null when unset |
| `utilization` | busy-time share of reporting tools over the window, null when the tool does not report `current` |
| `bottleneck` | true on the one station with the largest `queue_s.p50 × throughput`, the station that holds the most lot-hours of waiting |

Line totals: cycle time `{p50, p90}`, X-factor `{p50, p90}`, WIP, lots finished.

A metric with fewer than 3 samples in the window is null and the page says
"not enough data", the same rule the gate estimate uses.

## ETA

Each in-flight lot gets an estimate: the remaining queue and process p50 of the
stations still ahead of it (with the p90 sum as the pessimistic bound), plus,
for its current station, the station's p50 minus the time it has already spent
there (never below zero). A lot on hold has no ETA: its next step is a person.

A station ahead with no estimate (fewer than 3 samples, or never reached) is
never guessed at and never silently counted as zero. It is named in
`eta_unknown_stations` (route order, the current station included) and left
out of the sums; `eta_s` is then the sum over the stations that do have an
estimate and `eta_partial` is `true`: the ETA is a lower bound that says which
stations it does not cover. With no estimate anywhere ahead, `eta_s` is null
and every station ahead is listed. `slowest_ahead` is the slowest of the
stations with an estimate. The in-flight page and the pull request page show
the ETA, mark a partial one with the stations it leaves out, and name the
slowest remaining station. This replaces a separate per-stage estimator.

## The flow ledger

A table in the shift store, projected from events as they are stored (the same
transaction as the event, like the cost ledger), never pruned:

```
flow_ledger(lot_id, family, repo, pr, todo_id, shift, station, entered_ms,
            started_ms, left_ms, outcome, direction, rework_to,
            source_seq_first, source_seq_last)
```

- `lot_id` is the todo id, or `<owner>/<repo>#<pr>` for a pull request no todo carries.
- `direction` is `open` while the visit is in flight, `hold` when a person must
  act before the lot can move, and `forward` or `rework` once it left;
  `rework_to` names the station a rework visit went back to.
- `shift` is what joins a shift pull request to the lots it carries:
  `pr.opened` names no todo, so the projection moves every lot still at Shift
  on that branch **in the pull request's repository** (and family, when the
  event names one). Shift branch names (`dayshift/<date>`) are shared by
  every repository and family, so the branch alone is never the join: a lot
  that recorded no repository is not swept, and a pull request never stamps
  its repository or number onto a visit recorded against another repository.
  Later events on that pull request (review, gate, merge) move every lot it
  carries.
  A pull request lot (`<owner>/<repo>#<pr>`) is rewritten onto
  the todo's lot as soon as one event names both.
- One open row per lot per station visit; rework opens a new visit, so a lot
  that went round the gate twice has two Gate rows and the loop is countable.
- The projection is a pure function of the event sequence. Migration
  `0011_flow_ledger.sql` creates the table and the first start after it replays
  every event the log still holds, recording the replay in `flow_ledger_replay`
  so it happens once; a projection bug is fixed by truncating `flow_ledger`,
  `flow_ledger_events` and `flow_ledger_replay` and restarting, never by
  editing rows.
- The projection counts an event once per `(reporter, event_id)`, for good:
  `flow_ledger_events` records every event id it folded in and is never
  pruned, so an event re-posted after the log's 30-day prune (which makes the
  log accept it again) moves nothing a second time. An event without an
  `event_id` cannot be recognised and is always projected.
- A request reads only the visits of lots still on the line or that left a
  station inside the requested window (`flow_ledger_left` and
  `flow_ledger_lot` indexes), for the requested family; the read is the only
  part done under the event store's lock. A lot that finished before the
  window is never read, so the cost of a request follows the window, not the
  age of the ledger. Whether a station was ever reached (`no_events` versus a
  window with no samples) is one indexed probe per station
  (`flow_ledger_station_family`), not a read of its history. Implementation:
  `crates/jeryu-api/src/web/flow/project.rs` (the fold),
  `flow/report.rs` (the arithmetic), `flow/stations.rs` (the route).

## API

`GET /api/v1/flow` (admin-only, like `/api/v1/yield`). Query: `family`,
`window` (`7d`, `28d`, `90d`; default `28d`) or the same window as `days`
(`7`, `28`, `90`), which is what the page sends. Any other value, the two
spellings disagreeing, or a key the route does not read is
`422 invalid_query`; an unknown family is `422 family_unknown`.

```jsonc
{
  "schema_version": "jeryu.flow/v1",
  "generated_at": "2026-10-09T23:00:00Z",
  "window": { "since": "...", "until": "..." },
  "stations": [
    { "id": "gate", "label": "Gate", "order": 5,
      "wip": { "waiting": 3, "active": 2, "held": 0 }, "oldest_wait_s": 1260,
      "queue_s": { "p50": 322, "p90": 1500 }, "process_s": { "p50": 397, "p90": 640 },
      "throughput_per_hour": 4.2, "first_pass_rate": 0.81,
      "rework": [ { "to": "work", "count": 9, "rate": 0.19 } ],
      "capacity": null,   // and "utilization": null until the layout route lands:
      "utilization": null, // capacity is read from heartbeats, not from the ledger
      "bottleneck": true,
      "unknown": [ { "metric": "capacity", "reason": "no_heartbeat" } ] }
  ],
  "line": { "cycle_s": { "p50": 0, "p90": 0 }, "x_factor": { "p50": 0, "p90": 0 },
            "wip": 0, "finished": 0 },
  "lots": [ { "lot_id": "...", "station": "gate", "state": "waiting",
              "since": "...", "eta_s": { "p50": 0, "p90": 0 },
              "eta_partial": true, "eta_unknown_stations": [ "pin" ],
              "slowest_ahead": "deploy",
              "href": "/work/<id>" } ]   // the oldest 20 per station
}
```

`GET /api/v1/flow/layout`: the stations with their tools, each tool's
heartbeat state (`online`, `offline`, `busy`, last seen), the code it runs
(heartbeat `code`) and the expected capacity per station from site
configuration (`JERYU_FLOW_EXPECTED_<STATION>`, absent by default). A station
with fewer reporting tools than expected, or a tool whose beats show it
failing, is flagged.

Both answer `422` for bad parameters and never invent a number: unknowns are
listed per station in `unknown` as `{metric, reason}`, the reason being
`no_events` (nothing has ever reached the station; `metric` is then `all`),
`no_heartbeat` or `too_few_samples`.

## The page

- **Primary nav** entry **Flow** (`/flow`, `g f`), admin-only like Yield; the mode is `?mode=live|layout`,
  window `?days=`, family from the shell scope. Filters update in place.
- **Live**: the stations left to right as cards on one line. Each card: WIP
  (waiting / active / held), oldest wait, queue p50 against process p50 as two
  bars, throughput, and capacity in use. Rework loops are arcs under the line,
  thicker for more lots; holds are red; the bottleneck card is outlined and
  named in the header ("Gate holds the most waiting"). A line strip above shows
  cycle time, X-factor and WIP. Clicking a station lists its lots (oldest
  first, with ETA); clicking a lot opens its trace.
- **Layout**: the same cards showing tools instead of WIP: each runner id, host,
  online or offline, busy or idle, last seen, code version; reporting versus
  expected capacity; a red card when a station has no reporting tool or fewer
  than expected.
- Inline SVG in the house chart style, HTML labels so text stays legible on a
  phone, where the line becomes a vertical list. Colours follow the tone rules:
  red is a hold or a missing tool, amber is waiting past the station's p90,
  green is flowing.
- Activity links each event to its station, Runners links to Layout, the
  in-flight page and the pull request page show the lot's ETA.

## Build order

1. **Ledger and Live API** (jeryu-deploy) — built: migration, projection on
   insert, replay of the retained log, `GET /api/v1/flow`, ETA per lot. Tests
   drive event sequences through the projection, including rework and holds.
2. **Layout API** (jeryu-deploy): `GET /api/v1/flow/layout` from heartbeats and
   the expected-capacity settings.
3. **Flow page, Live** (jeryu-web), against 1.
4. **Flow page, Layout, and the links** (jeryu-web): Layout mode; ETA on the
   in-flight and pull request pages; station links from Activity and Runners.
5. **Close the gaps**: `gate.queued` from the gate runners; intake and docs
   stations when their objects exist; X-factor and queue-time trends; cost per
   station joined from the cost ledger.

Reconciliation is serialized across cloned event-store handles, at most once
per 30 seconds. Each batch reads at most 32 release events in sequence order;
unavailable Git objects back off for five minutes without blocking later events.
Concurrent reads use the last recorded projection. Work runs in the background and errors are logged; reads continue from the
recorded projection. This is read-triggered upkeep,
so release membership may lag until subsequent reads.

Migration 0020 deliberately repairs the derived ledger instead of truncating it:
the retained pipeline log cannot rebuild older visits. Its two in-place repairs
use authoritative merge evidence and remove the synthetic Shift/Stage starts
created by the previous projection (no producer measured those starts). A merge
closes every lot carried by that pull request. Restore requires the pre-release
shift.sqlite snapshot; the new evidence tables alone are additive. Process-time
fields for Shift and Stage become unknown until producers emit measured starts.

Compatibility: v1 already permits null metrics. Shift/Stage queue and process
times become null because their execution was never measured. `total_s` is an
additive measured-dwell percentile used by legacy ETA consumers, retaining
Shift/Stage elapsed time. Scorecard efficiency explicitly reports missing-start
coverage (docs/scorecard.md), rather than inventing execution or returning zero.

Migration 0021 invalidates cached reads of retained Jeryu deploy events after
correcting full-document TOML lock parsing. Events and existing per-lot receipts
are preserved. Reconciliation replays in bounded batches, restores missing web
membership. A valid lock without a web entry correlates the direct deploy tip.
An unreadable lock or invalid declared web pin defers correlation; a
partial direct-deploy match cannot be cached as complete.


## Closed work and disposition

The Flow response reconciles open ledger visits against current hosted PR and
family todo records. A closed, unmerged PR is excluded from current station WIP,
repository WIP and oldest-wait. A closed, unmerged todo is
excluded only for visits with no PR binding: closing a task cannot erase a live
PR or its remaining delivery steps. Merged PRs continue through Pin/Stage/Deploy.
Reopening restores the recorded visits. Current upstream visits bound to an
already merged PR are excluded too, while Pin/Stage/Deploy remain. The check
uses the exact repository/PR binding, never other PRs sharing a todo.
No closure is a passed gate or delivery.
This read overlay does not alter the durable ledger, history bars or Scorecard.
Historical durations remain observations, including entered-to-started samples
of currently closed visits. Bottleneck selection still uses these observed
queue durations and throughput; closure time is not fabricated.

`closures` retains each closed-unmerged station record, its original item link, title,
current description/note excerpt (2,000 characters), disposition and replacement.
The UI deduplicates item counts, pages station records, filters by disposition
and selected station, and opens retained history. `lifecycle_problems` reports
failed source reads; unreadable records remain in their recorded station state.
An older server omitting these fields has unknown lifecycle coverage.
`merged_upstream_record_count` separately accounts for suppressed open upstream
visits whose exact PR already merged, scoped to the repository filter. These
are not unresolved closures, and their original history remains available.
Family aliases are canonicalized for queue discovery and todo lookup.

Descriptions and todo notes can record an explicit relation on its own line:
`Superseded-by: owner/repo#123` (or `#123` in the same repository). Replacement
status is read from the forge; unavailable targets say unknown. A closed or
unknown replacement still needs attention. This records a claim, not ancestry
or proof of delivery. Similar titles, branch suffixes and vague salvage notes
never silently resolve work. Existing prose remains visible for manual triage.
An intentional abandonment requires both `Flow-disposition: abandoned` and a
nonempty `Flow-reason: ...` line. Otherwise the disposition is unknown and needs
attention. No PRs, todos, branches or commits are automatically closed or deleted.

The response caps closure records at 1,000, separately returning
`closure_record_count`, `closures_truncated` and a visible coverage warning when
more exist. Narrow the repository filter for additional records; source pull requests
and todo history remain available. Current pull states are fetched only for
referenced visits and explicit replacement targets, not all repository pulls.
Filesystem todo reads share a serialized, 30-second cache keyed by family set, retaining at most eight scopes;
no queue scan runs for pull-only inventory. Todo closure reflects the recorded
queue status, not an inferred landing proof. Canonical app routes are used for
source and replacement links; dot-segment references are rejected.
