# Release boards (`/releases`)

One board per product family: every **deliverable** the family ships, each as a lane of
**stages** in order, each stage with the **targets** it runs on, what promoting into it would ship,
and how much of the family's todo queue has actually reached production. `/releases` renders it;
`/releases?repo=owner/name` still shows the single-repository view.

Why a board and not the forge's own data: much of what a release is cannot be seen from the forge.
Deploy hosts, image registry channels, staged bundles, public download manifests and todo queues
live elsewhere, so a **collector** on the host that runs releases reads them and pushes one
snapshot per family.

## Configuration is yours, not jeryu's

jeryu ships the API, the page, the collector (`scripts/release-board/collect.sh`) and its library
(`lib.sh`). What a board reads is site configuration and never lives in this repository:

| Where | What |
|---|---|
| `~/.config/jeryu/release-board.env` | `JERYU_BASE` (the forge's URL, required), `JERYU_BOARD_TOKEN_FILE` (default `~/.config/jeryu/release-board.token`), and any secret an adapter needs. The collector reads it itself for any setting the caller left unset, so a refresh started by a release script needs no setup; `JERYU_RELEASE_BOARD_ENV` points elsewhere. |
| `~/.config/jeryu/release-board/families/<family>.sh` | one **adapter** per family: which repositories, hosts, paths and URLs make up its deliverables and stages. `scripts/release-board/examples/acme.sh` is an invented one to copy. |

The token belongs to a forge admin or a login named in `JERYU_BOARD_REPORTERS` on the forge.
Keep the env file and the token mode 600.

## Freshness

- The collector runs every **5 minutes** (`jeryu-release-board.timer`).
- Release scripts run it **at once** when they finish (`--trigger release`). jeryu's own
  `scripts/release/deploy-release.sh` refreshes every family, because a restarted forge holds no
  boards; a site adds the same call to its own release scripts. That refresh waits for the
  switched forge's `/health` before it pushes (a push sent to a forge that is still starting is
  lost), keeps its output in `~/.local/state/jeryu-release-board/logs/refresh-<release>-<UTC
  stamp>.log` (0600, newest 30) and ends in a receipt line saying whether it landed, so an empty
  `/releases` after a deploy is visible in the deploy's own output.
- Stages that report deployments to the forge (a `forge` binding) are also read **live** by the
  page from `/api/v3/repos/{repo}/environments`, so a release that reports itself shows before any
  snapshot, marked "reported after this snapshot".
- A successful PUT publishes `release_board.updated` on the `pipeline` websocket scope, so open
  pages refetch within seconds. It is not written to the event log.
- Snapshots are kept in memory, like runner heartbeats. After a forge restart the board is empty
  until the next collector run.

## API (`crates/jeryu-api/src/web/release_board.rs`)

| Method | Path | Who | Answer |
|---|---|---|---|
| PUT | `/api/v1/release-board/{family}` | a `JERYU_BOARD_REPORTERS` login (a site setting) or any admin | `200 {family, observed_at, accepted_at}`; `ignored: "older than stored snapshot"` when a later snapshot is already held; `403 permission_denied`; `422 invalid_input` naming the field; `413` over 512 KiB |
| GET | `/api/v1/release-board` | admin | `{boards: [{family, observed_at, accepted_at, summary, collector, problem_count}]}` |
| GET | `/api/v1/release-board/{family}` | admin | the snapshot plus `accepted_at`; `404 not_found` |

Reads are admin-only because a board names hosts, commands and pinned commits of private
repositories. Limits: 1–32 lanes, 0–8 columns, 1–16 stages per lane, 32 targets per stage, 50 `ships` lines,
64 problems, 200 pin rows, every string at most 2000 characters, `observed_at` at most five
minutes ahead of the forge's clock.

## Shape: `jeryu.release_board.v1`

`docs/release-board.example.json` is a complete, invented example (family `acme`). In short:

- `family`, `observed_at` (RFC 3339), `summary`, `collector {host, version, trigger, duration_ms}`.
- `columns[]?`: `{id, name}`, at most 8: the family's fixed stages left to right, such as main,
  dev, stage, prod. When present every lane is drawn on one grid under a single header row,
  a stage sits in the column its `column` names (several may share one, e.g. shift work beside
  main), a column a lane skips shows as "not used", and a stage with no `column` is listed after
  the grid. Without `columns` each lane is its own track of stages, joined by → and ‖.
- `lanes[]`: `{id, name, source, owner_family, group?, read_only?, stages[]}`. A lane owned by
  another family and shown on this one is `read_only`. Neighbouring lanes with the same `group`
  sit together under its name: give each separately installed tool its own lane and one group,
  rather than one lane whose "stages" are different tools.
- `stages[]`: `{id, name, version, state, status, known_by, column?, parallel?, never_deployed?,
  targets[], promote?, ships?, rollback?, forge?}`. `column` must be one of the board's
  `columns`.
  - `state` is `ok | warn | bad | none`; a stage is only `ok` when every target is.
  - `targets[]`: `{name, running, state, runners?}`. `running` is what the target runs, or null.
    `runners` names the forge runners that serve the target, each exactly as `runnerId` on
    `GET /api/v1/control-plane/runners` (e.g. `gate-a/slot0`), so `/releases` and `/runners` can
    link to each other. At most 64 per target, each 1–200 characters with no control
    characters and no duplicates; an empty list is omitted.
  - `known_by` says how the collector knows: `reported` (a forge deployment), `host` (read from
    the machine or service), `derived` (computed from git), `unverified`.
  - `parallel` marks a stage that runs beside the previous one instead of after it;
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
diff reverse-applies cleanly to the stage's tree), so work that reached main by a re-landed or
reset tree still counts. The todo's own merged flag and recorded shas are not used; landing
rebases them. The queue is listed with `$JERYU_BOARD_QUEUE_CMD list <family> --all` (default
`todoq`).

## Writing an adapter

Copy `scripts/release-board/examples/acme.sh` to your families directory and replace its values.
An adapter defines `collect_<family>` (dashes become underscores) and uses `lib.sh`:

- `mirror owner/repo` — a bare mirror of a forge repository, fetched once per run.
- `stage key=value…` / `lane id name source owner read_only [group=name] stage…` /
  `target name running state [runners]` — the board's pieces (`runners` is an optional JSON
  array of runner ids; empty or absent leaves the key out); `board_columns id=name…` declares the grid
  and `stage … column=id` places a stage on it.
- `behind`, `contains`, `worst`, `worst_of`, `short`, `lines_json` — comparisons and formatting.
- `work_summary family method unlinked mirror=tip…` — the work bar; set `main_specs` first.
- `problem source message` — record anything unreadable and keep going.

An adapter must only read. A source whose "read" changes state (for example a URL that hands
out a resource on GET) must not be called; read a health or status endpoint instead.

## Commands

```sh
scripts/release-board/collect.sh all                            # write boards, push nothing
scripts/release-board/collect.sh --push --trigger manual acme   # push one family now
scripts/release-board/install-release-board.sh                  # install + enable the timer
bash scripts/release-board/test-release-board.sh                # the gate's test
```

Boards are written to `~/.local/state/jeryu-release-board/boards/<family>.json` whether or not
they are pushed; git mirrors of every repository an adapter reads are kept beside them.
