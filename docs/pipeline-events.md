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
never HTML. A path under `/api/v1/` that no route matches answers
`404 api_route_not_found`.

Implementation: `crates/jeryu-api/src/web/pipeline.rs` and
`crates/jeryu-api/src/web/pipeline/` (store, emit points, attention rules),
`crates/jeryu-api/src/web/shift/truth.rs` and `shift/visibility.rs`,
`db/migrations/0002_pipeline_events.sql`.

## Access

| Route | Who |
|---|---|
| `POST /api/v1/events` | a global admin, or a login in `JERYU_EVENT_REPORTERS` (comma-separated, default `gatebot,pragent`) |
| `GET /api/v1/events` | global admins |
| `GET /api/v1/attention` | global admins |
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
| `actor` | string or null | who did the thing, e.g. `alton@xbabe0/w1`, `pragent`, `xbabe2/slot0`; clipped to 128 characters |
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
| `todo.filed`, `todo.action` | a todo is filed or released/blocked/re-prioritised from the web |
| `worker.stage` | a worker slot's state, stage or todo differs from its previous heartbeat; summary like `w1 jeryu: agent -> gate on <todo>`. An unchanged beat, or an idle slot appearing, emits nothing |
| `shift.pr_opened` | the Shift page opened a shift's review PR |
| `pr.opened`, `pr.review`, `pr.approved`, `pr.merged` | pull request steps on the v1 routes and the GitHub-compatible edge; `pr.review` has `outcome` `approve`, `request_changes` or `comment`, and `needs_human` for `request_changes`. Tagged with `family` and `shift` when the head is a shift branch |
| `queue.enqueued`, `queue.building`, `queue.landed`, `queue.failed`, `queue.dequeued`, `queue.refused` | merge-queue transitions. `queue.building` is a rebuild (moved base, or a retry after a red gate). `queue.landed` is followed by `pr.merged` with `detail.via = "merge_queue"`. `queue.refused` means the PR could not be replayed onto the base and somebody has to rebase it |
| `gate.started`, `gate.finished`, `review.started`, `review.finished` | a runner heartbeat's `current` or `last` differs from that runner's previous beat (`review.*` for the `redteam` label). A reviewer verdict of `hold`, `failed`, `publication_rejected` or `too_large` sets `needs_human` |
| `deploy.created`, `deploy.status` | a write to the Deployments API; `outcome` is the deployment status state, `needs_human` for `failure` and `error`. `deploy-release.sh` needs no change |

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

Query: `after_seq`, `before_seq`, `limit` (1 to 500, default 100), `family`,
`repo`, `pr`, `todo_id`, `source`, `kind`, `needs_human`. `kind` matches exactly,
or as a prefix when it ends with a dot (`kind=todo.`).

- Without `after_seq`: the newest events, **newest first** (a page to show).
- With `after_seq`: events with `seq > after_seq`, **oldest first** (a cursor
  tail to follow).

```json
{"schema_version": "jeryu.pipeline_events/v1", "events": [...], "latest_seq": 42}
```

### WebSocket

Every stored event is published on scope `pipeline` as a `WebEvent` whose
`payload` is the event object. The frame's own `seq` is the WebSocket hub's
in-memory counter; the durable cursor is `payload.seq`. Treat a frame as a
nudge and refetch with `after_seq`.

## Attention

`GET /api/v1/attention` is computed from current state on each call (cached
for 10 seconds), never from old events, so an item disappears when its cause
is fixed.

```json
{"schema_version": "jeryu.attention/v1", "generated_at": "...",
 "items": [...], "counts": {"critical": 0, "action": 3, "watch": 1}}
```

Items are sorted by severity, then oldest first.

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
| `action` | `{"label", "command"}`. `command` is a copyable shell line when the step happens off-site, else null |
| `next_step` | the one next step as a sentence: `<label>: run \`<command>\`` or `<label>: open <href>` |

Every item names exactly one next step: run `action.command` when it is set,
otherwise open `href` and do what `action.label` says.

