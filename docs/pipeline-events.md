# Pipeline events and the attention inbox

The pipeline (queued todo, worker attempt, shift branch, pull request, gate,
review, merge queue, staged release, deployment) runs across several hosts and
tools. This page is the contract that makes it visible in one place:

- **`/api/v1/events`**: an append-only log of what happened, with a durable
  cursor and a WebSocket nudge. It answers "what is the system doing".
- **`/api/v1/attention`**: what is waiting on a person right now, computed from
  current state. It answers "what needs me".
- **`/api/v1/pins`**: what each deploy repo pins and how far behind it is. It
  answers "what could be released".
- **`/api/v1/shift/todos`** additions: whether a done todo is merged and
  released, and which pull request carries it.
- **`/api/v1/trace`**: where one piece of work is, from the todo that filed it
  to the deployment that ships it. It answers "where is this".
- **`/api/v1/yield`**: what the agent factory spent against what reached main.
  It answers "what does a merged change cost".
- **`/api/v1/flow`**: how much work sits at each step of the line, how long it
  waits there, where it loops back and which step is the bottleneck. It
  answers "where does the time go". Contract: `docs/flow.md`.
- **`/api/v1/usage`**: how many tokens and how much money the worker slots are
  spending, live and over time. It answers "what is this costing right now".

JSON is snake_case. Every refusal on these routes is a typed JSON error
(`code`, `message`, `reason`, `repair_hint`, `common_fixes`, `docs_url`),
never HTML. A path under `/api/` (any version) that no route matches answers
`404 api_route_not_found`. A `family` the forge does not host answers
`404 shift_family_not_found` on the shift todo and shift list routes, never an empty list.

Implementation: `crates/jeryu-api/src/web/pipeline.rs` and
`crates/jeryu-api/src/web/pipeline/` (store, emit points, attention rules,
the work trace), `crates/jeryu-api/src/web/shift/truth.rs` and
`shift/visibility.rs`, `crates/jeryu-api/src/web/yields.rs` and `yields/`,
`crates/jeryu-api/src/web/flow.rs` and `flow/` (the station route, the
projection and the Flow report), `db/migrations/0002_pipeline_events.sql`,
`db/migrations/0010_cost_ledger.sql`, `db/migrations/0011_flow_ledger.sql`.
`crates/jeryu-api/src/web/usage.rs`, `db/migrations/0002_pipeline_events.sql`,
`db/migrations/0010_cost_ledger.sql`, `db/migrations/0015_usage_samples.sql`.

## Access

| Route | Who |
|---|---|
| `POST /api/v1/events` | a global admin, or a login in `JERYU_EVENT_REPORTERS` (comma-separated site setting, e.g. `ci-bot,review-bot`). Such a login may post a `detail.usage` sample or a `session.costed` event only when the event's `actor` names it as the operator (`<login>@<host>[/<slot>]`) |
| `POST /api/v1/shift/heartbeat` | a global admin, or a login in `JERYU_SHIFT_REPORTERS` (comma-separated site setting, e.g. `fleet-bot,pool-bot`) |
| `POST /api/v1/agent-presence` | a global admin or a login in `JERYU_PRESENCE_REPORTERS` for any scope; any other login for repositories it can write (`docs/agent-presence.md`) |
| `GET /api/v1/events` | global admins |
| `GET /api/v1/events/histogram` | global admins |
| `GET /api/v1/attention` | global admins |
| `GET`, `POST /api/v1/attention/acks` | global admins |
| `GET /api/v1/pins` | global admins |
| `GET /api/v1/trace` | global admins |
| `GET /api/v1/yield` | global admins |
| `GET /api/v1/flow` | global admins |
| `GET /api/v1/flow/layout` | global admins |
| `GET /api/v1/incidents` | global admins |
| `POST /api/v1/incidents/webhook` | a global admin, or a login in `JERYU_EVENT_REPORTERS`, like `POST /api/v1/events` |
| `GET /api/v1/usage` | global admins |
| WebSocket scope `pipeline` | global admins |

Reads are admin-only in v1 because events and attention items carry todo
titles, notes and log tails from repositories the reader may not be able to
see, and pins list the unreleased commits of every dependency. Filtering per
repository is a later step.

## Events

Stored in `<data_dir>/shift.sqlite`, table `pipeline_events`, for 30 days
(pruned at most hourly, on write). `seq` is never reused, so a cursor stays
valid across pruning and restarts.

### Event object

| Field | Type | Notes |
|---|---|---|
| `seq` | integer | server-assigned, strictly increasing, durable |
| `ts` | RFC 3339 UTC | server receive time |
| `event_id` | string or null | the producer's own name for the event; see "Retrying" |
| `source` | string | `forge`, `todoq`, `pr-gate`, `pr-redteam`, `auto-stage`, `deploy`; open set, `^[a-z][a-z0-9-]{0,31}$` |
| `kind` | string | dotted lower-case words, `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$`, at most 64 characters; the vocabulary below is open |
| `reporter` | string | the login that posted it, `forge` for server-emitted; always set by the server |
| `actor` | string or null | who did the thing, e.g. `dev@node-a/w1`, `review-bot`, `build-1/slot0`; clipped to 128 characters |
| `family` | string or null | shift family; server-derived from `repo` when the producer omits it (see below) |
| `repo` | string or null | `owner/name` |
| `pr` | integer or null | positive |
| `sha` | string or null | 7 to 64 hex characters, stored lower-case |
| `todo_id` | string or null | |
| `shift` | string or null | shift branch |
| `outcome` | string or null | a lower-case word of at most 32 characters: `success`, `failure`, `blocked`, `retry`, `error`, `timed_out`, ... |
| `needs_human` | bool | default false; a highlight in the feed, not the inbox's source of truth |
| `summary` | string | one human line, required; clipped to 300 characters |
| `reason` | string or null | why, especially when `needs_human`; clipped to 1000 characters |
| `cost_usd` | number or null | not negative |
| `seconds` | integer or null | duration, not negative |
| `log_tail` | string or null | the server keeps the last 16 KiB |
| `log_url` | string or null | at most 512 characters |
| `detail` | object or null | free JSON, at most 8 KiB serialized |

Malformed identity fields (`event_id`, `source`, `kind`, `repo`, `sha`, `pr`,
`outcome`, `cost_usd`, `seconds`, `log_url`, `detail`) are refused with `422`.
Human text (`summary`, `reason`, `actor`, `log_tail`) is clipped instead,
because producers send it best-effort and a long line is no reason to lose the
event. Blank strings become `null`. Unknown fields are ignored, so a producer
cannot choose its own `seq`, `ts` or `reporter`.

### Family is server-derived from the repo

A producer that names a `repo` need not name a `family`. When `family` is
absent and `repo` is set, the server resolves the repo's family before storing
the event and stores that key, so the row is already tagged when it is read.
The registry it reads is the one behind the repository family pills and
`GET /api/v1/shift/families`: the split catalog's classification of
`owner/name` first, the repository's persisted family second, canonicalised
(`<x>-split` and `<x>` are one key, see `docs/family-key.md`). A producer that
does name a family keeps it, and a repo the forge hosts in no family -- or an
event that names no repo at all -- stores `family` null.

This matters because `?family=` on the reads below is a column filter: before
the server derived it, every `gate.*`, `review.*`, `deploy.*`, `release.*` and
`pin.*` event stored a null family, so filtering the Activity page by family
hid work that was plainly there. Rows stored before this behaviour were
backfilled by `db/migrations/0016_pipeline_event_families.sql`.

### Kinds

Emitted by the forge (`source = "forge"`, `reporter = "forge"`):

