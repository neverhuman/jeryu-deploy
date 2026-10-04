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

JSON is snake_case. Every refusal on these routes is a typed JSON error
(`code`, `message`, `reason`, `repair_hint`, `common_fixes`, `docs_url`),
never HTML. A path under `/api/` (any version) that no route matches answers
`404 api_route_not_found`. A `family` the forge does not host answers
`404 shift_family_not_found` on the shift todo and shift list routes, never an empty list.

Implementation: `crates/jeryu-api/src/web/pipeline.rs` and
`crates/jeryu-api/src/web/pipeline/` (store, emit points, attention rules),
`crates/jeryu-api/src/web/shift/truth.rs` and `shift/visibility.rs`,
`db/migrations/0002_pipeline_events.sql`.

## Access

| Route | Who |
|---|---|
| `POST /api/v1/events` | a global admin, or a login in `JERYU_EVENT_REPORTERS` (comma-separated site setting, e.g. `ci-bot,review-bot`) |
| `GET /api/v1/events` | global admins |
| `GET /api/v1/attention` | global admins |
| `GET`, `POST /api/v1/attention/acks` | global admins |
| `GET /api/v1/pins` | global admins |
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
| `family` | string or null | shift family |
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

### Kinds

Emitted by the forge (`source = "forge"`, `reporter = "forge"`):

| Kind | When |
|---|---|
| `todo.filed`, `todo.action` | a todo is filed, or released/blocked/finished/closed/parked/edited/re-prioritised from the web |
| `worker.stage` | a worker slot's state, stage or todo differs from its previous heartbeat; summary like `w1 jeryu: agent -> gate on <todo>`. An unchanged beat, or an idle slot appearing, emits nothing |
| `shift.pr_opened` | the Shift page opened a shift's review PR |
| `pr.opened`, `pr.review`, `pr.approved`, `pr.merged` | pull request steps on the v1 routes and the GitHub-compatible edge; `pr.review` has `outcome` `approve`, `request_changes` or `comment`, and `needs_human` for `request_changes`. Tagged with `family` and `shift` when the head is a shift branch |
| `pr.ready_for_review`, `pr.draft` | the draft lifecycle: `POST /api/v1/repos/{id}/pulls/{number}/ready` or `/draft`, or the `gh`-compatible `PATCH /api/v3/repos/{owner}/{repo}/pulls/{number}` with `{"draft": …}`. The summary names the actor ("acme/app#7 marked ready by dana"); `outcome` is `ready` or `draft`. Only the author and admins may make either transition, and each one also appends a `pull_request.ready_for_review` / `pull_request.convert_to_draft` audit row |
| `pr.skipped` | an automation looked at a pull request and did nothing, once per head: `detail.automation` is the tool (`merge-queue`, `pr-redteam`) and `detail.skipped` the reason (`draft`). Without it a draft sits with no signal at all |
| `queue.enqueued`, `queue.building`, `queue.landed`, `queue.failed`, `queue.dequeued`, `queue.refused` | merge-queue transitions. `queue.building` is a rebuild (moved base, or a retry after a red gate). `queue.landed` is followed by `pr.merged` with `detail.via = "merge_queue"`. `queue.refused` means the PR could not be replayed onto the base and somebody has to rebase it; it is also persisted as a `dequeued` entry whose `refusal_code` is the forge code, which is what raises `queue_refused` in the attention inbox |
| `gate.started`, `gate.finished`, `review.started`, `review.finished` | a runner heartbeat's `current` or `last` differs from that runner's previous beat (`review.*` for the `redteam` label; never for the `automation` label, see [Runner heartbeats](#runner-heartbeats)). A reviewer verdict of `hold`, `failed`, `publication_rejected` or `too_large` sets `needs_human` |
| `deploy.created`, `deploy.status` | a write to the Deployments API; `outcome` is the deployment status state, `needs_human` for `failure` and `error`; `reason` is the status description, which `deploy-release.sh` ends with the last meaningful line of switch.sh's output on failure. The status request may also carry `log_path` (the operator's kept switch log on the release host) and `log_tail`: the Deployments API stores neither, and the event's `detail` carries both (the tail cut to its last 20 lines and at most 6000 bytes of JSON, so the event fits the 8 KiB `detail` limit) |

There is no `status.posted`: commit statuses are too noisy, and `gate.*` covers
them. Server-side emission is best-effort: a failed insert is written to the
server log and never reaches the response of the request it rode on.

Posted by producers:

| Source | Kinds |
|---|---|
| `todoq` | `todo.claimed`, `todo.attempt_finished` (`outcome` `done`, `retry`, `blocked` or `ratelimit`; `cost_usd`, `seconds`, `log_tail`), `todo.merged`, `todo.waiting_on_unmerged`, `shift.exhausted`, `worker.error` |
| `auto-stage` | `release.staged` (`sha` is the staged commit; `detail` has `release`, `previous_release`, `deploy_command`), `release.stage_failed` (`log_tail`; `needs_human` on the final attempt) |
| `auto-pin` | `pin.bump_opened` (`repo` is the consumer, `pr` the bump, `sha` the dependency commit; `detail` has `dependency`, `from`, `to`, `web_dist_sha256`), `pin.bump_failed` (`log_tail`; `needs_human` on the second failure for the same commit) |
| `pr-gate` | `gate.log` (`outcome` including `timed_out` and `inputs_changed`, `seconds`, `log_tail`, a receipt subset in `detail`) |

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
both with different values is `422 events_invalid_query`), `before_seq`, `limit` (1 to 500, default 100), `family`,
`repo`, `pr`, `todo_id`, `source`, `kind`, `needs_human`. `kind` matches exactly,
or as a prefix when it ends with a dot (`kind=todo.`). Anything else (`kind=todo`,
`kind=Not A Kind`) answers `422 events_invalid_query`: it could only ever match nothing, and an
empty page would read as "no such events".

