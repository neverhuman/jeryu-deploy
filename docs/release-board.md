# Release boards (`/releases`)

One board per product family: every **deliverable** the family ships, each as a lane of
**stages** in order, each stage with the **targets** it runs on, what promoting into it would ship,
and how much of the family's todo queue has actually reached production. `/releases` renders it;
`/releases?repo=owner/name` still shows the single-repository view.

Why a board and not the forge's own data: most of what a release is cannot be seen from the forge.
Fleet nodes, image registry channels, staged bundles, public download manifests and todo queues all
live on other hosts, so a **collector** on the release host (xbabe0) reads them and pushes one
snapshot per family.

## Freshness

- The collector runs every **5 minutes** (`jeryu-release-board.timer`).
- Release scripts run it **at once** when they finish (`--trigger release`):
  `scripts/release/deploy-release.sh` refreshes every family, because a restarted forge holds no
  boards. veox-ai's `deploy.sh`, `fleet-roll.sh` and `promote-cloud-appliance.sh` refresh
  veox-ai.
- Stages that report deployments to the forge (`forge` binding) are also read **live** by the page
  from `/api/v3/repos/{repo}/environments`, so a release that reports itself shows before any
  snapshot, marked "reported after this snapshot".
- A successful PUT publishes `release_board.updated` on the `pipeline` websocket scope, so open
  pages refetch within seconds. It is not written to the event log.
- Snapshots are kept in memory, like runner heartbeats. After a forge restart the board is empty
  until the next collector run.

## API (`crates/jeryu-api/src/web/release_board.rs`)

| Method | Path | Who | Answer |
|---|---|---|---|
| PUT | `/api/v1/release-board/{family}` | a `JERYU_BOARD_REPORTERS` login (default `gatebot,pragent`) or any admin | `200 {family, observed_at, accepted_at}`; `ignored: "older than stored snapshot"` when a later snapshot is already held; `403 permission_denied`; `422 invalid_input` naming the field; `413` over 512 KiB |
| GET | `/api/v1/release-board` | admin | `{boards: [{family, observed_at, accepted_at, summary, collector, problem_count}]}` |
| GET | `/api/v1/release-board/{family}` | admin | the snapshot plus `accepted_at`; `404 not_found` |

Reads are admin-only because a board names hosts, commands and pinned commits of private
repositories. Limits: 1–32 lanes, 1–16 stages per lane, 32 targets per stage, 50 `ships` lines,
64 problems, 200 pin rows, every string at most 2000 characters, `observed_at` at most five
minutes ahead of the forge's clock.

## Shape: `jeryu.release_board.v1`

`docs/release-board.example.json` is a complete example (veox-ai, 2026-09-28). In short:

- `family`, `observed_at` (RFC 3339), `summary`, `collector {host, version, trigger, duration_ms}`.
- `lanes[]`: `{id, name, source, owner_family, read_only?, stages[]}`. A lane owned by another
  family (the cloud appliance on jain's board) is `read_only`.
- `stages[]`: `{id, name, version, state, status, known_by, parallel?, never_deployed?, targets[],
  promote?, ships?, rollback?, forge?}`.
  - `state` is `ok | warn | bad | none`; a stage is only `ok` when every target is.
  - `known_by` says how the collector knows: `reported` (a forge deployment), `host` (read from
    the machine or service), `derived` (computed from git), `unverified`.
  - `parallel` marks a stage that runs beside the previous one (veox-ai's stage beside dev);
    `never_deployed` a stage that is declared but has never been deployed.
  - `promote {command, human_only, automatic}` is shown, never run, by the page.
  - `forge {repo, environment}` is the live overlay binding.
- `work {total, method, parts[{key, label, count}], unlinked?}`: every todo in the family queue
  placed at the furthest point it reached — `live`, `merged` (on main, not released), `stranded`
  (done but only on a branch that never reached main), `untraceable` (done, no commit carries
  its trailer), `blocked`, `open`.
- `pins {note, columns, rows[{repo, cells, behind, note?}]}` and `notes {title, items, coverage}`.
- `problems[{source, message}]`: sources this run could not read.

## How todos are matched

A todo's commits are the commits carrying its `Todo:` trailer, on any branch. It has reached a
stage when that stage's commit holds every one of them — by history, or by content (the commit's
diff reverse-applies cleanly to the stage's tree). The content test matters: veox-ai's forge main
was reset by tree, so history alone finds none of its earlier work. The todo's own `merged` flag
and recorded shas are not used; landing rebases them.

## The collector (`scripts/release-board/`)

```sh
scripts/release-board/collect.sh all                            # write boards, push nothing
scripts/release-board/collect.sh --push --trigger manual jeryu  # push one family now
scripts/release-board/install-release-board.sh                  # install + enable the timer (xbabe0)
bash scripts/release-board/test-release-board.sh                # the gate's test
```

- Boards are written to `~/.local/state/jeryu-release-board/boards/<family>.json` whether or not
  they are pushed; git mirrors of every repo it reads are kept beside them.
- Token: `JERYU_BOARD_TOKEN_FILE` (default the alton2 PAT used by auto-stage and auto-pin).
- Optional secrets go in `~/.config/jeryu/release-board.env`: `CLOUDFLARE_API_TOKEN` and
  `CLOUDFLARE_ACCOUNT_ID`, without which veox-ai's website production stage is `unverified`.
- One adapter per family in `families/<family>.sh`; each read is best-effort and becomes a
  `problems` line when it fails.
- **Never request `www.neverhuman.org/try-free`**: a GET there leases a real Try slot for ten
  minutes. The Try pool is read from `/healthz` only; the test fails on any adapter line that
  requests `/try-free`.

## Known gaps

- jain's Free publish runs on xbabe2 and does not trigger a refresh; the timer picks it up.
- Rollbacks on atomicsoul are not recorded on the forge, so the jeryu board learns of one only
  from the next deployment record.
