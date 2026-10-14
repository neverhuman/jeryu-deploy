# Agent presence

Which coding agents are working on which repositories right now. Every
active agent — an ad-hoc CLI session of any vendor, a queue worker, a pull
request reviewer, a gate runner, one of jeryu's own agent runs — is listed on
the Agents tab of each repository it works on. A session registered for a
whole family is listed on every repository of that family. The fan-out
happens when the list is read, so one record appears and disappears on every
repository at once.

Decision: `docs/adr/0006-agent-presence-on-the-repo-agents-tab.md`.
Code: `crates/jeryu-api/src/web/agent_presence.rs` and `agent_presence/`.
Store: `<data_dir>/shift.sqlite`, table `agent_presence`
(`db/migrations/0012_agent_presence.sql`).

The name is *agent presence*: "sessions" and `/api/v1/agent-runs` already
name other things, and `/api/v1/agent-runs*` is admin-only by path.

## Access

| Route | Who |
|---|---|
| `POST /api/v1/agent-presence` | any login, for the scope it may report (below) |
| `DELETE /api/v1/agent-presence/:session_id` | the login that registered the session, or a global admin |
| `GET /api/v1/repos/:id/agent-presence` | any login that may read the repository; never anonymous |
| `GET /api/v1/agent-presence` | any login; it sees what it may read |

Reporting scope:

- A global admin, or a login named in `JERYU_PRESENCE_REPORTERS`
  (comma-separated site setting, e.g. `agents-bot,pool-bot`), reports any
  scope.
- Any other login reports only repositories it can write, and a family only
  when it can write every hosted repository of that family (a family with no
  hosted repository cannot be claimed). Otherwise `403
  presence_scope_forbidden`, naming each repository or family refused.
- A session belongs to the login that registered it while its row is stored (see the bounds below: rows go 14 days after the last beat, or earlier when their own reporter stores more than 2048). A beat from another login answers `409 presence_session_owned`. A `DELETE` of a missing or foreign session answers the same `404 presence_session_not_found`. Ownership checks and writes share one SQLite transaction. An admin may beat
  or end anyone's session; the session stays its reporter's.

Bot tokens (`jat_…`) reach only `/api/v1/repos/*`, so they read a repository's
list but do not report; reporters use a login or a personal access token.

## Write: `POST /api/v1/agent-presence`

Register or beat one session: an idempotent upsert keyed by `session_id`.
Every beat sends the whole current record; there is no partial patch.
JSON, snake_case; an unknown field is refused.

```json
{
  "session_id": "cli:3f2a9c1e",
  "agent": "claude-code",
  "model": "model-x",
  "host": "node-a",
  "operator": "ada",
  "lane": "agent-presence",
  "branch": "ada/agent-presence",
  "title": "Wire presence into repo tab",
  "state": "active",
  "repos": ["acme/widgets"],
  "families": ["acme"],
  "started_at": 1791800000000,
  "last_active_at": 1791800300000,
  "ttl_seconds": 300,
  "cost_usd": 1.23,
  "tokens": {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0},
  "links": {"pr": "acme/widgets#12", "todo": "acme:20261012-1200-abc123"}
}
```