- Without `after_seq`: the newest events, **newest first** (a page to show).
- With `after_seq`: events with `seq > after_seq`, **oldest first** (a cursor
  tail to follow).

```json
{"schema_version": "jeryu.pipeline_events/v1", "events": [...], "latest_seq": 42}
```

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
{"schema_version": "jeryu.attention/v1", "generated_at": "...",
 "items": [...], "counts": {"critical": 0, "action": 3, "watch": 1}}
```

Items are sorted by severity, then oldest first. An item somebody
acknowledged (see [Acknowledgements](#acknowledgements)) is left out, and out
of `counts`, until the date it was acknowledged until.

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
| `action` | `{"label", "command", "run_in"}`. `command` is a copyable shell line when the step happens off-site, else null. `run_in` says where that line is run, as a short phrase naming the machine and the directory (`"node-a, any directory"`, `"node-a, in a jeryu/jeryu-deploy checkout"`); it is a string on every item whose `command` is set and the key is absent on every other item |
| `next_step` | the one next step as a sentence: `<label>: on <run_in>, run \`<command>\`` or `<label>: open <href>` |

Every item names exactly one next step: run `action.command` on the machine and
in the directory `action.run_in` names when it is set, otherwise open `href`
and do what `action.label` says. A command item's action:

```json
{"label": "Run auto-pin now",
 "command": "systemctl --user start jeryu-auto-pin.service",
 "run_in": "node-a, any directory"}
```

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
| `shift_stranded_work` | action | a shift's pull request already merged, and todos landed on the branch afterwards: their `Todo:` trailer is on no base commit, so the work is finished and on no open pull request |
| `mirror_diverged` | critical | the newest reconcile found GitHub holding commits the forge does not, or a tag GitHub published at another commit; ONE item PER repository, naming the commits, because each one is its own history question. Nothing was forced and nothing was deleted (`docs/github-mirror.md`) |
| `mirror_failing` | action | the newest `jeryu/github-mirror` push failed for one or more repositories; ONE item for the whole forge, naming up to four of them and what git said; the step is on the forge host (SSH rewrite and deploy key) |
| `pr_changes_requested`, `pr_checks_failing`, `pr_awaiting_approval`, `pr_ready_to_merge` | action | an open, non-draft pull request; the first that applies, in this order. `pr_checks_failing` is `action` only when a context the base branch **requires** failed. `pr_awaiting_approval` needs green required checks; `pr_ready_to_merge` needs the PR unchanged for 10 minutes **and** no merge-queue entry building for it, because the queue is the thing that merges it. When none of those applies and only checks the branch does not require failed, `pr_checks_failing` is a `watch` item whose reason names the check and says it does not block the merge |
| `pr_draft_waiting` | action | an open draft with no push for `JERYU_DRAFT_IDLE_DAYS` days (default 3), whatever its base branch. A draft does not merge, is not reviewed by an automation and is not queued, so nothing else in the inbox would mention it; the step is to mark it ready for review or close it |
| `queue_failed` | action | a merge-queue entry failed or was dropped in the last 24 hours and its PR is still open |
| `queue_refused` | action | an enqueue the queue refused in the last 24 hours (stored as a dequeued entry carrying the forge code) while its PR is still open. The entry never built a commit, so queueing or merging it again is refused again: the step is per code — a replacement PR from the base with the commits cherry-picked for `queue_conflict` and `queue_merge_commits`, a new head for `queue_mismatch` |
| `queue_stuck` | watch | an entry has been `building` for 30 minutes or more while its PR is open: a queue commit normally gates in a few minutes, so either no runner picked it up or its gate never reported |
| `reviewer_stuck` | action | the automated reviewer's last verdict on a still-open PR is `hold`, `too_large`, `publication_rejected` or `failed` |
| `gate_runner_down` | critical | no gate runner slot is online while a PR is open or the merge queue is building |
| `workers_down` | critical | a family has open or claimed todos and no healthy worker slot (the supervisor slot does not count) |
| `release_staged` | action | the newest `release.staged` event names a commit production does not run and is newer than the live deployment; `action.command` is the event's `detail.deploy_command` |
| `release_stage_failed` | critical | the newest `release.stage_failed` with `needs_human` is newer than the newest `release.staged` |
| `deploy_failed` | critical or watch | a repository's newest production deployment ended in `failure` or `error`. `watch` when the attempt left production as it was, because the live deployment is a successful one of the very release the attempt tried to deploy (release names decide it when both payloads name one, else the commits): production runs that release and needs no deploy |
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
| `release` | `note?` | back to `open`, lease and park cleared, attempts reset to 0 |
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
| `bump_pr` | `{"number", "state", "url"}` of an open pull request in the consumer titled `release: pin <dependency name> ...`, or from an `auto/pin-` branch naming the dependency; else null |
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
- `pr`: `{repo, number, state, url}` or null, the shift pull request that
  carries the todo; `prs` lists one per repo it committed to. `repo` is the
  family repo name, as in `commits`. `state` is the core pull request state
  (`mergeable`, `merged`, `closed`, ...), as on the shift cards.
