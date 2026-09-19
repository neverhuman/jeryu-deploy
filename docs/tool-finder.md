# Tool-finder operations

The system-wide tool-finder scans every split repository for duplicated code,
groups it into clusters (Shared tools → Findings, `/tools`), and lets an admin
propose a cluster as a shared tool in `jeryu-tool/tools-registry.toml`. This
page covers the two operator-facing pieces: the scheduled scan and the
proposal decision API. Code: `crates/jeryu-api/src/web/tool_finder.rs`,
`tool_finder_schedule.rs`, and `tool_proposals.rs`.

All `/api/v1/tool-finder/` routes are admin-only (`admin_only_path` in
`crates/jeryu-api/src/web/auth.rs`).

## Scheduled system scan

The web server re-runs the system scan on a schedule so findings stay current
without anyone pressing "Run live scan".

| Setting | Meaning |
| --- | --- |
| `JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS` unset or empty | Scan every 24 hours (default). |
| `JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS=N` (`N > 0`) | Scan every `N` hours. |
| `JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS=0` | Scheduler disabled; scans run only on demand. |

A value that is not a non-negative integer also disables the scheduler. The
scheduler only starts when the server was given split manifests
(`--split-manifest`); without them there is nothing to scan.

The 24-hour default is deliberate: any server with split manifests runs a
system scan daily unless an operator opts out with `=0`. At startup
`jeryu serve` logs one line to stderr with the effective setting, e.g.

    tool-finder: scheduled scan every 24h (default; set JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS=0 to disable)
    tool-finder: scheduled scan disabled (JERYU_TOOL_FINDER_SCAN_INTERVAL_HOURS=0)
    tool-finder: scheduled scan disabled (no split manifests configured)

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
