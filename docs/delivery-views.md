# Lineage and build candidates

`/lineage` follows the shell's family scope. Repositories with open pull requests
appear first, with the first three ancestry views open; the checkbox includes
repositories whose recent branches have no pull request. A repository has its
own Lineage tab. Source commits in Releases → Builds link back into it.

`GET /api/v1/repos/{id}/lineage` uses the normal repository read authorization.
The result has `repository`, `default_branch`, `default_sha`, `observed_at`,
`branches`, and `commits` (SHA, actual parent SHAs, subject, author, commit time).
Active means an open local-source pull request or an uncontained branch touched
in the last 14 days. The response admits at most 24 active branches, examines
200 recent branch refs, and draws at most 300 commits in topological order.
`truncated`, `missing_parents`, and `shallow` describe incomplete ancestry.
No edge is inferred from a pull request base, date, or predicted merge.
Each Git subprocess has a 3-second budget and the whole read has 15 seconds;
child processes are killed on cancellation. Each Git read is capped at 2 MiB. No Git writes occur.

Releases → Builds has one deliverable selector and one row per candidate.
The newest build time sorts first, including rejected, failed and never-deployed
builds. The default history ends after the build that production's current
release replaced. This is deployment history, not the preceding version number
or adjacent registry tag. During a rollout every reported production target
contributes a boundary. Older history is available on demand; paging never
implies that the previous-production boundary was found. Expanding a row shows
all reported repositories, immutable SHA-256 digests, tags, targets, source and
build log. More than 15 minutes since the environment observation is explicitly
unconfirmed. A tag alone never establishes where an image is running.

## Reporting builds

The normal release-board reporter identity (`JERYU_BOARD_REPORTERS` or admin)
may PUT `/api/v1/build-inventory/{family}`. Reads require an admin, matching the
release board's exposure of private repository and host information. A body is
at most 1 MiB and 500 builds; batches may accumulate history. The report is:

```json
{
  "observed_at": "2026-10-30T12:00:00Z",
  "builds": [{
    "id": "cloud-rc-21",
    "deliverable": "Cloud app",
    "built_at": "2026-10-30T11:00:00Z",
    "source_repo": "acme/app",
    "source_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "status": "built",
    "artifacts": [{
      "repository": "image.example/acme/app",
      "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "tags": ["cloud-rc-21", "dev"]
    }],
    "log_url": "https://build.example/runs/21"
  }],
  "placements": [{
    "deliverable": "Cloud app",
    "environment": "dev",
    "target": "app-01",
    "current": "cloud-rc-21",
    "previous": null,
    "production": false
  }]
}
```

A family adapter may call `build_inventory /path/to/report.json`; the existing
collector pushes it alongside the board. Keep the report outside the source
checkout and read build receipts, image manifests and running target identities
in the adapter. On a source read failure omit `placements`; do not send an empty
list, which means a complete observation with no configured targets. No existing
adapter is silently reinterpreted as a build producer. Site adapters supply the
actual registries/receipt paths; source branches alone do not prove a build.

`status` is `building`, `built`, `failed`, or `rejected`. A candidate may initially
have no artifacts and acquire them when its build finishes. Its id, deliverable,
build time, source and subsequently acknowledged content digests are immutable;
a rebuilt image needs a new build id. Tags, mirror locations and status can be
refreshed by a later report. Each report is atomic. A bad placement or changed
identity rolls back the batch. Repeating a report does not duplicate builds;
older reports cannot replace a newer placement snapshot. With no placements key,
builds are retained without changing any environment observation.

`GET /api/v1/build-inventory/{family}?deliverable=Cloud%20app&offset=0` returns
`deliverables`, the selected `deliverable`, `builds` (each with `observed_at`),
`placements`, `placements_observed_at`, `total`, and `next_offset`. A page has at
most 100 builds, sorted by build instant then id. Builds and the most recent
complete placement observation persist in `shift.sqlite` under migration 0029.
There is no automatic deletion: source retention can remove registry artifacts,
but the acknowledged build record remains.

Before shipping migration 0029, use the documented shift-store backup and restore
rehearsal. It adds empty tables and an index, has no backfill and changes no
existing rows. A prior binary ignores the added tables; retain them during a
rollback. `bash scripts/release-board/test-build-inventory.sh` checks the optional collector
boundary without a service or credentials.

The forward migration is checksum-bound by the existing migration
runner. The restart, atomicity and delayed-report tests verify the data boundary.