| Kind | When |
|---|---|
| `todo.filed`, `todo.action` | a todo is filed through `POST /api/v1/shift/todos` (the web filing form), or released/blocked/finished/closed/parked/edited/re-prioritised from the web. A todo pushed straight into the queue repository with git never reaches a forge route and emits nothing: `todoq`'s `todo.claimed` is the first event on it |
| `worker.stage` | a worker slot's state, stage or todo differs from its previous heartbeat; summary like `w1 jeryu: agent -> gate on <todo>`. An unchanged beat, or an idle slot appearing, emits nothing |
| `shift.pr_opened` | the Shift page opened a shift's review PR |
| `pr.opened`, `pr.review`, `pr.approved`, `pr.merged` | pull request steps on the v1 routes and the GitHub-compatible edge; `pr.review` has `outcome` `approve`, `request_changes` or `comment`, and `needs_human` for `request_changes`. Tagged with `family` and `shift` when the head is a shift branch. `pr.approved` is the one-click route (`POST /api/v1/repos/{id}/pulls/{number}/approve`) only: an approval sent as a review -- the v1 reviews route, the `gh`-compatible `POST /api/v3/repos/{owner}/{repo}/pulls/{number}/reviews`, pr-redteam -- is `pr.review` with `outcome = approve`, so a client that reviews rather than one-click-approves produces no `pr.approved` at all |
| `review.finding`, `review.resolved`, `defect.linked` | review catch rate. `review.finding` is one finding a human left: a line comment (`outcome = comment`) or a request-changes verdict with no line comment (`outcome = blocking`); `detail` has `reviewer_kind = human`, `severity`, `path`, `line`. `review.resolved` is written when a pull request merges, for each finding no reviewer resolved: `outcome = caught` when a commit after the finding's `sha` touched its `detail.path` (or, for a finding with no path, moved the head), else `not_acted`; `detail.finding` names the finding (its `event_id`, or `seq-<seq>`), `detail.reason` is `file_changed`, `head_changed` or `unchanged_at_merge`. `defect.linked` is written for every change the forge can see a defect in: the change a revert pull request names (`#N` in its title or body, `detail.cause = revert`), the changes a todo that redoes another one landed (`cause = rework`, `detail.by` the redoing todo), and the changes an `incident.caused_by` blames (`cause = incident`, `detail.by` the incident). A change a cause names by lot key rather than number (a todo id) is resolved to its pull requests through the flow ledger, and the same defect reported twice links the change once |
| `pr.ready_for_review`, `pr.draft` | the draft lifecycle: `POST /api/v1/repos/{id}/pulls/{number}/ready` or `/draft`, or the `gh`-compatible `PATCH /api/v3/repos/{owner}/{repo}/pulls/{number}` with `{"draft": …}`. The summary names the actor ("acme/app#7 marked ready by dana"); `outcome` is `ready` or `draft`. Only the author and admins may make either transition, and each one also appends a `pull_request.ready_for_review` / `pull_request.convert_to_draft` audit row |
| `pr.skipped` | an automation looked at a pull request and did nothing, once per head: `detail.automation` is the tool (`merge-queue`, `pr-redteam`) and `detail.skipped` the reason (`draft`). Without it a draft sits with no signal at all |
| `queue.enqueued`, `queue.building`, `queue.landed`, `queue.failed`, `queue.dequeued`, `queue.refused` | merge-queue transitions, and only those: a pull request merged without joining the queue has none of them, just `pr.merged` with `detail.via = "merge"`. `queue.building` is a rebuild (moved base, or a retry after a red gate). `queue.landed` is followed by `pr.merged` with `detail.via = "merge_queue"`. `queue.refused` means the PR could not be replayed onto the base and somebody has to rebase it; it is also persisted as a `dequeued` entry whose `refusal_code` is the forge code, which is what raises `queue_refused` in the attention inbox |
| `main.status` | the required gate ran on the head of a repository's default branch (a runner heartbeat whose `current` or `last` names that head's sha). `outcome` is `started`, `green` or `red`; `detail` has `runner`, `recipe`, `conclusion`, `seconds` and, on a red run, `tests` when the runner reported failing test names. A rerun of the same sha is another event, which is how a flake shows. `GET /api/v1/main-health` folds them into red intervals and a flake rate. While the newest verdict of a repository is `red`, its `RepositorySummary.health` is `failing`, so the repository page and the Repositories listing show a red main even though no check run on the head does |
| `main.escaped` | a head turned the default branch red where it had not been: `pr` is the merged pull request whose merge commit is the red head, the escaped-to-main defect charged to that change. A rerun of the same sha that goes green marks the interval a flake in the projection and takes it out of `escaped_defects` |
| `gate.started`, `gate.finished`, `review.started`, `review.finished` | a runner heartbeat's `current` or `last` differs from that runner's previous beat (`review.*` for the `redteam` label; never for the `automation` label, see [Runner heartbeats](#runner-heartbeats)). A reviewer verdict of `hold`, `failed`, `publication_rejected` or `too_large` sets `needs_human`. `*.started` comes from `current` alone: a runner that beats only once a pass has finished, carrying `last` and no `current`, produces `*.finished` with no `*.started` before it |
| `deploy.created`, `deploy.status` | a write to the Deployments API; `outcome` is the deployment status state, `needs_human` for `failure` and `error`; `reason` is the status description, which `deploy-release.sh` ends with the last meaningful line of switch.sh's output on failure. The status request may also carry `log_path` (the operator's kept switch log on the release host) and `log_tail`: the Deployments API stores neither, and the event's `detail` carries both (the tail cut to its last 20 lines and at most 6000 bytes of JSON, so the event fits the 8 KiB `detail` limit) |
| `repo.archived`, `repo.unarchived` | a repository became read-only, or writable again. Archiving deletes nothing and is reversible, so both are a plain success and the kind is what says which way it went; `detail` carries `archived` |
| `repo.renamed`, `repo.transferred` | the repository moved: a rename inside its owner, or a transfer to another one. The old slug keeps resolving until a repository is created there, so neither is a warning; `detail` carries `{from, to}` |


### Review catch rate

`GET /api/v1/yield` carries `review_catch`: `human`, `agent` and `total`
rows, each with `findings`, `caught`, `not_acted`, `open`, `escaped`,
`catch_rate`, `reviews_timed`, `review_seconds_mean` and
`review_seconds_median`.

Producers post three kinds the forge cannot see for itself:

- `review.finding` from the agent reviewer (jeryu-ci-runner's `pr-redteam`):
  `repo`, `pr`, `sha`, an `event_id`, and `detail` with `reviewer_kind =
  agent`, `severity`, `title`, `path` and `line`.
- `review.resolved` with `outcome = caught` and `detail.finding` set to the
  finding's `event_id`, when the reviewer sees a later head no longer shows
  it. The forge's merge-time resolution leaves a finding that already has one.
- `defect.linked` (`repo`, `pr` of the defective change, `detail.cause` of
  `revert`, `rework` or `incident`) for a defect no forge route sees. The
  forge links all three causes it can see itself: a revert pull request, a
  todo edited to rework another one, and an `incident.caused_by`.

`catch_rate = caught / (caught + escaped)`. A change *escaped* a reviewer kind
when that kind passed it (`pr.review` from a person, or `pr.reviewed`, with
`outcome = approve`) and a `defect.linked` names it; each change counts once.
Review time is from the earliest `pr.review_requested` or
`pr.ready_for_review` of a pull request to the first review event of each
kind that follows (`pr.review` by a person: `human`; `pr.reviewed` or
`review.finished`: `agent`). Findings and times count inside the report's
window.

### Flow ledger

Every stored event is also projected into `flow_ledger` in the same
transaction: one row per lot per station visit, which is what
`GET /api/v1/flow` reports from. Like the cost ledger it is never pruned, and
like it a repeated `event_id` writes nothing: `flow_ledger_events` keeps every
`(reporter, event_id)` the projection folded in, so an event re-posted after
the log pruned it is stored in the log again but moves no lot a second time.
A shift pull request moves only the lots of its own repository (and family)
on that shift branch. Each in-flight lot's ETA sums the stations ahead that
have an estimate; the ones that do not are listed in `eta_unknown_stations`
and `eta_partial` is `true`. The stations, the event kinds that move a lot
between them and the replay that rebuilds the table are `docs/flow.md`;
migration `0011_flow_ledger.sql`.

### Cost ledger

Every stored event with a `cost_usd` is also written to `cost_ledger` in the
same transaction, with the event's `reporter`, `event_id`, `ts_ms`, `source`,
`kind`, `family`, `repo`, `pr`, `sha`, `todo_id`, `shift`, `outcome`,
`cost_usd`, `seconds` and `detail`. The ledger is never pruned: spend is a
record, the event log is telemetry. A repeated `event_id` writes neither table,
and an event re-posted after the log pruned it is stored in the log again but
counted once in the ledger (the ledger is unique on `reporter`, `event_id`).
Migration `0010_cost_ledger.sql` copied every costed event the log held when it
ran.

There is no `status.posted`: commit statuses are too noisy, and `gate.*` covers
them. Server-side emission is best-effort: a failed insert is written to the
server log and never reaches the response of the request it rode on.

Posted by producers:

| Source | Kinds |
|---|---|
| `todoq` | `todo.claimed`, `todo.attempt_finished` (`outcome` `done`, `retry`, `blocked`, `ratelimit`, `retarget` or `killed`; `cost_usd`, `seconds`, `log_tail`; `detail` has `usage` (per model; its totals are also the slot's last usage sample, see [Token usage](#token-usage)), `model`, `agent`, `experiment`, `session`, `factory`), `todo.merged`, `todo.waiting_on_unmerged`, `shift.exhausted` (`shift` is the shift branch, `needs_human` when claimable todos are left waiting; `detail` has `spent_usd`, `budget_usd` and `waiting`), `worker.error` |
| `auto-stage` | `release.staged` (`sha` is the staged commit; `detail` has `release`, `previous_release`, `deploy_command`), `release.stage_failed` (`log_tail`; `needs_human` on the final attempt) |
| `auto-pin` | `pin.bump_opened` (`repo` is the consumer, `pr` the bump, `sha` the dependency commit; `detail` has `dependency`, `from`, `to`, `web_dist_sha256`), `pin.bump_failed` (`log_tail`; `needs_human` on the second failure for the same commit), `pin.bump_superseded` (a stale, unqueued bump closed for a newer head; `pr` is the new bump, `detail.superseded_pr` the closed one), `pin.bump_diverged` (`needs_human`; an open bump pins a commit that is not an ancestor of the dependency head, so it is left open) |
| the pull request reviewer | `pr.reviewed` (`repo`, `pr`, `sha`, `outcome` is the review decision, `cost_usd`, `seconds`; `detail` has `usage`, `model`, `decision`). The yield report reads the kind, not the source name |
| `pr-gate` | `gate.log` (`outcome` including `timed_out` and `inputs_changed`, `seconds`, `log_tail`, a receipt subset in `detail`) |
| an agent session reporter | `session.costed`: one increase of a hand-run agent session's spend (not a queue worker, not a reviewer) on one repository. `cost_usd` is the delta since the session's previous event for that repository, never the running total; `event_id` is stable per delta, so a re-post is dropped. `repo` is `owner/name`, left out when the spend touched no forge repository; `family` only then, when the session declared one family. `actor` is `<login>@<host>`. `seconds` is the optional delta of active time. `detail` has `session` (`<agent>:<id>`), `agent`, `model` (the delta's dominant model), `seq`, `cumulative_usd` (display only) and `tokens` (per model `input`, `output`, `cache_read`, `cache_write`, `cost_usd`; not `usage`, which is a todo attempt's). The yield report sums the kind as `adhoc` |

### `POST /api/v1/events`

The body is one event without `seq`, `ts` and `reporter`, or
`{"events": [...]}` with at most 50. The whole batch is validated before any
event is stored, and a refusal names the entry (`events[1]: kind: ...`).

```json
{"schema_version": "jeryu.pipeline_events/v1", "ok": true, "seqs": [41, 42], "duplicates": 0}
```

`201` when anything new was stored, `200` when every event was a repeat.

### `GET /api/v1/events`

Query: `after_seq` (`since` is the same cursor under another name; sending
both with different values is `422 events_invalid_query`), `before_seq`,
`limit` (alias `per_page`; 1 to 500, default 100, and out of range is a
`422 events_invalid_query`, never clamped — see `docs/pagination.md`), `family`,
`repo`, `pr`, `todo_id`, `source`, `kind`, `needs_human`. `kind` matches exactly,
or as a prefix when it ends with a dot (`kind=todo.`). Anything else (`kind=todo`,
`kind=Not A Kind`) answers `422 events_invalid_query`: it could only ever match nothing, and an
empty page would read as "no such events". A key this route does not read
(`?familyy=acme`) answers `422 events_invalid_query` too, naming the keys it
does: a misspelled filter used to be ignored, and the whole log read as the
filtered page. See `docs/errors.md`, "Invalid Query".

- Without `after_seq`: the newest events, **newest first** (a page to show).
- With `after_seq`: events with `seq > after_seq`, **oldest first** (a cursor
  tail to follow).

```json
{"schema_version": "jeryu.pipeline_events/v1", "events": [...], "latest_seq": 42,
 "limit": 100, "has_more": true, "next_cursor": 42}
```

`has_more` is read one row past the page, so a page exactly `limit` long is not
mistaken for "more behind it". `next_cursor` is the `seq` of the last event on
the page (absent for an empty one): send it as `after_seq` to keep walking
forward, or as `before_seq` on the default newest-first read. One paging rule
for both edges is written once in `docs/pagination.md`.

### `GET /api/v1/events/histogram`

How much pipeline there was, bucketed over time: what the chart at the top of
the Activity page draws and what its wall-mode tiles count. `GET
/api/v1/events` answers at most 500 rows, so a chart drawn from it counts the
page it happens to hold; this route counts the window itself, as one
`GROUP BY` over `pipeline_events`.

Query: `hours` (1 to 720, default 24; out of range is a
`422 events_invalid_query`, never clamped), `bucket` (`5m`, `15m`, `1h`,
`1d`), and the filters of `GET /api/v1/events` that narrow a window rather
than page it: `family`, `repo`, `source`, `kind` (exact, or a prefix ending in
a dot) and `needs_human`. A key this route does not read (`after_seq`,
`limit`) answers `422 events_invalid_query` naming the keys it does.

Without `bucket` the window picks one: `5m` up to 2 hours, `15m` up to 12,
`1h` up to 168, `1d` beyond. One answer holds at most 1500 buckets; a finer
bucket over a longer window is refused with the bucket to use instead
(`hours=720&bucket=5m` is 8640 bars), never silently coarsened.

Bucket starts are whole multiples of the bucket since the epoch, so two calls
seconds apart draw the same bars and a `1d` bucket is a UTC day. `from` is the
first bucket's start, inclusive; `to` is the last bucket's end, exclusive, and
the bucket holding "now" is included whole, so `to` is at or just after the
time of the request. Every bucket in between is present: a quiet hour is a row
of zeros, never a gap in the series.

```json
{"schema_version": "jeryu.pipeline_events/v1",
 "from": "2026-10-05T09:00:00Z", "to": "2026-10-06T10:00:00Z", "bucket_seconds": 3600,
 "buckets": [{"start": "2026-10-05T09:00:00Z", "total": 12,
              "by_group": {"todos": 4, "gates": 3, "reviews": 2, "pulls": 1,
                           "merges": 1, "releases": 0, "deploys": 1, "other": 0},
              "failures": 1, "needs_human": 1, "cost_usd": 0.42, "seconds_p50": 114}],
 "totals": {"todos_finished": 9, "blocked": 1, "prs_merged": 4, "deploys": 2, "spent_usd": 3.71}}
```

- `by_group` counts the same kind prefixes as the Activity chips: `todo.`,
  `gate.`, `review.` plus `pr.review`, `pr.`, `queue.`, `release.`, `deploy.`,
  and `other` for everything no chip names (`worker.`, `pin.`, `shift.`). A
  `pr.review` is a review verdict, not a pull-request beat, so it is counted
  as a review; each event is counted in exactly one group, so the groups sum
  to `total`.
- `failures` counts the events whose `outcome` says the step did not get
  through: `failure`, `failed`, `error`, `blocked`, `timed_out`,
  `request_changes`, `inputs_changed`.
- `cost_usd` and `spent_usd` are summed to the cent. `seconds_p50` is the
  median `seconds` of the events in the bucket that carry one (the upper of
  the two middles for an even count), `null` when none does.
- `totals` covers the whole window, not one bucket: the five wall-mode tiles.
  `todos_finished` and `blocked` are `todo.attempt_finished` with `outcome`
  `done` and `blocked`, `prs_merged` is `pr.merged`, and `deploys` is
  `deploy.status` with `outcome` `success`.

**One happening, counted once.** A finished gate is reported twice —
`gate.finished` from the forge's reading of the runner heartbeat, then
`gate.log` from the runner's own reporter — and so is a review
(`review.finished`, then the `pr.review` verdict). The histogram drops the
echo, keeping the report that carries more, exactly as the feed does
(`foldEchoes` in `jeryu-web`'s `activityModel.ts`): a `gate.finished` is
dropped when a `gate.log` for the same `repo` and `sha` with the same outcome
(or any non-plain outcome the heartbeat flattened to `error`) lies within five
minutes, and a `review.finished` when a `pr.review` for the same `repo`, `pr`
and `sha` does. A lone report of either kind is kept. So `total` is happenings,
not rows, and it can be lower than the number of rows `GET /api/v1/events`
returns for the same window.

### WebSocket

`GET /api/v1/ws` (implementation: `crates/jeryu-api/src/web/ws.rs`) is one
socket for every live scope; the event log is scope `pipeline`. It needs the
same login as the API (session cookie or `Authorization: Bearer`). A request
without `Upgrade: websocket` answers `426 websocket_upgrade_required` as a
typed JSON error, and a non-integer cursor `422 ws_invalid_query`.

Frames are JSON text, protocol `jeryu.ws.v1`, tagged by `type`:

| Direction | `type` | Body |
|---|---|---|
| server | `hello` | `server_time`, `current_seq` (hub counter), `protocol`; sent on connect and in answer to a client `hello` |
| client | `hello` or `subscribe` | `subscriptions: [{"scope": "pipeline", "filters": {"after_seq": 41}}]` |
| client | `unsubscribe` | `scopes: ["pipeline"]` |
| client / server | `ping` / `pong` | `nonce`, echoed with `server_time` |
| server | `event` | `event`: a `WebEvent` (`seq`, `timestamp`, `scope`, `kind`, `entity`, `summary`, `payload`) |
| server | `error` | `code`, `message`: `subscription_denied` (the scope is not yours; `pipeline` is admin-only), `unknown_message`, `events_store_failed` |

Every stored event is published on scope `pipeline` as a `WebEvent` whose
`payload` is the event object and whose `kind`/`summary` are the event's. The
frame's own `seq` is the hub's in-memory counter, reset on restart; the durable
cursor is `payload.seq`.

**Resuming.** Give the socket the same cursor as the HTTP route: connect to
`/api/v1/ws?after_seq=<last payload.seq you saw>` (or `since=`), or put
`"filters": {"after_seq": <seq>}` on the `pipeline` subscription (it wins over
the URL). When `pipeline` is subscribed, the server first sends every stored
event with `seq > after_seq`, oldest first, then live events. The URL cursor is
used for the first `pipeline` subscription of the connection only. A replay
holds at most 500 events: if you get 500, page the rest with
`GET /api/v1/events?after_seq=`. An event stored while the replay is being read
can arrive twice, so drop frames whose `payload.seq` you have already seen.
Without a cursor there is no replay: only events stored after the subscription.

**Runner changes.** A gate slot's or the reviewer's heartbeat that starts or
finishes a pass is also pushed as `runner.changed` frames (timers, deployers and
the audit runner push none). Scope `runners` is admin-only, like
`GET /api/v1/control-plane/runners`, and its payload is the whole beat:
`runnerId`, `kind`, `current` (`repo`, `pr`, `sha`, `recipe`, `startedAt`) and
`last` (`repo`, `pr`, `sha`, `recipe`, `conclusion`, `seconds`). Each repository
the beat touches also gets one frame on `repo.<owner>.<name>` (anyone who can
read that repository) whose payload names only that repository and no runner:
`{"repo", "pass": "gate"|"review", "phase": "started"|"finished"|"stopped", "pr", "sha", "conclusion"}`
(`pr`/`sha` with `started` and `finished`, `conclusion` with `finished`).
These frames are not stored and have no cursor: treat one as a nudge to
refetch. A beat that changes nothing pushes nothing.

```js
const ws = new WebSocket(`wss://${forgeHost}/api/v1/ws?after_seq=${last}`);
ws.onopen = () => ws.send(JSON.stringify({type: "subscribe", subscriptions: [{scope: "pipeline"}]}));
ws.onmessage = ({data}) => {
  const frame = JSON.parse(data);
  if (frame.type === "event" && frame.event.scope === "pipeline" && frame.event.payload.seq > last) {
    last = frame.event.payload.seq; // handle frame.event.payload
  }
};
```

## Attention

`GET /api/v1/attention` is computed from current state on each call (cached
for 10 seconds), never from old events, so an item disappears when its cause
is fixed.

```json
{"schema_version": "jeryu.attention/v1.4", "generated_at": "...",
 "items": [...], "counts": {"critical": 0, "action": 3, "watch": 1},
 "total": 4, "page": {"limit": 100, "page": 1, "total": 4, "has_more": false}}
```

Items are sorted by severity, then oldest first. An item somebody
acknowledged (see [Acknowledgements](#acknowledgements)) is left out, and out
of `counts`, until the date it was acknowledged until.

Query: `family` (either spelling of a family key), `severity` and `kind`, plus
the paging keys `limit` (alias `per_page`) and `page` (`docs/pagination.md`).
Each filter comes from a closed set, so one that could never match answers
`422 invalid_query` naming what it accepts rather than an empty inbox (see
`docs/errors.md`, "Invalid Query"); `counts` is recounted for the rows a filter
kept, and an item of no family is not any family's. `counts` and `total` stay
over every item the filter kept, not over the page in hand, so a reader of page
2 still sees how much is waiting.

| Field | Notes |
|---|---|
| `id` | stable, e.g. `todo-blocked:jeryu:<id>` |
| `kind` | see below |
| `severity` | `critical` (the pipeline is stuck or broken), `action` (a human decision or click is the next step), `watch` (unusual, may heal by itself) |
| `title` | one line |
| `reason` | why it needs a person, written for a reader with no context. The specific fact leads (a blocked todo's note verbatim, the failing check's name, the queue's own reason), the consequence follows; at most 1000 characters |
| `since` | RFC 3339 or null |
| `family`, `repo`, `pr`, `todo_id`, `sha`, `shift` | nullable join keys. `repo` is `owner/name`, except on `shift_without_pr`, which uses the family repo name as the Shift API does |
| `href` | the in-app page where the step happens |
| `action` | `{"label", "command", "run_in", "api"}`. `command` is a copyable shell line when the step happens off-site, else null. `run_in` says where that line is run, as a short phrase naming the machine and the directory (`"node-a, any directory"`, `"node-a, in a jeryu/jeryu-deploy checkout"`); it is a string on every item whose `command` is set and the key is absent on every other item. `api` is the call that performs the step, and the key is absent when no route of this API does it (see [Replaying a step](#replaying-a-step)) |
| `budget` | `{"spent_usd", "budget_usd", "waiting"}` on `shift_budget_spent` only, from the `shift.exhausted` event's `detail`: the spend, the operator's cap and how many claimable todos were left waiting. A number the event did not carry is absent, and so is the whole key on every other kind |
| `next_step` | the one next step as a sentence: `<label>: on <run_in>, run \`<command>\`` or `<label>: open <href>` |

Every item names exactly one next step: run `action.command` on the machine and
in the directory `action.run_in` names when it is set, otherwise open `href`
and do what `action.label` says. A command item's action:

```json
{"label": "Run auto-pin now",
 "command": "systemctl --user start jeryu-auto-pin.service",
 "run_in": "node-a, any directory"}
```

### Replaying a step

`action.api` is the step as one call on this API: `{"method", "path", "body"}`,
with `body` absent when the route takes none. The path is absolute and ready
to send as it stands, so a caller performs the step without parsing
`next_step` or guessing which endpoint it meant; `href` stays where to go to
understand the item. Performing the call clears the item on the next collect,
which is the test the rules are held to.

```json
{"label": "Open the shift's review PR",
 "command": null,
 "api": {"method": "POST", "path": "/api/v1/shift/shifts/acme/pr",
         "body": {"branch": "nightshift/2026-09-19"}}}
```

`api` is set only where one route performs the whole step. A step that starts
somewhere else carries none, and the item still says in prose what to do:
pushing a fix, adding a repo to the family config, cherry-picking onto a base
that wants linear history, deciding a title.

| Kind with an `api` | Call |
|---|---|
| `shift_without_pr` (no replacement PR open) | `POST /api/v1/shift/shifts/<family>/pr` with `{"branch": "<shift branch>"}` |
| `shift_stranded_work` | the same: the merged pull request is behind it, so asking again opens a fresh one |
| `todo_blocked` (`owner_task`) | `POST /api/v1/shift/todos/<family>/<id>/action` with `{"action": "done"}` |
| `todo_blocked` (`handoff`, `agent_blocked`), `todo_handoff`, `todo_parked` (`action`) | the same route with `{"action": "release"}` |
| `pr_draft_waiting` | `POST /api/v1/repos/<owner>/<name>/pulls/<n>/ready` |
| `pr_ready_to_merge` | `POST /api/v1/repos/<owner>/<name>/pulls/<n>/queue` |
| `pr_superseded` | `PATCH /api/v3/repos/<owner>/<name>/pulls/<n>` with `{"state":"closed"}`; the row names the replacement to cite when closing; both PRs must share the author, base and source repository |
| `queue_failed` | the same: the entry is retried by queueing it again once the reason is fixed |
| `queue_refused` (a code with no step of its own) | the same |

The schema version says `v1.4` because `action.api` (`v1.1`), `budget`
(`v1.2`), `pulls` (`v1.3`) and `assignee` (`v1.4`) are additive: every field
of `v1` is unchanged, so a client written against `v1` reads a `v1.4` answer
as it always did. `assignee` is set on a todo item when a person has taken the
todo on (see "Taking a todo on" below).
`pulls` is set on `pr_branch_campaign` only, where one row stands for several
pull requests, and each entry is `{"repo": "owner/name", "number": 7, "href":
"..."}`.

The machine in `run_in` is one of three roles, each named by an environment
variable of the server (see [Operations](#operations)): the release host
(auto-pin, auto-stage, `deploy-release.sh`, the todoq workers), the gate host
(the `pr-gate-runner@` slots) and the forge host (the forge itself; mirror
pushes leave from there).

| Kind with a command | `run_in` |
|---|---|
| `gate_runner_down` | `<gate host>, any directory` |
| `workers_down` | `<host>, any directory`, where the host is the one the family's newest worker or supervisor heartbeat named, else the release host |
| `mirror_failing` | `<forge host>, as the user the forge runs as` (the command reads that user's global git config) |
| `release_staged` | `<release host>, in a <owner/name> checkout`, the repository the `release.staged` event names |
| `pin_behind` (`action`) | `<release host>, any directory` |

| Kind | Severity | Meaning |
|---|---|---|
| `todo_blocked` | action | status `blocked`; the reason opens with the note, and the step is the one the todo's `block_kind` names (see [Block kinds](#block-kinds)) |
| `todo_parked` | watch or action | status `parked`. `watch` while `park_until` is in the future, and the label says there is nothing to do until then; `action` once it has passed, or when the park carries no date, because then only a person moves it |
| `todo_handoff` | action | status `handoff` |
| `todo_untriaged` | action | open and `triaged = false`; workers skip it |
| `todo_stuck_claim` | watch | claimed, lease dead for 10 minutes or more |
| `todo_waiting_on_blocker` | watch | open, and a `blocked_by` todo is blocked, handed off, or done but not merged |
| `shift_without_pr` | action | a shift branch holds todos that are on no base commit (`unmerged_todos`), with no pull request or only a closed one. Being ahead by sha is not enough: a branch closed and replaced by a rebased one stays ahead for ever |
| `shift_budget_spent` | action | the newest `shift.exhausted` of a family (which is also the newest of its shift: a new shift's event replaces the one before it) said the operator's shift budget ran out with claimable todos still waiting, and no `todo.claimed` has been posted for the family since. Nothing else says so: the cap is the operator's, not state the forge holds, so this one rule reads the event log. An event whose `detail.waiting` is 0 is a shift that ended tidily and asks for nobody |
| `shift_stranded_work` | action | a shift's pull request already merged, and todos landed on the branch afterwards: their `Todo:` trailer is on no base commit, so the work is finished and on no open pull request |
| `mirror_diverged` | critical | the newest reconcile found GitHub holding commits the forge does not, or a tag GitHub published at another commit; ONE item PER repository, naming the commits, because each one is its own history question. Nothing was forced and nothing was deleted (`docs/github-mirror.md`) |
| `mirror_failing` | action | the newest `jeryu/github-mirror` push failed for one or more repositories; ONE item for the whole forge, naming up to four of them and what git said; the step is on the forge host (SSH rewrite and deploy key) |
| `pr_changes_requested`, `pr_checks_failing`, `pr_awaiting_approval`, `pr_ready_to_merge` | action | an open, non-draft pull request; the first that applies, in this order. `pr_checks_failing` is `action` only when a context the base branch **requires** failed. `pr_awaiting_approval` needs green required checks; `pr_ready_to_merge` needs the PR unchanged for 10 minutes **and** no merge-queue entry building for it, because the queue is the thing that merges it, and is folded into `merger_stalled` once two or more have been ready for 30 minutes. `pr_changes_requested` is left out where the same pull request has a `reviewer_stuck` row. When none of those applies and only checks the branch does not require failed, `pr_checks_failing` is a `watch` item whose reason names the check and says it does not block the merge. A blocking `pr_checks_failing` repeats what the gate runner published about the failed attempt on that head, because its `pr-gate-runner` check run has no `details_url` and its build log is in a mode-0700 directory on the gate host: the check run's title (jeryu-ci-runner's `gate_failure_cause` prints a cause only for a few known failure shapes, rebuilt from allowlisted tokens), the attempt id from its summary, and the path that attempt's log is at on the gate host (`~/gate-runner/attempts/<owner>-<repo>-<sha>/<attempt>/build.log`). The step then says to fix that cause and re-run the gate; no line of the log is carried, and a gate that published no cause keeps the checks panel as the way in |
| `pr_changes_own_gate` | action | the diff of an open, non-draft pull request touches the paths that define the gate judging it: a `Justfile` anywhere, `ops/ci/**`, `ops/jankurai/**`, any `*jankurai*` file or `policies/**`, with `docs/**` left out. The gate runs the recipe at the pull request's own head, so such a diff rewrote the check that then reports on it and a green gate is the change proving itself. It is raised whatever the merge posture says — a green, mergeable pull request is the case worth seeing — and beside any other row for the same pull request, which is about something else. Ordinary test directories are deliberately NOT in the set: nearly every pull request touches tests, and a flag that fires on all of them says nothing. The step is a person reading the gate change against the base branch; jeryu-ci-runner's gate annotates its `pr-gate-runner` check run from the same path set (`ops/pr-gate/README.md`), and still reports its own conclusion unchanged |
| `pr_already_landed` | action | an open, non-draft pull request whose change is already on the live tip of its base branch: its live source-branch head commit is in the base's history, or `git merge-tree` of the head into the base yields the base's own tree with no conflicts (a squash, a cherry-pick or a replacement pull request landed it). It retains `pr_changes_own_gate` and replaces the other pull-request rows for that pull request, because reviewing, gating or merging it is no longer the step: closing it is. `action.api` is the call that performs it (`POST /api/v1/repos/:id/pulls/:number/close`): it comments the commit the work landed as and then closes the pull request, so the reason outlives the close, and it refuses any pull request that still has something to land (`pull_close_has_work_to_land`), which is why the one-click close exists only for this row. The branch is left alone. The check runs against a throwaway object directory and is cached by repository, base tip and head |
| `pr_draft_waiting` | action | an open draft with no push for `JERYU_DRAFT_IDLE_DAYS` days (default 3), whatever its base branch. A draft does not merge, is not reviewed by an automation and is not queued, so nothing else in the inbox would mention it; the step is to mark it ready for review or close it |
| `pushed_after_merge` | action | a branch holds commits a pull request's merge (or its close) did not take: the pull request closed within the last 30 days with that branch as its head, no pull request from the branch is open, and the branch is ahead of that pull request's base. Such a commit reaches nothing — no open pull request carries it, no gate runs for it, nothing lands it — and once a merge deletes the head branch, a push recreates the branch with no pull request at all. ONE item per branch, naming the pull request and up to ten of the commits. Receive-pack refuses updates and late-work recreations of same-repository branches whose pull request merged within 30 days, unless a new PR carries them or an explicit administrator exception is durably audited for the exact update. Restoring a deleted branch at exactly the recorded merged head is allowed so a fresh PR can be opened. Fork PRs, default branches and branch deletion are excluded. A rejected batch reports the offending ref and tells other refs to retry separately; no ref in the batch is updated. Receive-pack refuses the affected update (`this branch's pull request #<n> was already merged; push to a new branch and open a new pull request`); older stranded commits and branches closed without merging still surface here until a pull request from the branch opens, or the branch is deleted |
| `stranded_branch` | action | a non-default branch contains patch-unique commits absent from the default branch and has no open same-repository pull request; one item per branch. Each refresh scans at most 32 repositories, 64 eligible branches in total and 16 per repository, rotating the partial scan once per minute. Collections in the same minute inspect the same sorted window. The response's `branch_discovery` field explains that a missing row does not establish resolution. Each Git log is capped at 200 commits and subjects at 500 characters; a capped count is shown as "at least 200". The feed is admin-only, including private repository branch names and subjects. |
| `queue_failed` | action | a merge-queue entry failed or was dropped in the last 24 hours and its PR is still open |
| `queue_refused` | action | an enqueue the queue refused in the last 24 hours (stored as a dequeued entry carrying the forge code) while its PR is still open. The entry never built a commit, so queueing or merging it again is refused again: the step is per code — a replacement PR from the base with the commits cherry-picked for `queue_conflict` and `queue_merge_commits`, a new head for `queue_mismatch`, and for a code with no step of its own, reading what the queue reported and queueing again |
| `queue_stuck` | watch | an entry has been `building` for 30 minutes or more while its PR is open: a queue commit normally gates in a few minutes, so either no runner picked it up or its gate never reported |
| `pr_branch_campaign` | the worst of the rows it folds | three or more open pull requests that merge from the same branch, in any repositories, got the same verdict (the same `pr_*` or `queue_stuck` kind). A campaign is one change pushed to one branch in many repositories, so the same review on each of them is one decision for the owner: the branch is answered once and every pull request on it follows, where a row apiece reads like that many decisions and buries the rest of the inbox (live on 2026-10-06: about twenty `pr_changes_requested` rows for `dev/jankurai-v3-receipts`). The folded rows are replaced by this one, whose reason names the branch, the count and how many repositories, lists every `owner/name#number`, and whose `pulls` links each pull request; `href` is the search for the branch, which lists exactly them. The step is the one every folded row asked for, and no `action.api` is carried, because one call would perform it on one pull request only. Two pull requests on a branch, or three with different verdicts, keep their own rows. `pr_ready_to_merge` rows are folded into `merger_stalled` first, so a stalled merger is still reported as one |
| `merger_stalled` | critical | two or more pull requests have been `pr_ready_to_merge` for 30 minutes or more. The reviewer merges approved work on every pass, so half an hour of several ready and none merged is the reviewer having stopped, not a queue of owner decisions; the individual ready rows are folded into this one (it lists up to six `owner/name#number` and counts the rest, and no `pr_ready_to_merge` row is emitted for them), and its reason names the command that says what the reviewer last did (`journalctl --user -u pr-redteam | grep "waiting to merge"`). A single ready pull request, or one ready for less than 30 minutes, stays an ordinary `pr_ready_to_merge` row |
| `reviewer_offline` | critical | one or more pull requests wait for review and either no reviewer heartbeat has arrived within `JERYU_REVIEWER_HEARTBEAT_OFFLINE_MINUTES` minutes (default 10), or no review pass has completed within `JERYU_REVIEWER_PASS_OFFLINE_MINUTES` minutes (default 15). The row links to `/runners`, carries the last heartbeat time when one exists, and includes the count of pull requests waiting for review |
| `reviewer_stuck` | action | the automated reviewer's last verdict on a still-open PR is `hold`, `too_large`, `publication_rejected` or `failed`. When the same pull request also has changes requested — the usual case for a `hold` — only this row is emitted: it names the reviewer, the verdict and the review, so a `pr_changes_requested` row beside it would only repeat it |
| `gate_runner_down` | critical | no gate runner slot is online while a PR is open or the merge queue is building |
| `workers_down` | critical | a family has open or claimed todos and no healthy worker slot (the supervisor slot does not count) |
| `release_staged` | action | a repository's newest `release.staged` event names a commit production does not run and is newer than the live deployment; `action.command` is the event's `detail.deploy_command`. One item per repository, so a release staged in one repository does not hide one staged in another |
| `release_stage_failed` | critical | a repository's newest `release.stage_failed` with `needs_human` is newer than that repository's newest `release.staged` |
| `deploy_failed` | critical or watch | a repository's newest production deployment ended in `failure` or `error`. `watch` when the attempt left production as it was, because the live deployment is a successful one of the very release the attempt tried to deploy (release names decide it when both payloads name one, else the commits): production runs that release and needs no deploy |
| `release_board_problem` | action or watch | what a family's release board (`/releases`, `docs/release-board.md`) is showing and nothing else notices. `action` per lane with a stage in state `bad`, whose `href` is that lane on the board (`#lane-<id>`), and `action` once per family for the sources its collector could not read on its newest run. `watch` when the newest snapshot was observed 15 minutes ago or more, or carries no readable `observed_at`: the collector posts every five minutes, so the page is showing something older than now. Items of one board have ids of their own, so acknowledging a red lane leaves the others |
| `pin_behind` | action or watch | a deploy repo's pin misses green, merged work of a dependency (see [Pins](#pins)). A `commit` pin with no bump open is `watch` with no command while the dependency's newest commit (`latest_at`) is younger than 20 minutes, because auto-pin is about to open the bump; after that, or when `latest_at` is missing or unreadable, it is `action` and the command starts auto-pin (`systemctl --user start jeryu-auto-pin.service`), which builds the web bundle, changes the two lock fields and opens the pull request. The id is the same on both sides of the line. When auto-pin has given up on the dependency's current head (the newest `pin.bump_failed` with `needs_human` whose `sha` is `latest_sha`) it is `action` at once, grace or not: the title says auto-pin gave up on `<sha7>`, the reason leads with the event's `reason` (else a `reason` in `detail`, else the last line of `log_tail`) on one trimmed line, and the command clears the give-up marker and retries (`rm -f ~/.local/state/jeryu-auto-pin/failures/<sha> && systemctl --user start jeryu-auto-pin.service`); a give-up for an older head is ignored. With a bump pull request open it is `watch` and `href` is that pull request; a `tag` pin is `watch`, because nothing cuts tags. Gone when the pin is current |

### Block kinds

A blocked todo's `block_kind` says what a worker could not get past, which
decides what the inbox asks of a person. It is read off the todo's title, its
note and its newest attempt's `outcome`, in this order, and is reported on
`GET /api/v1/shift/todos` too. Releasing is offered only where releasing can
help.

| `block_kind` | Read from | `action.label` |
|---|---|---|
| `owner_task` | `OWNER:`, `needs the owner`, `only the owner` | `Do it, then mark done` |
| `over_budget` | `over_budget`, `over budget`, `budget cap`, `cost cap`, `out of budget` | `Close and refile smaller, or raise the cap` |
| `unknown_repo` | `unknown repo`, `is not in family`, `not in the family config` | `Add the repo to the family config, then release` |
| `handoff` | status `handoff`, or `handoff`/`by hand` in the words | `Release the todo` |
| `agent_blocked` | anything else | `Release the todo` |

The two labels that end in a release, and `owner_task`'s, carry the call that
performs them in `action.api`; `over_budget` and `unknown_repo` carry none,
because both steps start outside this API.

An over-budget todo is never offered a release: spend is summed over every
attempt, so the next attempt stops where the last one did. An owner's task is
not a worker's to pick up at all.

### Acknowledgements

Not every item is a todo, and not every item can be fixed today: a mirror
everybody knows is failing, or a draft kept open on purpose, otherwise asks
for a person every ten seconds. An acknowledgement defers one item by its own
`id`, whatever kind it is, until a date; after that date the item is listed
again, because the acknowledgement said "not now", not "never".

```json
POST /api/v1/attention/acks
{"item_id": "mirror-failing:forge", "until": "2026-12-01T00:00:00Z", "note": "waits on the new deploy key"}
```

`201` with the stored acknowledgement, or `204` when `until` is `null`, which
drops it and lists the item again. `GET /api/v1/attention/acks` lists every
acknowledgement, expired ones too: they are the record of what somebody
decided to live with. An `item_id` whose cause is already gone is stored and
simply never matches. Kept in `<data_dir>/shift.sqlite`
(`db/migrations/0006_attention_acks.sql`).

A todo is deferred the other way round, as queue state rather than as an
inbox row: `POST /api/v1/shift/todos/:family/:id/action` with
`{"action": "park", "until": ...}` (see [Todo actions](#todo-actions)).

### Todo actions

`POST /api/v1/shift/todos/:family/:id/action` takes `{"action", ...}` and
answers with the stored todo. Every action is admin-only, and none of them
moves a `done` todo.

| Action | Body | What it does |
|---|---|---|
| `release` | `note?`, `force?` | back to `open`, lease and park cleared, attempts reset to 0, and the note replaced by `note` (empty when none is given: the reason the todo stopped is no longer true). A todo whose claim lease is still live is being worked on right now, so releasing it would let a second worker claim the same work: that answers `409 claim_live` unless the body says `"force": true` |
| `block` | `note` | `blocked`, attempts kept; the note is what the inbox reads the `block_kind` from |
| `done` | `note?` | `done`: the work is finished, by hand or otherwise, and the item leaves the inbox |
| `close` | `note?` | `closed`: it will not be done. Like `done` it asks nobody for anything, and unlike `done` an admin may still release it |
| `park` | `until?` (RFC 3339), `note?` | `parked` with `park_until`, a state the inbox only watches until that time. No date parks it until a person acts |
| `edit` | `title?`, `body?`, `repos?` | overwrites the fields given. A repo the family config does not list is `422`; a title and repos together triage the todo |
| `priority` | `value` 1..4 | |
| `mode` | `value` `now` or `night` | |

## Pins

What could be released. A deploy repo pins its dependencies: the web app by
commit in its `*-split.lock.toml`, Rust crates by git tag in its Cargo
manifests. When a dependency's default branch moves, `GET /api/v1/pins` says
how far behind each pin is and what a bump would ship. It reads the hosted bare
repositories directly and is cached for 60 seconds.

```json
{"schema_version": "jeryu.pins/v1", "generated_at": "...",
 "consumers": [{"repo": "jeryu/jeryu-deploy", "family": "jeryu", "branch": "main", "pins": [...]}]}
```

A consumer is every hosted repository whose default branch has a root
`*-split.lock.toml`. A consumer with nothing to report has `"pins": []`.

| Pin field | Notes |
|---|---|
| `dependency` | `owner/name` of the hosted repository: under the consumer's owner, else the only hosted repository with that name. A dependency this forge does not host is left out |
| `kind` | `commit`: a lock `[[repo]]` entry with a 40-hex `commit` and a `web_dist_sha256`, the only lock entry the release build uses (the lock's crate tags are not read by cargo and drift, so they are never reported); also a git dependency pinned by `rev`. `tag`: a git dependency with `tag = "..."` in the root or a `crates/*/Cargo.toml` manifest, whatever host its URL names |
| `source` | the file that holds the pin |
| `pinned_ref`, `pinned_sha` | the sha or tag as written, and the commit it resolves to (null when it does not) |
| `latest_sha`, `latest_at` | the dependency's default-branch head and its commit time |
| `behind` | commits on the default branch the pin does not reach (counting stops at 1000) |
| `latest_green` | the combined commit status of `latest_sha`: true, false (failing or pending), or null when nothing posts statuses there |
| `state` | `current`; `behind` (behind, and the head is green or ungated); `behind_not_green`; `diverged` (the pin is not an ancestor of the head); `unknown` (unresolvable) |
| `bump_pr` | `{"number", "state", "url", "title", "head_ref", "head_sha", "created_at", "changes_requested", "blocking_review"}` of an open pull request in the consumer titled `release: pin <dependency name> ...`, or from an `auto/pin-` branch naming the dependency; else null |
| `unreleased` | up to 20 `{"sha", "subject"}`, newest first: what a bump would ship |

Lock shapes it does not know (a `repo = "..."` entry, `commit = "PENDING"`) are
skipped, never an error.

**The web pin bumps itself.** `scripts/release/auto-pin.sh` (timer
`jeryu-auto-pin.timer`, every 5 minutes on the release host) opens the bump
pull request once jeryu-web main is green: `release: pin jeryu-web <sha7>` from
`auto/pin-web-<sha12>`, exactly the two lock fields, listing the commits it
ships. It only proposes; review, the merge queue, auto-stage and a person's
deploy follow as for any change. A build takes about three minutes, so the
inbox gives it 20 minutes from the dependency's newest commit before it asks a
person to start `jeryu-auto-pin.service` by hand. Tag pins have no automation: the item tells a
person that work is waiting for a tag.

## Todo truth

`GET /api/v1/shift/todos` items gain:

- `merged`: the file value, or derived. todoq writes `merged = true` only for a
  todo another todo waits on, so the server checks the hosted repositories:
  every commit in `commits` is an ancestor of the family's base branch, or
  (shift PRs are rebased, so shas change) one of the last 500 base-branch
  commits carries the trailer `Todo: <id>`. A repository this forge does not
  host keeps the file value.
- `released`: `true`, `false`, or `null` when no production deployment is known
  for any of the todo's repos. A repo with its own production deployment uses
  its sha; a repo that ships inside another's release is looked up in that
  deployment's payload as `<repo_name_with_underscores>_commit`
  (`jeryu_web_commit`).
- `landing`: one row per repo the todo names or records, so `/work/<id>` can
  link landed and saved work without parsing `note`. Each row has `repo` and
  `state` (`landed`, `gate_failed`, `conflict`, `push_race`, `kept`, or
  `pending`). Optional fields appear only when known: `target` (the branch
  todoq tried to land on), `sha`, `main_sha` (the commit on the base branch,
  found by ancestry or the `Todo: <id>` trailer), `rescue_branch`,
  `rescue_sha`, `rescue_ahead` (rescue commits not already on the base by
  patch), `failing` (up to ten failing step or test names), and `check` (a
  check run name on `rescue_sha`, such as `todoq/gate`). The server first reads
  a structured `landing` front-matter value when todoq writes one, then falls
  back to todoq's fixed note wording and the todo's `commits`, and finally
  verifies rescue branches in the hosted repo.
- `pr`: `{repo, number, state, url}` or null, the shift pull request that
  carries the todo; `prs` lists one per repo it committed to. `repo` is the
  family repo name, as in `commits`. `state` is the core pull request state
  (`mergeable`, `merged`, `closed`, ...), as on the shift cards.
- `cost_usd`: the sum over `worked_by`, or null.
- `triage_cost_usd`: what triaging the todo cost, or null when todoq recorded
  none.
- `claimed_at`, `claim_model`: when the attempt in progress began and which
  model runs it, as the claiming worker wrote them; null unless the todo is
  `claimed` (an attempt reaches `worked_by` only once it ends) and from older
  workers.
- `reworks`: todo ids this todo redoes. API edits accept at most 64 unique IDs in the same queue and refuse missing targets, self-reference and cycles. Read from optional front matter as one
  string or a list. Editing a todo can set or clear this relationship.
- `worked_by[]` attempts carry, besides `by, host, slot, model, session,
  started, ended, outcome, cost_usd, note, shift`, whichever of these the file
  has (omitted when it has not): `agent` (harness CLI, e.g. `claude` or
  `codex`; absent means the operator default), `factory`
  (`agent:model:p<prompt sha8>`), `run` (run directory name), `experiment`
  (routing experiment; absent means not routed), `arm_p` (the probability the
  arm had of being drawn), `cost_estimated` (`true` when the cost was priced
  from a transcript because the run was stopped first) and `usage`
  (`{"<model>": {input, output, cache_read, cache_write, thinking, cost_usd,
  cost_basis, ...}}`, numbers as numbers). `outcome` may also be `killed`.
- `park_until`: when a `parked` todo comes back by itself, or `""`.
- `block_kind`: on `blocked` and `handoff` todos, what kind of help it needs
  (see [Block kinds](#block-kinds)); null on every other status.

The route is polled every 30 seconds, so the work is bounded: one `rev-parse`
per family repo per request, per-todo git only when its base branch or
production deployment moved, and merged or released work is never re-checked.
A family whose repos live under another owner than its queue resolves them by
unique repository name.

## Work trace

`GET /api/v1/trace` answers where one piece of work is in one call. A todo and
the pull request that carries it are the same work, and nothing said so before:
the event log filters by `todo_id` **or** by `repo` and `pr` and never joins
them, so a todo's page knew the queue file and a pull request's page knew its
own events. The trace is that join, made on the `Todo: <id>` trailer every
landing carries.

Ask for one todo or one pull request:

- `GET /api/v1/trace?todo=<id>`
- `GET /api/v1/trace?repo=<owner>/<name>&pr=<number>` — every todo whose
  trailer is on one of the pull request's commits, traced together

Anything else is `422 trace_invalid_query`; a todo no family queue holds is
`404 trace_todo_not_found` and a pull request this forge does not host is
`404 trace_pull_not_found`, never an answer with empty stages.

```json
{"schema_version": "jeryu.trace/v1", "generated_at": "2026-10-03T07:00:00Z",
 "subject": {"todos": ["20261003-020951-c0c38e"], "family": "acme",
             "family_label": "acme", "repo": "acme/widgets-web", "pr": 7,
             "shift": "nightshift/2026-10-03", "released_by": "acme/deploy"},
 "stages": [{"stage": "pinned", "state": "done",
             "at": "2026-10-03T06:10:00Z",
             "source": {"from": "derived"},
             "summary": "acme/deploy pins it at 4e11f0b, which reaches the work",
             "href": "/releases?repo=acme/deploy",
             "attention": []}]}
```

| Field | Meaning |
|---|---|
| `subject.todos` | every todo the trace follows, by id |
| `subject.repo` | the repository the work lands in, `owner/name` |
| `subject.released_by` | the repository whose release carries the work, when the work's own repository ships by being pinned into another; null when it releases itself |
| `stages[].stage` | `filed`, `claimed`, `done`, `shift`, `pr`, `gate`, `review`, `queue`, `merged`, `pinned`, `staged`, `deployed`, always all twelve and always in this order |
| `stages[].state` | `done`, `active` (running now), `waiting`, `blocked` (a person is the next step), `failed`, `skipped` (it will not happen and nothing is missing), `not_applicable` (this work never passes this stage) or `unknown` (nothing the forge can read says) |
| `stages[].at` | when the stage reached that state, RFC 3339, or null |
| `stages[].source` | `{"from": "event", "seq": 30}` when a stored event said so (read it with `GET /api/v1/events?after_seq=29&limit=1`), `{"from": "derived"}` when it was read from the repositories, `{"from": "reported"}` when another system filed it with the forge (a deployment, a review, a commit status) |
| `stages[].summary` | one line: what this stage says about the work |
| `stages[].href` | the in-app page that explains the stage |
| `stages[].attention` | the ids of the open [attention](#attention) items that belong to this stage, so a stuck stage carries its own next step |

Where each stage's answer comes from:

| Stage | Read from |
|---|---|
| `filed`, `claimed`, `done` | the newest `todo.*` event for the todo, else the queue file: its `filed_at`, its attempts and its status. A `blocked` or `handoff` todo is `blocked` at `done` and the summary is its own note; a `closed` one is `skipped` |
| `shift` | the todo's shift branch |
| `pr` | the shift pull request the todo names, or the one asked for: `done` when merged, `failed` when closed unmerged, `waiting` while it is a draft |
| `gate` | the newest `gate.*` event, else the combined commit status of the head |
| `review` | the newest `pr.approved`, `pr.review` or `review.*` event, else the newest review verdict on the pull request |
| `queue` | the newest `queue.*` event, else the merge-queue entry. A pull request that merged with no entry is `skipped`: the queue is not the only way in |
| `merged` | `pr.merged` or `todo.merged`, else the derived [todo truth](#todo-truth): every commit of the work is on the base branch, as itself or replayed under its `Todo:` trailer |
| `pinned` | the consumer pin `/api/v1/pins` reports for the work's repository: `done` when the pinned commit reaches the work, `waiting` while it is behind, `not_applicable` when nothing pins the repository |
| `staged` | the newest `release.staged` for whichever repository ships the work, `done` when the release it names carries the work. A deployment that carries the work also settles this: production cannot run a release that was never staged |
| `deployed` | what production runs: for a pinned repository the consumer's production deployment, else the todo's derived `released` |

**A repository that ships by being pinned.** jeryu-web has no release and no
deployment of its own: jeryu-deploy pins it and ships it, which is why
[todo truth](#todo-truth) leaves its work `released = null` for ever. The trace
answers it instead by reading the consumer's lock **at the commit in question**
— the pin on the consumer's branch for `pinned`, the staged commit for
`staged`, the deployed commit for `deployed` — and asking whether the
dependency commit it names reaches the work. Reaching means the work's commit
is that commit or an ancestor of it, counting both the sha the todo recorded
and the base commit that carries its `Todo:` trailer, because a shift branch
is replayed onto the base and the sha changes.

## Yield

`GET /api/v1/yield` reports spend against what reached main. Admin-only, like
the event log. Implementation: `crates/jeryu-api/src/web/yields.rs` (route)
and `yields/report.rs` (arithmetic).

Query: `family` (either spelling; an unknown family is `422 family_unknown`),
`since` and `until` (`YYYY-MM-DD`, UTC, both included; default `until` is
today and `since` 28 days before `until`), `by` = `model`, `agent`,
`experiment`, `family`, `repo`, `requester` or `week` (default `model`). A bad
value, `since` after `until`, or a key the route does not read is
`422 invalid_query`.

The unit of work is a todo, read with the same merged truth as
`GET /api/v1/shift/todos` ([Todo truth](#todo-truth)). A todo is in the window
when at least one of its attempts started in it; all of its attempts then
count. Author spend (an attempt's `cost_usd`) goes to that attempt's group. A
todo's outcome (merged, first pass), its `triage_cost_usd` and its review spend
go to the group of its landing attempt: the last `done` attempt, else its last.
Review spend is every `pr.reviewed` row of the cost ledger whose `repo` (by
name) and `pr` is one of the todo's shift pull requests (`prs`), split equally
over every todo that pull request carries.

Groups: `model` is the attempt's model; `agent` its harness, `claude` when
absent; `experiment` is `<experiment>/<agent>:<model>` or `(not routed)`;
`family`; `repo` is the todo's first repo; `requester` is `requested_by`;
`week` is the Monday (UTC) of the attempt's `started`. A missing value groups
as `(unknown)` (`(none)` for a todo without repos).

Where a todo ended up: **main** when merged; **scrap** when `blocked`,
`closed` or `handoff` and not merged; **pending** otherwise.

```jsonc
{
  "schema_version": "jeryu.yield/v1",
  "generated_at": "2026-10-14T12:00:00Z",
  "window": {"since": "2026-10-01", "until": "2026-10-14"},
  "family": null,              // or the canonical family key asked for
  "by": "model",
  "price_sources": ["config", "pricing@0000001"],  // distinct usage cost_basis, sorted
  "totals": Row,
  "groups": [Row & {"group": "model-a"}],          // spend_usd, highest first
  "weekly": [{"week": "2026-10-05", "group": "model-a", "merged": 1,
              "main_usd": 4.75, "usd_per_merged": 4.75}],  // group "all" for by=week
  "experiments": [{"experiment": "exp-1",
                   "arms": [{"agent": "codex", "model": "model-b", "arm_p": 0.25,
                             "enough_data": false, "row": Row}]}],
  "costliest": [TodoRef],      // top 10 by spend
  "scrap": [TodoRef]           // top 10 scrap by spend
}
```

`Row` (money in USD, rates 0..1, null when undefined): `todos`, `attempts`,
`merged`, `first_pass`, `reworked`, `rework_usd`, `spend_usd` (= `author_usd + review_usd +
triage_usd` for the arm's own todos), `author_usd`, `review_usd`, `triage_usd`, `main_usd`,
`scrap_usd`, `pending_usd`, `killed_usd` (attempts with outcome `killed`),
`estimated_usd` (attempts with `cost_estimated`), `yield` (`main_usd /
(main_usd + scrap_usd)`), `usd_per_merged` (`(main_usd + rework_usd) / merged`),
`usd_per_merged_ci95` (`[lo, hi]`, mean ± 1.96·sd/√n over the whole spend of
each merged todo landing in the group; null below two), `first_pass_rate`
(`first_pass / merged`). A merged todo is a first pass when exactly one of its
attempts is not `ratelimit`, `retarget` or `killed`, and no todo names it in
`reworks`. A reworked todo's spend is also charged as `rework_usd` and
`reworked` to the original todo's landing arm. A todo with its own `reworks`
relationship is marked `rework` in `TodoRef`, while its spend stays with its
landing arm. An experiment arm has
`enough_data` once it has 20 merged todos.

`TodoRef`: `{id, family, title, status, merged, rework, spend_usd, attempts, agent,
model}`, agent and model of the landing attempt.

### Ad-hoc sessions

`adhoc` is what agent sessions run by hand spent: the cost ledger's
`session.costed` rows recorded on a day of the window (UTC), and with `family`
only the rows of that family. This spend did no todo, so it is reported beside
the todo economics and is never part of `totals`, `groups`, `weekly`,
`experiments` or any `usd_per_merged`. The block is always present.

```jsonc
"adhoc": {
  "total_usd": 2.5,
  "sessions": 4,                  // distinct detail.session
  "unattributed_usd": 0.25,       // rows with neither repo nor family
  "by_family": [{"group": "globex", "usd": 1.75, "sessions": 2}],
  "by_repo":   [{"group": "globex/gadgets", "family": "globex", "usd": 1.0, "sessions": 1}],
  "by_agent":  [{"group": "claude-code", "usd": 2.5, "sessions": 4}],
  "by_model":  [{"group": "model-x", "usd": 2.5, "sessions": 4}],
  "weekly":    [{"week": "2026-10-12", "usd": 2.5}]   // Monday (UTC), as in weekly
}
```

A row's family is the one the ledger recorded (the producer's, or the one the
server derived from `repo` when it was posted), else the repository's family
when the report is read. Groups are sorted by `usd`, highest first, then by
name; `weekly` by week. A row without a family is in no `by_family` group, a
row without a repo in no `by_repo` group (`family` there is `null` for a
repository in no family); spend with neither is `unattributed_usd`. A missing
`detail.agent` or `detail.model` groups as `(unknown)`; a row without
`detail.session` counts no session.

## Human minutes per change

`GET /api/v1/human-minutes` reports how much of people's time each change
cost, and the engineering time behind each successful outcome: it is what the
Scorecard divides by. Admin-only, like the yield report. Implementation:
`crates/jeryu-api/src/web/human_time.rs`; store:
`db/migrations/0022_change_outcomes.sql`.

Query: `family` (either spelling; an unknown family is `422 family_unknown`),
`since` and `until` (`YYYY-MM-DD`, UTC, both included; default `until` is
today and `since` 28 days before it), `by` = `family`, `repo` or `work_kind`
(default `family`). A bad value, `since` after `until`, or a key the route
does not read is `422 invalid_query`.

### What is counted

Minutes are charged to a change lot -- a todo id, or `<owner>/<repo>#<N>` for
a pull request no todo carries, as the [flow ledger](#flow-ledger) spells it
-- and each charge is labelled with where it came from:

| `source` | What it counts |
|---|---|
| `review` | a person submitting a review or approval, or leaving a comment; passive reads and background diff polling do not write marks |
| `commit` | a commit a person authored, pushed to the branch of a lot's open pull request. The author is the account that proved that email address (`PUT /api/v1/auth/email` and its verification); a commit no account proved the address of counts for nobody |
| `session` | an `active` agent presence beat whose `operator` is a person and whose `links` name a todo or a pull request |
| `triage` | a todo action a person took through `POST /api/v1/shift/todos/:family/:id/action`: release, block, rework filing, any other |

Each of those writes a *mark*: the moment a person did something. A mark
charges the gap since that person's previous mark on the same lot from the
same source when that gap is at most 15 minutes; a longer gap is not work, so
the mark opens a new session and charges one minute for the action itself. A
mark that does not move forward in time (a commit pushed again) charges
nothing. Gaps are per person, so two reviewers on one pull request are two
sessions. The initial minute is an explicit session-opening estimate; repeat
marks charge only elapsed milliseconds, never another per-mark minimum. This
is an attention estimate from explicit actions, not a billing clock or a
measurement of passive reading. Diff GETs leave the ledger unchanged.

Whether an account is a person is the account kind the instance resolves
(`GET /api/v1/admin/users`, `crates/jeryu-api/src/web/people.rs`),
so the agent reviewer going through the same review routes is not human time,
and an anonymous read is nobody's.

### What is not counted

Agent time and model spend, which `GET /api/v1/yield` reports in dollars; time
nobody recorded (work off the forge, reading a page that is not a pull
request's files); and anything per person. `actor` is stored only so two
people on one lot are two sets of gaps: there is no per-person read, by
design. The answer repeats this in `not_counted`.

### Change outcomes

`change_outcomes` holds one row per lot, folded in the same transaction as the
event that moves it: `pr.merged` lands a change, `defect.linked` and
`incident.caused_by` fail one, `pin.bump_opened` makes the lot a `pin_bump`,
and the todo a `defect.linked` with `cause = rework` names in `detail.by` is
`rework`. A change is **successful** when it landed and no defect was linked
to it and no incident named it; the rate divides by those only.

```jsonc
{
  "schema_version": "jeryu.human_minutes/v1",
  "generated_at": "2026-10-14T12:00:00Z",
  "window": {"since": "2026-10-01", "until": "2026-10-14"},
  "family": null,              // or the canonical family key asked for
  "by": "family",
  "sources": [{"source": "review", "label": "...", "minutes": 42.5}],  // all four, in order
  "not_counted": ["..."],
  "totals": Row,
  "groups": [Row & {"group": "acme"}]   // by family key, repo slug or work kind
}
```

`Row`: `minutes` (the window's, on this group's lots), `review_minutes`,
`commit_minutes`, `session_minutes`, `triage_minutes`, `changes` (lots that
landed inside the window), `successful_changes`, `failed_changes`, and
`minutes_per_successful_change` (`minutes / successful_changes`, null while
the group landed none). Minutes count by the UTC day they fell on; changes
count by when they landed. A lot with no family groups as `(unknown)`, one
with no repository as `(none)`.

## Flow

`GET /api/v1/flow` reports the delivery-line stations. Admin-only, like the
event log and yield report. The live metrics start unknown until the flow
ledger has samples.

`GET /api/v1/flow/layout` reports the same stations with the tools whose
heartbeats serve them. It reads no query parameters; an unknown parameter is
`422 invalid_query`. The route returns station cards with each tool's
`runnerId`, `host`, `online`/`offline` state, `busy`/`idle` load, `lastSeen`,
reported `code`, failure state and station capacity.

Expected capacity is optional site configuration:
`JERYU_FLOW_EXPECTED_<STATION>`, where `<STATION>` is `QUEUE`, `WORK`,
`SHIFT`, `REVIEW`, `GATE`, `MERGE`, `PIN`, `STAGE` or `DEPLOY`. Absent expected
capacity is reported as unknown. A station is flagged when it has no reporting
tool, fewer online tools than its expected capacity, an invalid expected
capacity value, or a tool whose latest heartbeat shows a failure.

## Runner heartbeats

`/runners` is drawn from heartbeats, not from the event log. Gate runner slots,
the pr-redteam reviewer, the release timers (`auto-pin.sh`, `auto-stage.sh`),
host deploy timers and the jankurai audit runner (jeryu-ci-runner
`ops/pr-gate/bin/pr-gate-audit.sh`) all report through one route; the forge keeps the latest beat per `runnerId` in
memory, so a restarted forge repopulates within one tick of each reporter.
Implementation: `crates/jeryu-api/src/web/control_plane/gate_runners.rs`.

### `POST /api/v1/runners/heartbeat`

Who may post: a login in `JERYU_RUNNER_REPORTERS` (comma-separated site
setting, e.g. `ci-bot,review-bot`), or any global admin. The rule exists so an ordinary account
cannot paint fake runners; admins are not ordinary accounts, and the release
timers run as an admin account. Anyone else gets `403 permission_denied`.

```json
{"runnerId": "node-a/auto-pin", "host": "node-a", "slot": 0,
 "labels": ["automation"], "intervalSeconds": 300,
 "last": {"repo": "jeryu/jeryu-deploy", "pr": 74, "sha": "ea04cac1f0…",
          "recipe": "auto-pin", "conclusion": "opened", "seconds": 0,
          "finishedAt": "2026-09-20T04:10:00Z"}}
```

Unknown fields are refused. A malformed beat is `422 invalid_input` whose
`message` names the field.

| Field | Meaning |
|---|---|
| `runnerId`, `host`, `slot` | required. `runnerId` is the stable key, `<host>/<name>` by convention (`build-1/slot0`, `node-a/redteam`, `node-a/auto-stage`) |
| `labels` | optional. `redteam` marks the reviewer, `automation` a background timer, `deploy` a host deploy timer and `jankurai-audit` the jankurai audit runner (these four are exclusive); anything else is a gate slot |
| `intervalSeconds` | optional integer, 30 to 86400: how often this runner beats. Absent means the runner is offline after 180 seconds of silence; present, after `max(180, 3 * intervalSeconds)` |
| `current` | optional: `{repo, pr?, sha, recipe, startedAt}`, the work in hand |
| `last` | optional: `{repo, pr?, sha, recipe, conclusion, reason?, seconds, finishedAt}`, the newest finished work. Leave it out when there is no history |
| `last.reason` | optional: why a pass that went wrong ended that way, in the runner's own words (`no usable base ref: refs/heads/main does not exist`). It becomes the event's `reason`; without it the event still carries one derived from `conclusion` |
| `pr` | optional in both, for every label: absent or `null` when the work has no pull request (auto-stage stages a commit) |
| `last.conclusion` | by label. Gate slot: `success`, `failure`, `error`. `redteam`: `approve`, `hold`, `failed`, `interrupted`, `publication_rejected`, `too_large`. `automation`: `opened` (a pull request), `staged` (a release), `waiting` (behind `pr`, or for the gate of `sha` when there is no `pr`), `failed`. `deploy`: `deployed`, `failed`, `skipped`. `jankurai-audit`: `scored` (the forge recorded the governed report; the verdict itself is on `jankurai/proof`), `tool-failed` (the auditor exited nonzero with no usable report), `refused` (the forge refused the submission), `failed` (the head could not be fetched, so nothing ran) |
| `code` | optional: `{repo, commit, version?, installedAt?}`, the code the runner itself runs. `repo` is where it comes from (`acme/gate-scripts`, 1-200 characters, no control characters), `commit` the installed commit (7-64 lowercase hex digits), `version` a free-form label such as a tag (at most 100 characters), `installedAt` (or `installed_at`) when it was installed. Unknown fields inside are refused too. Leave it out when the runner does not know |
| `tools` | optional: `[{name, version?, sha256?}]`, the evaluation tools the runner uses, as it measured them (`{"name": "jankurai", "version": "1.6.11", "sha256": "<64 hex>"}`). At most 32; `name` 1-64 characters of `[a-z0-9._@-]`, unique within the list; `version` 1-100 characters, no control characters; `sha256` exactly 64 lowercase hex digits, the digest of the binary the runner invokes. Unknown fields inside are refused; a refusal names the entry (`tools[2].sha256: expected 64 lowercase hex digits`) |

The answer is `{"accepted": true, "runnerId": "…", "offlineAfterSeconds": 900}`
with the threshold that now applies to this runner.

A gate slot's or the reviewer's beat emits `gate.*` / `review.*` events when its
`current` or `last` changes (see Kinds). A `gate.finished` or `review.finished`
that did not go well always carries a `reason`. The same verdict on the same
head, beaten again after a restart or a retry, collapses onto the event already
stored: the `event_id` is derived from the runner, the head, the recipe and the
conclusion, so a retried pass does not repeat in the feed. An `automation` beat never emits an
event: the release scripts post their own `pin.*` and `release.*` events. Neither does a
`deploy` beat, nor a `jankurai-audit` beat: the score the audit runner submits is the record.

#### The jankurai audit runner

The gate host's auditor, `pr-gate-audit.sh` in jeryu-ci-runner `ops/pr-gate` (a
30-second timer installed with the gate, see its README), beats on every pass with
the gate identity's token, the same one it claims audits and submits reports with,
so that login must be allowed to report too:

```json
{"runnerId": "gate-a/jankurai-audit", "host": "gate-a", "slot": 0,
 "labels": ["jankurai-audit"], "intervalSeconds": 30,
 "current": {"repo": "acme/widgets", "sha": "89abcdef01…", "recipe": "jankurai audit",
             "startedAt": "2026-09-30T12:00:00Z"},
 "last": {"repo": "acme/widgets", "sha": "0123456789…", "recipe": "jankurai audit",
          "conclusion": "scored", "seconds": 41, "finishedAt": "2026-09-30T11:59:20Z"},
 "code": {"repo": "acme/gate-scripts", "commit": "<installed commit>",
          "installedAt": "2026-09-29T08:00:00Z", "version": "pr-gate 2.1.0"},
 "tools": [{"name": "jankurai", "version": "1.6.11", "sha256": "<64 hex>"}]}
```

`runnerId` is `<short hostname>/jankurai-audit`. A pass that claims nothing beats
with no `current` and the previous `last` unchanged (kept in the gate home's
`cache/audit-last.json`), so an idle auditor never invents a result. An audit
beats `current` when it starts, again every 60 seconds while it runs (an audit
can outlast the 180-second offline threshold), and `last` when it ends;
`last.reason` says why one that went wrong did. Audit jobs are per head, so `pr`
is left out. `code` is the gate's installed code, as its slots report it.
`tools` names the governed binary the auditor actually invokes, after the host's
verifier has checked it against its installation receipt: `version` from its
`--version`, `sha256` of the file. Beats are best-effort (5-second curl timeout,
never fail the pass); a `422` is retried once without `code` and `tools` for an
older forge. Masking the auditor's timer on a host stops both the audits and the
beats.

### Reading them: `GET /api/v1/control-plane/runners`

Each reporting runner is one entry of `local.nodeDetails`:

```json
{"runnerId": "ops-a/auto-stage", "kind": "automation", "source": "automation", "state": "active",
 "capacity": 0, "inFlight": 0, "labels": ["ops-a", "slot 0", "automation"],
 "classes": ["automation"], "activeTaskCount": 0,
 "lastUpdated": "2026-09-20T04:15:02+00:00", "activeTasks": [],
 "lastActivity": {"repo": "jeryu/jeryu-deploy", "pr": null, "sha": "77dc3310aa…",
                  "recipe": "auto-stage", "conclusion": "staged", "seconds": 0,
                  "finishedAt": "2026-09-20T04:10:00+00:00"},
 "offlineAfterSeconds": 900}
```

`kind` is what the node is, one of `gate`, `reviewer`, `automation`,
`deployer`, `jankurai-audit` (from the beat's labels) or `workcell`; group rows
by it rather than by labels. `source` is `pr-gate-runner`, `pr-redteam`,
`automation`, `deployer`, `jankurai-audit-runner` or `workcell`, and `classes`
says the same as `pr-gate`, `reviewer`, `automation`, `deployer`,
`jankurai-audit`. `state` is `offline` once
`lastUpdated` is older than `offlineAfterSeconds`; an offline runner is kept and
shown, never dropped. `lastActivity.pr` is `null` for work without a pull
request, and a task's `label` is then `<repo>@<sha7>` instead of `<repo>#<pr>`.
Only `gate` nodes hold a gate slot: reviewers, timers, deployers and the audit
runner have `capacity` 0 and are left out of `onlineRunners`, `offlineRunners`, the slot totals and the inbox's
`gate_runner_down` rule. `offlineAfterSeconds` is absent on workcell nodes,
which do not report by heartbeat.

A node whose beat carried `code` repeats it as
`"code": {"repo": "acme/gate-scripts", "commit": "0123456789ab…", "version": "gate-scripts-v1.2.0", "installedAt": "2026-09-30T12:00:00+00:00"}`
(`version` and `installedAt` only when sent). A runner that sent none, and every
workcell node, has no `code` key. Likewise a node whose beat carried `tools`
repeats them as `"tools": [{"name": "jankurai", "version": "1.6.11", "sha256": "<64 hex>"}]`
(`version` and `sha256` only when sent); with none there is no `tools` key.

The response also says what code the forge itself runs, at the top level:

```json
"forge": {"version": "5.0.0", "commit": "<40-hex jeryu-deploy commit>",
          "webCommit": "<40-hex jeryu-web commit>"}
```

`version` is jeryu-api's crate version; `commit` is the jeryu-deploy commit the
binary was built from (`JERYU_BUILD_COMMIT` at build time, which the release
build passes, else the checkout's `git rev-parse HEAD`) and `null` when the
build could not tell; `webCommit` is the jeryu-web commit `jeryu-split.lock.toml`
pinned, and so the SPA embedded. `GET /api/v1/version` carries the same
`commit` and `webCommit` beside `version` and `name`.

**How long a running pass takes.** A gate slot's or the reviewer's running
task carries `"estimate": {"typicalSeconds": 420, "slowSeconds": 600, "samples": 12}`:
the median and the 90th percentile of the newest 20 passes of the same recipe
on the same repository that ran to the end (`gate.finished` with outcome
`success`, `review.finished` with `approve` or `hold`; implementation
`crates/jeryu-api/src/web/pipeline/estimate.rs`). With fewer than three such
passes in the event log's 30 days there is no `estimate` key, and a page shows
elapsed time instead of a guess. The response's top-level `serverTime` is the
forge's clock when it answered, so a page can tick elapsed time from
`startedAt` without trusting the viewer's clock.

`GET /api/v1/repos/:id/pulls/:number/checks` says the same about the gate on a
pull request head. The pending required status the gate posted (its newest
row is `pending` and was written by the login the runner reports as; a status
any other reporter posted never qualifies) carries
`"running": {"recipe", "started_at", "typical_seconds", "slow_seconds", "samples"}`
while an online gate slot reports that head; the two figures are `null` with
too few passes. It names no runner (public repositories' checks are open to
anonymous readers). Every status row's `started_at` is the `pending` post that
opened its newest run and `completed_at` is `null` until it concludes; a status
concluded with no `pending` before it (a green head re-verified in place) has
`started_at: null`. The answer carries `server_time`.

## Shift heartbeats

`POST /api/v1/shift/heartbeat` is how a worker host says which slots it runs
and what each is doing; `/shifts` is drawn from those beats. It is the one
write under `/api/v1/shift/` that is not admin-only: a login named in
`JERYU_SHIFT_REPORTERS` (comma-separated site setting, no default, e.g.
`fleet-bot,pool-bot`) may post it, and so may any global admin. Anyone else
gets `403 permission_denied` naming the setting.

The grant is the heartbeat and nothing more. Filing todos, todo actions
(`POST /api/v1/shift/todos/:family/:id/action`) and opening a shift pull
request (`POST /api/v1/shift/shifts/:family/pr`) stay admin-only, so a
worker-pool host that only reports slots is given a reporter token rather than
an admin one. Reads under `/api/v1/shift/` are unchanged: any logged-in account
may make them, and being a reporter neither widens nor narrows that.

An operator's own box may keep reporting as an admin; the setting exists for a
host outside the forge's trust, such as an external worker pool. Give that host
a personal access token of the reporter login alone.

### Pool slots

A beat is about a queue slot unless it says otherwise. An external fleet -- a
pool of accounts, containers or browser profiles the forge does not run --
reports each member as its own slot and adds four optional fields:

| Field | Meaning |
|---|---|
| `kind` | `"todoq"` (the default, and what a queue slot means) or `"pool"` |
| `detail` | a few words behind `state`, e.g. `"sign-in needed"` for a paused member |
| `capacity` | how many jobs the member can hold at once |
| `active` | how many it holds now |

`GET /api/v1/shift/workers` returns all four on every row (`kind` is
`"todoq"`, the rest `null`, for a beat that left them out), so `/work` can say
how many members of a pool are available of how many and why one is not. An
unknown `kind` or a negative `capacity`/`active` is `422`.

In `GET /api/v1/shift/workers/history` a pool member contributes the largest
`active` it reported in the hour to that bucket's `busy`, rather than the one
slot a queue worker contributes: a pool of four accounts running five jobs is
five busy. A pool member that reports no `active` counts as one busy slot
while it works.
## Token usage

`GET /api/v1/usage?hours=24&bucket=5m[&family=]` answers what the worker slots
are spending: a chart of the token counters over time, and what each slot is
spending on the attempt it is running now. Implementation:
`crates/jeryu-api/src/web/usage.rs`, samples in `<data_dir>/shift.sqlite`
(`usage_samples`), pruned with the same 30-day retention as the event log.

### Reporting it

A producer reports **running totals for the slot's current attempt**, as a
`usage` object on its shift heartbeat (`POST /api/v1/shift/heartbeat`) or as
`detail.usage` on a `todo.attempt_finished` event. The totals **reset when the
slot moves to another todo**: they measure one attempt, not a slot's lifetime.

`detail.usage` on `todo.attempt_finished` comes in either of two shapes. The
flat object below is checked and stored as the slot's sample. The per-model
object todoq writes for the yield report (`{"<model>": {input, output,
cache_read, cache_write, thinking, cost_usd, cost_basis, ...}}`, see the todo
attempt fields above) is kept exactly as sent, because the yield report reads
it, and the sample is its sum over models: `input`, `output`, `cache_read` and
`cache_write` are the four token counts below, `cost_usd` adds up, and `model`
is the one model's name, or null when the attempt ran on more than one.

| Field | Type | Notes |
|---|---|---|
| `model` | string or null | the model the attempt runs on, as the producer spells it |
| `input_tokens` | integer | not negative; default 0 |
| `output_tokens` | integer | not negative; default 0 |
| `cache_read_input_tokens` | integer | not negative; default 0 |
| `cache_creation_input_tokens` | integer | not negative; default 0 |
| `cost_usd` | number or null | not negative |
| `turns` | integer or null | not negative |
| `updated_at` | string or null | RFC 3339, when the producer last read the totals |

The object is optional everywhere: a heartbeat or an event without it is
stored as before, and the slot's usage reads `null`. A negative count or cost
is refused with `422` (`shift_invalid_request` on the heartbeat,
`events_invalid_request` on the event), because a count nobody can chart is a
bug in the producer, not news. A beat that repeats the totals the slot already
reported for the same todo stores no sample, so an idle attempt costs one row
an hour, not one a beat.

### Reading it

`GET /api/v1/shift/workers` carries each slot's `usage`: the newest totals it
reported **for the todo it is working now**, or `null` when its producer
reports none.

`GET /api/v1/usage` takes `hours` (1 to 720, default 24; out of range is a
`422 usage_invalid_query`, never clamped), `bucket` (a count and a unit, `30s`
to `24h`, default `5m`; at most 2000 buckets per answer) and `family` (either
spelling of a family key; one the forge does not host answers
`422 family_unknown`). A key the route does not read answers
`422 usage_invalid_query` too.

```json
{"from": "2026-10-06T00:00:00Z", "to": "2026-10-07T00:00:00Z", "bucket_seconds": 300,
 "buckets": [{"start": "2026-10-06T00:00:00Z", "input_tokens": 1200, "output_tokens": 140,
              "cache_read_input_tokens": 90000, "cache_creation_input_tokens": 400,
              "cost_usd": 0.42}],
 "live": [{"slot": "w1", "operator": "dana", "host": "build-1.example", "family": "acme",
           "todo_id": "20261006-120000-a1b2c3", "model": "model-a",
           "input_tokens": 1400, "output_tokens": 200, "cache_read_input_tokens": 90000,
           "cache_creation_input_tokens": 400, "cost_usd": 0.55, "turns": 7,
           "updated_at": "2026-10-06T12:00:00Z", "tokens_per_minute": 312.5}],
 "totals": {"input_tokens": 1400, "output_tokens": 200, "cache_read_input_tokens": 90000,
            "cache_creation_input_tokens": 400, "cost_usd": 0.55}}
```

A bucket holds **deltas between consecutive cumulative samples of the same
`(slot, todo_id)`**, never negative. So a producer that restarted its counters,
or a slot that moved to the next todo, reports smaller totals than before and
that step counts as no spend at all — the same tokens are never counted twice.
An attempt already running when the window opened is measured from its newest
sample before the window, so a token counted in yesterday's chart is not
counted again in today's. Empty buckets are kept: a chart needs the gap.
`tokens_per_minute` is fresh input plus output plus cache-creation tokens a
minute over the attempt's samples in the window, and `null` until the attempt
has two samples apart in time. `totals` is the sum of the buckets.

### `usage.changed`

A sample that changed a slot's usage publishes one `usage.changed` frame on the
`pipeline` WebSocket scope, **at most one a second server-wide**: a dozen slots
beating every few seconds would otherwise repaint the page faster than a reader
can see it change. The payload is
`{slot, operator, host, family, todo_id, usage}`. Like `release_board.updated`
it is a nudge, not an event: nothing is written to the log, and a page that
missed a frame reads `GET /api/v1/usage` again.

## Release boards

`PUT /api/v1/release-board/{family}` (a `JERYU_BOARD_REPORTERS` login or an
admin) stores a family's `jeryu.release_board.v1` snapshot, which `/releases`
renders. A stored snapshot publishes one `release_board.updated` frame
`{family, observed_at}` on the `pipeline` websocket scope so open pages
refetch; it is **not** an event and is never written to the log, because the
collector posts every five minutes. The shape, the collector and its release
triggers are in [release-board.md](release-board.md).

## For agents

**Post an event.** Send a bearer token of an admin or a `JERYU_EVENT_REPORTERS`
login. Post best-effort with a short timeout, and never let a failed POST fail
the work it describes.

```sh
curl -sS --max-time 10 -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"event_id":"todoq:20260101-120000-a1b2c3:attempt:1","source":"todoq",
       "kind":"todo.attempt_finished","family":"jeryu","todo_id":"20260101-120000-a1b2c3",
       "outcome":"done","cost_usd":0.33,"seconds":212,"summary":"w3 finished: close the authz gap"}' \
  https://forge.example/api/v1/events
```

**Retrying.** Give every event an `event_id` (at most 64 characters of
`[A-Za-z0-9._:-]`) built from what makes it unique, e.g.
`<source>:<todo or sha>:<step>:<attempt>`. If a POST times out, send the same
body again: the forge returns the original `seq` and stores nothing new. Ids
are scoped to the posting login, and events without an `event_id` are never
deduplicated, so retrying those creates duplicates.

**Follow the log.** Read `latest_seq` once, then poll
`GET /api/v1/events?after_seq=<last seq you saw>` (oldest first) and advance
the cursor to the last event's `seq`. Add `todo_id=`, `repo=&pr=` or `kind=todo.`
to follow one thing. Or subscribe to the `pipeline` WebSocket scope with
`?after_seq=<last seq you saw>` and reconnect with the newest `payload.seq`
(see [WebSocket](#websocket)).

**Find out what needs a person.** `GET /api/v1/attention`, then for each item
read `next_step`. When `action.api` is set the step is that call and can be
replayed as it stands; when `action.command` is set the step is that shell
command, to be run by the operator on the host the reason names; otherwise
the step happens on the page at `href`. Items are current state: fix the cause and the
item is gone on the next call (within 10 seconds). Do not treat `needs_human`
on an old event as an open item.

**Find out what could be released.** `GET /api/v1/pins`: every pin with
`state = "behind"` is merged, green work that no release of the consumer would
include; `unreleased` lists it and `bump_pr` says whether the bump is already
proposed. Do not open a second bump while `bump_pr` is set.

**Find out where one piece of work is.** `GET /api/v1/trace?todo=<id>` (or
`?repo=&pr=`), then read the stages in order: the first one that is not `done`
is where the work stands, its `summary` says what that means and its
`attention` ids name the items waiting on a person. A stage's `source` says
whether to believe an event, the repositories or another system's report.

**Tell a missing route from success.** A `404` with
`code = "api_route_not_found"` means this server does not have the route; it
may be older than the client.

## Operations

- **Migration.** `0002_pipeline_events.sql` adds a table to `shift.sqlite`,
  which holds telemetry only. `switch.sh` does not snapshot that file, and does
  not need to: an older binary checks only the migrations it knows, so a
  rollback runs against the migrated file unchanged. The one startup stop
  besides a changed migration file is a version the store and the binary both
  know recorded under a different name (a renumbered migration); see
  `db/migrations/README.md`.
  `0010_cost_ledger.sql` adds `cost_ledger`, which is a record, not telemetry:
  rows older than 30 days exist nowhere else, so back up `shift.sqlite`
  (`sqlite3 shift.sqlite '.backup <file>'`) before deleting it or rolling the
  migration back. An older binary ignores the table; costs it stores during an
  overlap are not copied.
- **Environment.** `JERYU_EVENT_REPORTERS` (site setting, e.g. `ci-bot,review-bot`) and
  `JERYU_SHIFT_REPORTERS` (site setting, e.g. `fleet-bot,pool-bot`; unset, only
  admins may post shift heartbeats). todoq
  and `auto-stage.sh` post with an admin token and need no entry. The machine
  names the inbox sends an operator to, read on each inbox computation:
  `JERYU_RELEASE_HOST` (e.g. `node-a`; auto-pin, auto-stage,
  `deploy-release.sh` and the todoq workers), `JERYU_GATE_HOST` (e.g.
  `build-1`; the `pr-gate-runner@` slots) and `JERYU_FORGE_HOST` (e.g.
  `edge-1`; the forge itself, where mirror pushes are made by the user the
  forge runs as). A blank value counts as unset. They only change the text of
  `action.run_in`; nothing connects to these hosts. `JERYU_DRAFT_IDLE_DAYS`
  (default 3) is how many days a draft may sit before `pr_draft_waiting`
  reports it; a value that is not a positive whole number is ignored.
- **auto-stage.** After this lands on main, re-run
  `scripts/release/install-auto-stage.sh` on the release host so the installed
  copy posts `release.staged`; until then the inbox cannot know about a staged
  release. Staging output is also kept under
  `~/.local/state/jeryu-auto-stage/logs/`.
- **auto-pin.** Not enabled by a deploy. Run
  `scripts/release/install-auto-pin.sh` once on the release host (it needs
  docker for the pinned node image, a git credential that may push branches to
  jeryu-deploy, and the admin token file named by `JERYU_PIN_TOKEN_FILE`, mode 0600). Disable with
  `systemctl --user disable --now jeryu-auto-pin.timer`. State and build logs
  are under `~/.local/state/jeryu-auto-pin/`.
- **Deployment logs.** The switch log itself lives on the release host
  (`~/.local/state/jeryu-release/logs/`, named by the status's `log_path`) and
  its last 20 lines travel in the status's `log_tail`. `deploy-release.sh` sets
  `log_url` to the `deploy.status` events that carry them
  (`<forge>/api/v1/events?kind=deploy.status&repo=jeryu/jeryu-deploy`, override
  with `JERYU_RELEASE_LOG_URL`), so a failed status links to what `switch.sh`
  printed. Serving the whole log is a follow-up.
- **Redeploying what is live.** `deploy-release.sh` reads the live release name
  before recording anything: deploying the release production already runs
  prints "already live" and exits 0, recording no deployment (`--json` answers
  `already_live: true` with null ids). `switch.sh` is the same no-op when it is
  run on its own.

## Taking a todo on

A todo that stopped for a person (`blocked` or `handoff`) can be picked up by
any person who can work on it, not only by an administrator. A todo carries an
optional `assignee` (a forge login, front-matter key `assignee`). Through
`POST /api/v1/shift/todos/<family>/<id>/action`:

| Action | Who | Effect |
|---|---|---|
| `{"action": "claim"}` | a person who can work on it, when nobody has it | the caller becomes the assignee |
| `{"action": "assign", "value": "<login>"}` | a person who can work on it when nobody has it or they hold it; an admin | hands it to another person who can work on it |
| `{"action": "unassign"}` | the assignee or an admin | nobody has it again |
| `{"action": "done"}` | the assignee or an admin | finishes it |
| `{"action": "release"}` | admin | back to the workers; clears the assignee |

A person who is not an admin may `claim`, `assign` and `done` only a `blocked`
or `handoff` todo. No worker claims those (todoq claims only `open` and lapsed
`claimed` todos), so a person and a worker never meet on one, whichever todoq
version a worker host runs; and `done` cannot be undone, so work an admin
parked, closed or left to the workers is never finished by anyone else.
todoq also skips any todo with an assignee (`Todo.is_claimable`), which keeps
an admin's assignment of open work out of the pool.

"Can work on it" means write access to a repository the todo names. A todo
that names none (untriaged) could be about any of the family's repositories,
so it needs write access to every one this forge hosts; a grant on one
repository never reaches work on a sibling. An account with no grant — one
that only signed up, or that cannot read a private family — can take nothing
on and is not offered as an assignee. The stored assignee is the account
store's login, never the request's spelling of it, and an assignee who is no
longer an active person (deactivated, or marked as automation) holds nothing:
the todo is free to claim again.

Every other action stays admin-only and answers `403 todo_action_forbidden` to
anyone else, before the todo is looked up. A todo a worker is running (live
lease) answers `409 claim_live` to `claim`, `assign` and a non-admin's `done`,
as it does to `release`; only an admin's `{"force": true}` overrides it. Bots
and accounts that are automation never take a todo on. A person's action is
committed to the family's queue repository under their login, whatever their
grants on that repository: the queue's writers are the family's people.

An account is automation when an admin marks it `service`
(`PUT /api/v1/admin/users/<login>/kind` with `{"kind": "service"}`) or the
instance configuration names it: `JERYU_SHIFT_PR_AUTHOR`,
`JERYU_SHIFT_REPORTERS`, `JERYU_REVIEW_IDENTITY`, `JERYU_MERGE_IDENTITY`,
`JERYU_AUTOMATION_IDENTITIES`, `JERYU_CI_PUBLISHERS`, or `JERYU_JANKURAI_SCORERS`;
these configuration roles win over a stored mark, since those credentials
are held by machines. `JERYU_DEPLOYERS` names administrators allowed to record
deployments, including release-host accounts. Those logins default to service;
an admin can explicitly mark a person who deploys `human`. That mark does not
override any of the other automation roles above. A machine-held deployer can
be marked `service`, or listed in `JERYU_AUTOMATION_IDENTITIES` when its service
kind must remain enforced by configuration. Deployment permission still
requires both admin and deployer-list membership. The check fails closed: if the stored
marks cannot be read, nobody counts as a person. `GET /api/v1/admin/users`
reports each account's `kind` and `kind_source` (`set`, `config` or
`default`).

`GET /api/v1/shift/todos/<family>/<id>/people` lists the active people who
can work on that todo, for an "assign to" picker. Only an admin or someone who
can work on the todo may read it, and it never reaches past the todo's own
repositories, so it shows no more than the caller's own grants do.

Compatibility with existing Activity clients: the histogram accepts
`range=6h|24h|7d`, and usage accepts `range=24h`, alongside the explicit `hours`
queries. Conflicting windows are refused. Worker rows omit token and cost usage
for non-admin readers. A non-admin event reporter may attribute a usage sample
only to its own authenticated operator login; administrators retain fleet
reporting.

## Incidents

Four kinds, posted by any reporter (an alert webhook, a person, an agent) through
`POST /api/v1/events`. Each names its incident in `detail.incident_id`
(at most 64 characters of `[A-Za-z0-9._:-]`); a malformed one is a `422`.

| Kind | `detail` |
|---|---|
| `incident.opened` | `incident_id`, `detected_at` (RFC 3339, default: when posted), `severity`, `service`; the event `summary` is the incident's |
| `incident.restored` | `incident_id`, `restored_at` (default: when posted), `restoring_change` (a change lot key) |
| `incident.caused_by` | `incident_id`, `changes`: one or more change lot keys (or `change` for one). May arrive at any time, also after `incident.closed`; repeats are counted once |
| `incident.closed` | `incident_id`, `closed_at` |

They are folded, in the same transaction as the event, into `incident_ledger`
and `incident_causes` (migration 0019), which are never pruned. The fold does
not depend on order: a restore posted before the opening lands on the same
incident.

`GET /api/v1/incidents?family=` reads the ledger. Per incident:
`time_to_restore_seconds` is `detected_at` to `restored_at`; each cause has
`deployed_at` (the last time that lot left the Deploy station forward in the
flow ledger) and `time_to_detect_seconds` from it to `detected_at`, both null
when unknown. A change named by `incident.caused_by` has outcome `failed` and
is listed in `failed_changes`. An incident with no cause has
`cause_unknown: true` and `cause: "cause unknown"`; it still counts for time to
restore. `summary` holds the counts and the mean and median times.

`POST /api/v1/incidents` is what the authenticated "Report incident" form
sends: `incident_id` (optional; the server generates `inc-<UTC stamp>-<hex>`
when the reporter does not name one), `summary` (the only required field),
`service`, `severity`, `family`, `repo`, `detected_at`, `changes`,
`restored_at`, `restoring_change`, `closed`. A reporter sends what they know,
so one submission may open, attribute, restore and close at once, and what they
do not know yet arrives later as its own event on the same incident. It replies
`201` with the `incident_id`; a missing `summary` is a `422`
`incident_report_invalid`. Event ids are `report-<incident_id>-opened|caused-by|restored|closed`,
so sending the form twice reports the incident once. Posting needs the same
reporter identity as `POST /api/v1/events`.

`POST /api/v1/incidents/webhook` takes a generic alert body (or an array):
`incident_id`, `status` (`firing` or `resolved`), `service`, `severity`,
`summary`, `family`, `detected_at`, `restored_at`, `restoring_change`,
`changes`. It becomes the events above with source `alert-webhook` and event ids
`alert-<incident_id>-opened|restored|caused-by`, so a redelivered alert is
stored once. A vendor body is another `AlertAdapter` in `web/incidents.rs`.


Review-catch accounting records the reviewer's human/service kind when a forge
review or one-click approval is emitted. Automation route verdicts do not emit
human findings: the agent's structured finding reporter owns those events.
The route verdict and the agent's own report count at most one escape for the
agent row on a change. Historical route events without a recorded reviewer
kind remain unclassified rather than being assumed to be human reviews.

Revert defect attribution accepts only dedicated `Reverts #N` body lines on a
revert PR, resolves an existing merged target in the same repository, and emits
at most 50 stable, deduplicated links after scanning at most 1,000 lines.
Incidental issue references, absent/open targets and self references are ignored.
