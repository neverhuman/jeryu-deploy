# Tool-finder operations

The system-wide tool-finder scans every split repository for duplicated code,
groups it into clusters (Shared tools → Findings, `/tools`), and lets an admin
propose a cluster as a shared tool in `jeryu-tool/tools-registry.toml`. This
page covers the operator-facing pieces: where the scan reads its sources, the
scheduled scan, and the proposal decision API. Code:
`crates/jeryu-api/src/web/tool_finder.rs`, `tool_finder/hosted.rs`,
`tool_finder_schedule.rs`, and `tool_proposals.rs`.

All `/api/v1/tool-finder/` routes are admin-only (`admin_only_path` in
`crates/jeryu-api/src/web/auth.rs`).

## Where the scan reads sources

A scan needs the source of every repository in a split family. There are two
ways to get it, tried in this order:

| Source | What it reads | Where it applies |
| --- | --- | --- |
| `working-tree` | The sibling checkouts beside each `--split-manifest` path, walked in place. | A development host that keeps checkouts. |
| `hosted` | The bare repositories this forge serves, read at each repository's default branch. | The hosted forge, which has bare repos and no checkout. |

The working-tree source is used whenever the configured manifests actually
resolve to checkouts on disk; otherwise the scan falls back to the hosted
source. Neither needs the other, and a host with no checkouts needs no
`--split-manifest` at all.

### How hosted discovery finds a family

A repository names a split family when its default branch has, at the repo
root, either `repos.manifest.toml` or a `*-split.lock.toml`. The authority
manifest names its members in `required_repos`, with the control plane under
`[control_plane]` and every other member one `[[repo]]` row; a lock names them
in its `[[repo]]` rows (`name` or `jeryu_slug`). Either way the names are
resolved back to hosted repositories the same way
`GET /api/v1/pins` resolves consumers: a repository under the naming
repository's owner first, else the one hosted repository with that name. The
naming repository is itself a member. Archived and disabled repositories are
left out. The family label comes from the manifest's `repo_family`, else the
forge's own grouping for the repository, else the lock file's prefix.

Each member's default branch is then materialized into a scratch directory —
`git ls-tree` for the blob list, `git cat-file --batch` for their contents —
which is removed as soon as the scan ends. Nothing is cloned and no checkout
is ever required.

### Bounds

Materialization is bounded on every axis, so one scan cannot fill a disk or
run forever:

| Bound | Value |
| --- | --- |
| Repositories per scan | 64 |
| Bytes per file | 512 KiB |
| Bytes per repository | 64 MiB |
| Bytes per scan | 512 MiB |
| Files per scan | 200,000 |
| Materialization wall clock | 300 seconds |

Whatever a bound drops is reported, never silently lost: see the `skipped`
list below, and the `tool_finder.scan.source` WebSocket event each scan
publishes on the `tool_finder.scan` scope before it starts fingerprinting.

### Private repositories

Hosted discovery materializes private repositories too, because it reads them
as the server rather than as a caller, and findings name files and line ranges
across the whole family. That is sound only while every `/api/v1/tool-finder/`
route stays global-admin-only. Do not widen those routes without first
splitting results per reader.

## `GET /api/v1/tool-finder/source`

What a scan would read right now, and what it would leave out. Admin-only,
like every other tool-finder route. It only discovers — it materializes
nothing — so it is cheap enough for the Findings page to ask on load.

```json
{
  "schema_version": "jeryu.tool_finder.source/v1",
  "generated_at": "2026-09-20T09:00:00Z",
  "kind": "hosted",
  "configured": true,
  "detail": "2 hosted repositories, read from their default branches in the bare repos",
  "split_manifests": [],
  "repos": [
    {
      "repo": "jeryu/jeryu-deploy",
      "family": "jeryu-split",
      "branch": "main",
      "commit": null,
      "private": true
    }
  ],
  "skipped": []
}
```

- `kind` is `working-tree`, `hosted`, or `none`.
- `configured` is `false` only when this server has no split family at all:
  no `--split-manifest` checkout, and no hosted repository whose default
  branch has a root `repos.manifest.toml` or `*-split.lock.toml`. `detail`
  says exactly that, and a scan started in that state returns `424` with code
  `tool_finder_not_configured`.
