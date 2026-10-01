# Releasing the forge: run every release command on xbabe0 only

**Run releases on xbabe0 only.** It is the release host: these scripts reach xbabe2 (build) and
atomicsoul (forge) from there, and the auto-stage and auto-pin timers run there.

Production is `jeryu serve` on atomicsoul (systemd user unit `jeryu.service`),
reached through xbabe2. Releases are unsigned (owner decision) and built from a
commit of this repository.

```sh
rel=$(scripts/release/stage-release.sh)        # build main, stage on atomicsoul
scripts/release/deploy-release.sh "$rel"       # switch, and record the deployment
```

- **`stage-release.sh [COMMIT]`** builds the pinned web dist with
  `build-web-dist.sh` (below), then `jeryu-cli` in the glibc 2.35 builder
  image on xbabe2 with no network (`cargo --locked --offline`, dependencies fetched
  first through `.cargo/hosted-gitconfig`; `JERYU_BUILD_COMMIT=<COMMIT>` passed in, so
  the binary reports its own commit at `/api/v1/version` and as `forge.commit` on
  `/runners`), refuses a binary needing a newer glibc
  than the forge host has, and stages `bundle/jeryu`, the web dist,
  `RELEASE.txt` (with the jeryu-web commit and dist hash), `RELEASE.env` (`REL`,
  and `PREV` read from the live symlink; a live name not in `prod-…-unsigned` form
  is refused unless `--prev <that name>` confirms it),
  `switch.sh`, `rollback.sh` and `SHA256SUMS` in `~/.jeryu/incoming/<release>/` on
  atomicsoul. It changes nothing else there.
- **`deploy-release.sh RELEASE`** records a `production` deployment of
  `jeryu/jeryu-deploy` (Deployments API; needs an admin token, default the
  `alton2` PAT), runs the staged `switch.sh`, and appends `success` or `failure`.
  The previous production deployment is marked `inactive` automatically.
- **`switch.sh`** (on the forge host) refuses unless `PREV` is live, the checksums
  hold and no snapshot exists yet; then stops, snapshots `forge`/`work`/`codegraph`
  with SQLite's backup API, installs, repoints `~/.jeryu/bin/jeryu` and
  `~/.jeryu/share/web-dist`, starts, and proves the running binary is the staged one.
  It polls `JERYU_HEALTH_URL` (default `http://172.19.0.1:8787/health`) once a
  second, `JERYU_HEALTH_TRIES` times (default 30), and fails if it never answers:
  the new release is then live but unhealthy, so run `rollback.sh`.
- **`rollback.sh`** (in `~/.jeryu/releases/<release>/`) restores `PREV` and the
  pre-switch snapshot, keeping the post-switch databases in
  `~/.jeryu/backups/post-<release>-<time>/`.

`test-release-scripts.sh` runs the real `switch.sh` and `rollback.sh` against a
throwaway forge home and is part of `ops/ci/pr-ci.sh`.

Before a release that adds a database migration, run the staged binary against a
backup copy of the databases on a spare loopback port and check the new schema and
routes; `switch.sh` snapshots before starting, and `rollback.sh` restores that
snapshot.

## The web UI

The SPA is not in this repository. `jeryu-split.lock.toml` pins it with one entry:

```toml
web_artifact = "pinned"

[[repo]]
name = "jeryu-web"
commit = "<40-hex jeryu-web commit>"
web_dist_sha256 = "<sha256 of that commit's dist manifest>"
```

- **`build-web-dist.sh [--commit SHA] OUT_ROOT`** checks jeryu-web out at exactly
  the locked commit (refusing a dirty tree or a different HEAD), runs `npm ci` and
  `vite build` in the pinned `node:20.20.1` image (the only networked build step),
  and writes `OUT_ROOT/<commit>/dist` plus `MANIFEST.sha256`: one `sha256sum` line
  per file, sorted by path. The dist hash is the sha256 of that manifest, and the
  script refuses one that differs from the lock.
- The offline binary build passes the dist in as `JERYU_WEB_DIST`.
  `crates/jeryu-api/build.rs` recomputes the manifest hash and fails the build on
  a mismatch, a missing dist, or a release build without one. Tests and dev builds
  embed no SPA (with a cargo warning), or an unverified local dist named explicitly
  by `JERYU_WEB_DIST_LOCAL`, which a release build refuses.

