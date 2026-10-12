# Flow

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