- `commit` is filled in by a scan (the commit the sources were read at); plain
  discovery leaves it `null`.
- Each `skipped` entry carries `repo`, a stable `reason`
  (`unreadable`, `no_default_branch`, `no_sources`, `repo_budget`,
  `byte_budget`, `file_budget`, `deadline`, `write_failed`) and a `detail`
  sentence.

## Scheduled system scan

The web server re-runs the system scan on a schedule so findings stay current
without anyone pressing "Run live scan".

| Setting | Meaning |
| --- | --- |
| `JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS` unset or empty | Scan every 24 hours (default). |
| `JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS=N` (`N > 0`) | Scan every `N` hours. |
| `JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS=0` | Scheduler disabled; scans run only on demand. |

A value that is not a non-negative integer also disables the scheduler. The
scheduler starts when the server has a possible scan source: split manifests
(`--split-manifest`) or at least one hosted repository. Whether a family is
actually discoverable is decided per scan, so a forge that gains its first
family file needs no restart.

The 24-hour default is deliberate: any server with a scan source runs a
system scan daily unless an operator opts out with `=0`. At startup
`jeryu serve` logs one line to stderr with the effective setting, e.g.

    tool-finder: scheduled scan every 24h (default; set JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS=0 to disable)
    tool-finder: scheduled scan disabled (JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS=0)
    tool-finder: scheduled scan disabled (no split manifests and no hosted repositories)

How it decides to scan:

- The loop wakes once an hour (the first check happens right after startup).
- A scan starts only when **both** the persisted scan (the stored cluster
  rows' creation time) and the last scan this process started are older than
  the interval. Because the age comes from persisted data, a deploy or restart
  neither skips a due scan nor triggers an extra one.
- It reuses the manual scan path (`POST /api/v1/tool-finder/scan`), so progress
  streams the same way, and it never overlaps a scan already running; a busy
  scan is simply retried on the next hourly wake.

Since checks are hourly, a scan may start up to an hour after it becomes due.

## Proposal decision API

`POST /api/v1/tool-finder/proposals/:tool_id/decision`

Approves or rejects a tool whose registry entry has `status = "proposed"`.
The registry file is edited in place, preserving comments and layout.

Request body:

```json
{"decision": "approve" | "reject", "reason": "optional free text"}
```

- `approve` — moves the tool from `proposed` to `building`. The build task
  filed with the proposal stays open.
- `reject` — removes the `[[tool]]` entry, deletes its still-`open` build
  tasks (`jeryu-tool/tasks/*.toml`), and ignores the tool's `origin_cluster`
  (with `reason`, default `"proposal rejected"`) so the finder stops
  resurfacing it. The ignore is best effort: the rejection stands even if it
  fails.

Success returns `200` with a receipt:

```json
{
  "tool_id": "example-tool",
  "decision": "reject",
  "status": null,
  "removed_tasks": ["task-id"],
  "decided_by": "admin-login"
}
```

`status` is `"building"` after approve and `null` after reject.

Errors (typed JSON with `code`):

| Status | Code | Cause |
| --- | --- | --- |
| 422 | `tool_proposal_invalid_request` | Body is not the JSON above. |
| 404 | `tool_proposal_not_found` | No tool with that id in the registry. |
| 409 | `tool_proposal_not_proposed` | Tool is not `proposed` (already decided). |
| 424 | `tool_proposal_registry_unavailable` | No registry wired (missing `--split-manifest`) or it does not parse. |
| 424 | `tool_proposal_registry_write_failed` | The registry or a task file could not be written. |

## Scan errors

| Status | Code | Cause |
| --- | --- | --- |
| 409 | `tool_finder_scan_running` | A scan is already in flight. |
| 424 | `tool_finder_not_configured` | No split family on this server; see `GET /api/v1/tool-finder/source`. |
| 424 | `tool_finder_store_unavailable` | The codegraph store could not answer the dashboard. |
| 500 | `tool_finder_source_failed` | Source discovery panicked; retry. |

## Still to do

The Findings page still paints only the scan status. It should ask
`GET /api/v1/tool-finder/source` and paint the `not configured` state and the
`skipped` list, so an operator sees what a scan left out without reading the
WebSocket stream. That is a `jeryu-web` change and lands with its own todo.