- `cost_usd`: the sum over `worked_by`, or null.
- `park_until`: when a `parked` todo comes back by itself, or `""`.
- `block_kind`: on `blocked` and `handoff` todos, what kind of help it needs
  (see [Block kinds](#block-kinds)); null on every other status.

The route is polled every 30 seconds, so the work is bounded: one `rev-parse`
per family repo per request, per-todo git only when its base branch or
production deployment moved, and merged or released work is never re-checked.
A family whose repos live under another owner than its queue resolves them by
unique repository name.

## Runner heartbeats

`/runners` is drawn from heartbeats, not from the event log. Gate runner slots,
the pr-redteam reviewer, the release timers (`auto-pin.sh`, `auto-stage.sh`),
host deploy timers and the jankurai audit runner (`ops/ci/jankurai-audit-runner.sh`)
all report through one route; the forge keeps the latest beat per `runnerId` in
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

`ops/ci/jankurai-audit-runner.sh` (a 30-second timer on a gate host, installed by
`ops/ci/install-jankurai-audit-runner.sh`) beats on every run through
`ops/ci/jankurai-audit-heartbeat.sh`, with the token it already uses to claim
audits, so that login must be allowed to report too:

```json
{"runnerId": "gate-a/jankurai-audit", "host": "gate-a", "slot": 0,
 "labels": ["jankurai-audit"], "intervalSeconds": 30,
 "current": {"repo": "acme/widgets", "sha": "89abcdef01…", "recipe": "jankurai audit",
             "startedAt": "2026-09-30T12:00:00Z"},
 "last": {"repo": "acme/widgets", "sha": "0123456789…", "recipe": "jankurai audit",
          "conclusion": "scored", "seconds": 41, "finishedAt": "2026-09-30T11:59:20Z"},
 "code": {"repo": "acme/gate-scripts", "commit": "<VERSION commit>",
          "installedAt": "2026-09-29T08:00:00Z"},
 "tools": [{"name": "jankurai", "version": "1.6.11", "sha256": "<64 hex>"}]}
```

`runnerId` is `<short hostname>/jankurai-audit`. A run that claims nothing beats
with no `current` and the previous `last` unchanged (kept in
`$XDG_STATE_HOME/jeryu-jankurai-audit-runner/last.json`), so an idle runner never
invents a result. An audit beats `current` when it starts, again every 60 seconds
while it runs (`JERYU_AUDIT_BEAT_EVERY`; an audit can outlast the 180-second
offline threshold), and `last` when it ends; `last.reason` says why one that went
wrong did. Audit jobs are per head, so `pr` is left out. `code` is sent only when
`JERYU_AUDIT_RUNNER_REPO` is set, with `commit` from the install's `VERSION` file
and `installedAt` from its mtime. `tools` names the governed binary the runner
actually invokes (`$JERYU_GOVERNED_JANKURAI_BIN`): `version` from its
`--version`, `sha256` of the file. Beats are best-effort (5-second curl
timeout, never fail the run); a `422` is retried once without `code` and `tools`
for an older forge. `JERYU_AUDIT_HEARTBEAT=0` turns them off.

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
read `next_step`. When `action.command` is set the step is that shell command,
to be run by the operator on the host the reason names; otherwise the step
happens on the page at `href`. Items are current state: fix the cause and the
item is gone on the next call (within 10 seconds). Do not treat `needs_human`
on an old event as an open item.

**Find out what could be released.** `GET /api/v1/pins`: every pin with
`state = "behind"` is merged, green work that no release of the consumer would
include; `unreleased` lists it and `bump_pr` says whether the bump is already
proposed. Do not open a second bump while `bump_pr` is set.

**Tell a missing route from success.** A `404` with
`code = "api_route_not_found"` means this server does not have the route; it
may be older than the client.

## Operations

- **Migration.** `0002_pipeline_events.sql` adds a table to `shift.sqlite`,
  which holds telemetry only. `switch.sh` does not snapshot that file, and does
  not need to: an older binary checks only the migrations it knows, so a
  rollback runs against the migrated file unchanged.
- **Environment.** `JERYU_EVENT_REPORTERS` (site setting, e.g. `ci-bot,review-bot`). todoq
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