| Field | Rule |
|---|---|
| `session_id` | required, `[A-Za-z0-9._:-]{1,64}` |
| `agent` | required, 1–32 characters |
| `model`, `host`, `operator` | optional, ≤ 64 characters; `host` is returned to admins only |
| `lane` | optional, ≤ 64 characters, a folder name: a `/` or `\` is refused |
| `branch` | optional, ≤ 200 characters |
| `title` | optional, ≤ 160 characters |
| `state` | required: `active`, `idle` or `ended` |
| `repos` | `owner/name` of hosted repositories, ≤ 64 distinct entries |
| `families` | family keys (either spelling, see `docs/family-key.md`), ≤ 8 distinct entries |
| `started_at`, `last_active_at` | required, milliseconds since the epoch, ≥ 0 |
| `ttl_seconds` | optional, 30–3600, default 300 |
| `cost_usd` | optional, ≥ 0, spend so far (a list-price estimate) |
| `tokens` | optional; `input`, `output`, `cache_read`, `cache_write`, each optional |
| `links` | optional; `pr` and `todo`, each ≤ 200 characters |

Text fields refuse control characters; a blank optional field is the same as
an absent one. Duplicate entries in `repos` or `families` are folded.

Answers:

- `200 {"ok": true, "session_id": "…", "expires_at": <ms>}`.
- `422 presence_invalid_request`: a field breaks the table above; the message
  names it.
- `422 presence_scope_empty`: neither `repos` nor `families` names anything.
- `422 presence_repo_unknown`: a repository the forge does not host. A
  private repository the caller cannot read is reported the same way, so the
  answer never confirms that it exists.
- `422 family_unknown`: a family the forge does not know.
- `403 presence_scope_forbidden`, `409 presence_session_owned`: see Access.

Checks run in that order: shape, scope empty, repositories, families, scope
permission, ownership.

## End: `DELETE /api/v1/agent-presence/:session_id`

Marks the session ended. `200 {"ok": true, "session_id": "…", "state":
"ended", "ended_at": <ms>}`; ending it again keeps the first `ended_at`.
`404 presence_session_not_found` when no beat registered the id.

## Expiry

`expires_at = received_at + ttl_seconds`. Past it, a session that did not end
reads as `stale`. A stale session is listed until 10 minutes after
`expires_at`, an ended one until 10 minutes after it ended; then both are
left out. A session that beats `active` or `idle` after ending is live again.
Rows not beaten for 14 days are deleted on write. Repeated ended beats preserve the original retention timestamp; they cannot keep ended history indefinitely. Listed rows (live, stale, or ended or expired less than 10 minutes ago) are capped at 64 per authenticated reporter and 1024 forge-wide (`429 presence_limit`). The cap is checked on every write that makes a row listed: a new session, and a beat that brings back a session that had stopped being listed. A row already listed keeps beating freely. Ending a session that is no longer listed (a `DELETE` or an `ended` beat) records it as ended when it expired, so ending never lists a row again. Stored rows are bounded at 2048 per reporter: past that, that reporter's own oldest rows that are no longer listed are deleted, and their session ids are released. At the forge-wide bound of 16384 stored rows, insertion reclaims unlisted history from the reporter with the most retained rows first, oldest first within that reporter. Listed rows are never evicted; insertion is refused (`429 presence_limit`) only when no unlisted row can make room. Reclaimed session ids are released. Beats and ends share a 120-write-per-minute budget per authenticated login (`429 presence_write_budget`). Reuse session IDs rather than minting one per beat.

## Read: `GET /api/v1/repos/:id/agent-presence`

Paged like every list (`limit`/`per_page`, `page`; `docs/pagination.md`).

```json
{
  "items": [
    {
      "id": "external:cli:3f2a9c1e",
      "source": "external",
      "session_id": "cli:3f2a9c1e",
      "agent": "claude-code",
      "model": "model-x",
      "operator": "ada",
      "lane": "agent-presence",
      "branch": "ada/agent-presence",
      "title": "Wire presence into repo tab",
      "state": "active",
      "via": "repo",
      "family": "acme",
      "started_at": 1791800000000,
      "last_active_at": 1791800300000,
      "expires_at": 1791800600000,
      "cost_usd": 1.23,
      "tokens": {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0},
      "links": {"pr": "acme/widgets#12", "todo": "acme:20261012-1200-abc123"},
      "host": "node-a",
      "collides_with": []
    }
  ],
  "total": 1,
  "page": {"limit": 100, "page": 1, "total": 1, "has_more": false}
}
```

- `id` is `<source>:<session_id>`, unique in the list.
- `state` is `active`, `idle`, `stale` or `ended`.
- `via` is `repo` or `family`; `family` is the repository's family key when
  it has one.
- `reporter` on external items is the authenticated login that registered the session. `operator`, `agent`, title and other external labels are self-reported and may be false.
- `host` and a gate runner's reporting login (`operator`) are present for global admins only.
- Optional fields are left out when unknown: `started_at` and `expires_at`
  are absent for sources that do not know them (see below); `last_active_at`
  is always present.
- `links` is always an object; `agent_run` is set on jeryu's own runs, which
  the web links to the existing terminal view.
- `collides_with` lists at most 64 other items, using a repository/branch index to bound matching and response size. It lists the other live items with `via: repo` on the same
  branch. A family-scoped session is not known to work on this repository,
  so it collides with nothing; an item with no branch collides with nothing.
  Nothing collides on the repository's default branch when a branch
  protection rule guards it, which is the forge default unless the repository
  opts out (it then moves only by merge, so sessions there are
  reading or reviewing); an unprotected default branch collides like any other
  branch. Two `queue_worker` items
  never collide with each other (a queue lands its slots on one shared branch).

Order: live first (`active`, `idle`), then `stale`, then `ended`; within
each, `last_active_at` newest first.

### Sources

| `source` | What | `via` | Notes |
|---|---|---|---|
| `external` | reported sessions naming this repository, or its family | `repo` when `repos` names it (even if `families` does too), else `family` | the reported record |
| `agent_run` | jeryu's running agent runs on this repository (not the companion shells) | `repo` | `links.agent_run` = the run id; finished runs are left out, since the store keeps no end time and `/agent-runs` lists them; `last_active_at` is the time of the read; no `started_at`/`expires_at` |
| `reviewer`, `gate` | online gate runners whose current pass is on this repository; a runner with the reviewer label is `reviewer`; deployment, automation and audit timers are excluded | `repo` | `title` is the recipe and pull request; `links.pr` = `owner/name#n`; `branch` is not known; `expires_at` is when the runner would read offline |
| `queue_worker` | healthy worker slots of this repository's family in the `working` state | `repo` when the slot's todo lists this repository, else `family` | the family's todo queue is read once per request, only when one of its slots is working; a slot whose todo lists other repositories is left out; when the todo cannot be read the slot is listed for the whole family; idle slots are never listed |

Forge-side items have no host in their ids: `session_id` is an opaque digest
of the runner or slot identity, because hosts are for admins only.

## Read: `GET /api/v1/agent-presence[?family=<key>]`

Reported (`external`) sessions only, for a family page or an overview. The
same item shape without `via`, plus `repos` and `families`:

- With `family`: sessions whose `families` names it or whose `repos` include
  a repository of it, live, stale and recently ended. `family` on each item is
  the requested key. An unknown family is `422 family_unknown`.
- Without it: every live (`active`, `idle`) session.
- Admins see every row. Anyone else sees a row only when it names a
  repository they may read, or a family with at least one repository they may
  read; `repos` and `families` then list only those.
- `collides_with` names other live items sharing a repository in `repos` and
  the branch, with the same default-branch and queue-worker exceptions.

Unknown query keys answer `422 invalid_query`.

`links.pr` must be `owner/name#positive-number`; `links.todo` is an identifier using letters, digits, dots, colons, underscores and hyphens. Clients must parse these references and construct their own same-origin paths; never use a reported link value directly as an `href`. Unknown-family responses reveal no family inventory.