To ship a UI change, merge it to jeryu-web main. **The bump then proposes itself:**
`auto-pin.sh` (timer `jeryu-auto-pin.timer`, every 5 minutes; install once with
`install-auto-pin.sh`, disable with `systemctl --user disable --now jeryu-auto-pin.timer`)
waits for jeryu-web main to be green, builds the dist with this script from jeryu-deploy main,
changes exactly the two lock fields on `auto/pin-web-<sha12>` and opens
`release: pin jeryu-web <sha7>` as alton2. (A head that changes no shipped file, such as a
test-only commit, builds the bundle already pinned: then only `commit` moves and the pull request
says the bundle is unchanged.) It never merges: the reviewer and the merge queue land
it, auto-stage stages main, and a person deploys. It skips a head while any bump is open, retries
a failing head once, then posts `pin.bump_failed` for a human and leaves that head alone. What is
pinned, how far behind it is and whether a bump is open: `GET /api/v1/pins` and the inbox item
`pin_behind` (docs/pipeline-events.md).

Every tick also ends with one runner heartbeat (`<host>/auto-pin`, label `automation`), so
`/runners` shows the timer alive and what it last did: `opened` a bump, `waiting` behind one or
for the web gate, or `failed` on a head. Same token, same `curl --config` file, same origin guard
as the events; a refused beat costs one log line and never the tick. `JERYU_AUTO_PIN_BEAT=0` turns
it off. Contract: [`docs/pipeline-events.md`](../../docs/pipeline-events.md#runner-heartbeats).

The manual fallback changes only the lock entry:

```sh
scripts/release/build-web-dist.sh --commit <jeryu-web sha> /tmp/web-dist   # prints "<sha> <hash>"
# set commit = "<sha>" and web_dist_sha256 = "<hash>" in jeryu-split.lock.toml; land it
```

jeryu-web keeps its build reproducible (same commit, same hash) and checks it
with its `scripts/check-reproducible-web-build.sh`.

## Machine-readable output

`stage-release.sh` and `deploy-release.sh` take `--json` and `--dry-run`. Under `--json` stdout is
exactly one JSON line (everything else goes to stderr): the result on success, or the API's error
envelope `{"code","message","exit_code"}` on failure. `--dry-run` reads what it needs and changes
nothing. Refusals exit with a code per class, named by the envelope's `code`:

| exit | code | meaning |
|---|---|---|
| 64 | `usage` | bad argument or env value |
| 65 | `state` | the live or staged release is not what the script expects |
| 69 | `unreachable` | a host or the remote did not answer |
| 70 | `build` | the staged binary needs a newer glibc (stage) |
| 77 | `credential` | the deploy token is not readable (deploy) |
| 1 | `failed` | anything else |

A failed switch exits with `switch.sh`'s own code (`switch_failed`). Without `--json`, the release id
is still the last line of `stage-release.sh`'s stdout.

## Builder image

`stage-release.sh` builds in `jeryu-builder:rust1.95-glibc2.35-r2`, defined by
`builder.Dockerfile` (Ubuntu 22.04 / glibc 2.35, rustup-init pinned by sha256, Rust 1.95.0). When
the tag is missing on the build host, the script builds it there first. Change the Dockerfile only
together with a new `-rN` tag.

## Auto-staging

`auto-stage.sh` runs every 5 minutes on the release host (xbabe0) from
`jeryu-auto-stage.timer`. It stages the forge's main once its combined status is `success`, using
`stage-release.sh` from that same commit. It never deploys, so a "go" is only
`deploy-release.sh <rel>`. The newest staged id is in `~/.local/state/jeryu-auto-stage/latest`, and
every staged commit is in `staged.tsv`. A commit that fails to stage twice is left for a human.
Install or update the timer with `scripts/release/install-auto-stage.sh`, and follow it with
`journalctl --user -u jeryu-auto-stage`.

Each attempt also posts a pipeline event to the forge (`release.staged` with the deploy command,
or `release.stage_failed` with the tail of the staging output), which is how the attention inbox
knows a release is staged and waiting. Posting is best-effort and never fails a staging. The
contract is in [`docs/pipeline-events.md`](../../docs/pipeline-events.md).

Every tick, idle ones included, ends with one runner heartbeat (`<host>/auto-stage`, label
`automation`, no `pr`), so `/runners` shows the timer alive and whether it last `staged` a commit,
is `waiting` for main's gate, or `failed`. Best-effort like the events; `JERYU_AUTO_STAGE_BEAT=0`
turns it off.