| Kind | Severity | Meaning |
|---|---|---|
| `todo_blocked` | action | status `blocked`; the reason opens with the note, and the one step is to release the todo |
| `todo_handoff` | action | status `handoff` |
| `todo_untriaged` | action | open and `triaged = false`; workers skip it |
| `todo_stuck_claim` | watch | claimed, lease dead for 10 minutes or more |
| `todo_waiting_on_blocker` | watch | open, and a `blocked_by` todo is blocked, handed off, or done but not merged |
| `shift_without_pr` | action | a shift branch holds todos that are on no base commit (`unmerged_todos`), with no pull request or only a closed one. Being ahead by sha is not enough: a branch closed and replaced by a rebased one stays ahead for ever |
| `shift_stranded_work` | action | a shift's pull request already merged, and todos landed on the branch afterwards: their `Todo:` trailer is on no base commit, so the work is finished and on no open pull request |
| `pr_changes_requested`, `pr_checks_failing`, `pr_awaiting_approval`, `pr_ready_to_merge` | action | an open, non-draft pull request; the first that applies, in this order. `pr_checks_failing` is `action` only when a context the base branch **requires** failed. `pr_awaiting_approval` needs green required checks; `pr_ready_to_merge` needs the PR unchanged for 10 minutes. When none of those applies and only checks the branch does not require failed, `pr_checks_failing` is a `watch` item whose reason names the check and says it does not block the merge |
| `queue_failed` | action | a merge-queue entry failed or was dropped in the last 24 hours and its PR is still open |
| `reviewer_stuck` | action | the automated reviewer's last verdict on a still-open PR is `hold`, `too_large`, `publication_rejected` or `failed` |
| `gate_runner_down` | critical | no gate runner slot is online while a PR is open or the merge queue is building |
| `workers_down` | critical | a family has open or claimed todos and no healthy worker slot (the supervisor slot does not count) |
| `release_staged` | action | the newest `release.staged` event names a commit production does not run and is newer than the live deployment; `action.command` is the event's `detail.deploy_command` |
| `release_stage_failed` | critical | the newest `release.stage_failed` with `needs_human` is newer than the newest `release.staged` |
| `deploy_failed` | critical | a repository's newest production deployment ended in `failure` or `error` |
| `pin_behind` | action or watch | a deploy repo's pin misses green, merged work of a dependency (see [Pins](#pins)). A `commit` pin with no bump open is `action`, with the documented bump command; with a bump pull request open it is `watch` and `href` is that pull request; a `tag` pin is `watch`, because nothing cuts tags. Gone when the pin is current |

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
deploy follow as for any change. Tag pins have no automation: the item tells a
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

The route is polled every 30 seconds, so the work is bounded: one `rev-parse`
per family repo per request, per-todo git only when its base branch or
production deployment moved, and merged or released work is never re-checked.
A family whose repos live under another owner than its queue resolves them by
unique repository name.

## For agents

**Post an event.** Send a bearer token of an admin or a `JERYU_EVENT_REPORTERS`
login. Post best-effort with a short timeout, and never let a failed POST fail
the work it describes.

```sh
curl -sS --max-time 10 -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"event_id":"todoq:20260919-121040-5e3f6e:attempt:1","source":"todoq",
       "kind":"todo.attempt_finished","family":"jeryu","todo_id":"20260919-121040-5e3f6e",
       "outcome":"done","cost_usd":0.33,"seconds":212,"summary":"w3 finished: close the authz gap"}' \
  https://git.neverhuman.org/api/v1/events
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
to follow one thing. Or subscribe to the `pipeline` WebSocket scope and refetch
on each frame.

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
- **Environment.** `JERYU_EVENT_REPORTERS` (default `gatebot,pragent`). todoq
  and `auto-stage.sh` post with an admin token and need no entry.
- **auto-stage.** After this lands on main, re-run
  `scripts/release/install-auto-stage.sh` on the release host so the installed
  copy posts `release.staged`; until then the inbox cannot know about a staged
  release. Staging output is also kept under
  `~/.local/state/jeryu-auto-stage/logs/`.
- **auto-pin.** Not enabled by a deploy. Run
  `scripts/release/install-auto-pin.sh` once on the release host (it needs
  docker for the pinned node image, a git credential that may push branches to
  jeryu-deploy, and the alton2 token file, mode 0600). Disable with
  `systemctl --user disable --now jeryu-auto-pin.timer`. State and build logs
  are under `~/.local/state/jeryu-auto-pin/`.
- **Deployment logs.** `deploy-release.sh` still sets no `log_url`: nothing
  serves the switch log yet. A `deploy.status` event carries the status
  description; serving the log is a follow-up.
