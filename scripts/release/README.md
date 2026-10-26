# Releasing the forge: run every release command on the release host

**Run releases on the release host only.** These scripts reach the build host
(`JERYU_BUILD_HOST`) and the forge host (`JERYU_FORGE_HOST`) from there, and the
auto-stage and auto-pin timers run there. Which machines those are is site
configuration, not part of this repository.

Production is `jeryu serve` on the forge host (systemd user unit `jeryu.service`),
reached through the build host. Releases are unsigned and built from a
commit of this repository.

```sh
rel=$(scripts/release/stage-release.sh)        # build main, stage on the forge host
scripts/release/deploy-release.sh "$rel"       # switch, and record the deployment
```

- **`stage-release.sh [COMMIT]`** builds the pinned web dist with
  `build-web-dist.sh` (below), then `jeryu-cli` in the glibc 2.35 builder
  image on the build host with no network (`cargo --locked --offline`, dependencies fetched
  first through `.cargo/hosted-gitconfig`; `JERYU_BUILD_COMMIT=<COMMIT>` passed in, so
  the binary reports its own commit at `/api/v1/version` and as `forge.commit` on
  `/runners`), refuses a binary needing a newer glibc
  than the forge host has, and stages `bundle/jeryu`, the web dist,
  `RELEASE.txt` (with the jeryu-web commit and dist hash), `RELEASE.env` (`REL`,
  and `PREV` read from the live symlink, for the record only: `switch.sh` replaces
  whatever is live when it runs; a live name not in `prod-…-unsigned` form
  is refused unless `--prev <that name>` confirms it),
  `switch.sh`, `rollback.sh`, `install-forge-unit.sh`, the `jeryu.service.in` it
  renders and `SHA256SUMS` in `~/.jeryu/incoming/<release>/` on
  the forge host. It changes nothing else there.
- **`deploy-release.sh RELEASE`** records a `production` deployment of
  `jeryu/jeryu-deploy` (Deployments API; needs an admin token, read from
  `JERYU_DEPLOY_TOKEN_FILE`), runs the staged `switch.sh`, and appends `success` or `failure`.
  The previous production deployment is marked `inactive` automatically.
  It refuses an older build over a newer one: the live release's commit (from
  its `RELEASE.txt`) must be an ancestor of the staged one, or it exits 65 and
  records nothing. `--allow-downgrade` is for a deliberate roll back by
  redeploying. Any newer release may be live, so deploying release A and then
  B, both staged before A went live, needs no re-staging.
- **`switch.sh`** (on the forge host) refuses unless the live symlink names an
  installed binary, the checksums hold and no snapshot exists yet; it records the
  live release as the rollback target in `~/.jeryu/releases/<release>/ROLLBACK.env`
  (whatever `PREV` staging saw); then stops, snapshots `forge`/`work`/`codegraph`/`shift`
  with SQLite's backup API, installs, repoints `~/.jeryu/bin/jeryu` and
  `~/.jeryu/share/web-dist`, starts, and proves the running binary is the staged one.
  It polls `JERYU_HEALTH_URL` (a required site setting, the forge's health
  endpoint) once a
  second, `JERYU_HEALTH_TRIES` times (default 30), and fails if it never answers:
  the new release is then live but unhealthy, so run `rollback.sh`.
- **`install-forge-unit.sh`** (on the forge host, run by `switch.sh` before it
  stops anything) renders `systemd/jeryu.service.in` into
  `~/.config/systemd/user/jeryu.service` and reloads the user manager, so every
  release also refreshes the forge's limits. See "What the forge may cost the
  host" below.
- **`rollback.sh`** (in `~/.jeryu/releases/<release>/`) restores the release
  `switch.sh` replaced (`ROLLBACK.env`, else `RELEASE.env`'s `PREV`) and the
  pre-switch snapshot, keeping the post-switch databases in
  `~/.jeryu/backups/post-<release>-<time>/`.

`test-release-scripts.sh` runs the real `switch.sh`, `install-forge-unit.sh` and `rollback.sh` against a
throwaway forge home, and the real timers (`auto-stage.sh`, `auto-pin.sh`, `auto-tag.sh`,
`bump-hosted-pin.sh`) against local bare repositories and a stand-in forge. It is part of
`ops/ci/pr-ci.sh`.

Before a release that adds a database migration, run the staged binary against a
backup copy of the databases on a spare loopback port and check the new schema and
routes; `switch.sh` snapshots before starting, and `rollback.sh` restores that
snapshot.

## What the forge may cost the host

`jeryu.service` is versioned here as `systemd/jeryu.service.in` and installed by
every release. Its shape (`Type=simple`, `Restart=on-failure`, `NoNewPrivileges`,
`PrivateTmp`, `ExecStart=%h/.jeryu/bin/jeryu serve`, the `EnvironmentFile`) is the
one the forge has always run; what it adds is a ceiling, so the forge cannot take
the host with it and leave the kernel nothing to kill.

The numbers are the site's, never this repository's: the template carries
placeholders, and `install-forge-unit.sh` fills them from the site limits file
(`JERYU_FORGE_LIMITS_ENV`, default `~/.config/jeryu/forge-limits.env`). A site
that has not set them is refused by name, with nothing installed and — because
`switch.sh` installs the unit *before* it stops anything — production still up.

| limits file key | becomes | bounds |
|---|---|---|
| `JERYU_FORGE_MEMORY_HIGH` | `MemoryHigh` | where the forge starts being throttled and reclaimed; must be below `MemoryMax` |
| `JERYU_FORGE_MEMORY_MAX` | `MemoryMax` | where the kernel kills the forge instead of the host dying |
| `JERYU_FORGE_TASKS_MAX` | `TasksMax` | processes and threads the forge may hold at once |
| `JERYU_FORGE_OOM_SCORE_ADJ` | `OOMScoreAdjust` | the forge's bias when the host as a whole runs out |

Budget the host for `MemoryMax`, not for an idle forge: a release, a clone and a
migration all land inside the same cgroup.

**Applying it the first time.** The owner writes the limits file on the forge
host once — it is site configuration and stays outside this repository:

```sh
# on the forge host, as the user that runs jeryu.service
mkdir -p ~/.config/jeryu
cat > ~/.config/jeryu/forge-limits.env <<'EOF'
JERYU_FORGE_MEMORY_HIGH=<size>
JERYU_FORGE_MEMORY_MAX=<size>
JERYU_FORGE_TASKS_MAX=<count>
JERYU_FORGE_OOM_SCORE_ADJ=<-1000..1000>
EOF
```

The next `deploy-release.sh` then installs the unit on its own: its `switch.sh`
runs `install-forge-unit.sh` first, which prints the limits it installed, and the
forge comes back up inside them. Without that file the deploy is refused before
anything stops, so write it before the next release. Check what the running forge
got with `systemctl --user show jeryu.service -p MemoryMax -p MemoryHigh -p TasksMax`.

## A site's own environment and drop-ins

`jeryu.service` belongs to the release: `install-forge-unit.sh` writes
`~/.config/systemd/user/jeryu.service` from the template on every switch and
**replaces whatever is there**, so a hand-written unit — and any `EnvironmentFile=`
line a site added to it — is lost at the next release. A site keeps its own
settings in a drop-in instead, which systemd merges on top of the installed unit
and no release touches:

```sh
# on the forge host, as the user that runs jeryu.service
mkdir -p ~/.config/systemd/user/jeryu.service.d ~/.config/jeryu
cat > ~/.config/jeryu/site.env <<'EOF'
JERYU_SERVE_BIND=<address:port>
EOF
cat > ~/.config/systemd/user/jeryu.service.d/10-site.conf <<'EOF'
[Service]
EnvironmentFile=%h/.config/jeryu/site.env
EOF
systemctl --user daemon-reload
```

A drop-in that also rebuilds `ExecStart` from those variables has to define every
one of them. `install-forge-unit.sh` collects the `${VAR}`/`$VAR` references on
the `ExecStart*` lines of the installed unit and of every
`jeryu.service.d/*.conf`, resolves them against all the `Environment=` and
`EnvironmentFile=` sources those files name (following the drop-ins' own
`EnvironmentFile=` lines; a `-` prefixed file may be missing), and refuses by
name if any variable is defined nowhere — before `switch.sh` stops anything, so
production stays up. Without that check the forge comes back with an empty
argument and crash-loops with the previous release already gone.

The installer also requires a nonempty `JERYU_MIRROR_REMOTE_ALLOWLIST` in the
service environment, for example `github.com/example-org`. Configure it in a
service environment file or `Environment=` drop-in; setting it only in the
deploy caller's shell does not configure the service. Later environment files
can override earlier values, and an empty value is refused before production
stops. Sites that intentionally run without mirror pushes must explicitly set
`JERYU_GITHUB_PUSH=0` in the service environment instead.

A unit or environment file the preflight cannot read, or a line it cannot
parse, is refused by name with that reason -- also before production stops --
rather than failing as a crash the operator has to decode. A word in
`Environment=` that assigns nothing is ignored, as systemd ignores it.

To change a limit without a release, edit the limits file and run
`scripts/release/install-forge-unit.sh` on the forge host, then
`systemctl --user restart jeryu.service`. `install-forge-unit.sh --help` lists
every key.

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
`release: pin jeryu-web <sha7>` as the configured bump identity (`JERYU_PIN_GIT_NAME`). (A head that changes no shipped file, such as a
test-only commit, builds the bundle already pinned: then only `commit` moves and the pull request
says the bundle is unchanged.) It never merges: the reviewer and the merge queue land
it, auto-stage stages main, and a person deploys. It skips a head while a bump is open and
holding, but not a stale one: a bump recorded in `opened.tsv` that pins an older
ancestor of green web main may be replaced when it is not queued. An effective
approval keeps it open unless its exact-head required checks have all finished
with at least one failure. Green, pending, missing or unreadable required results,
and advisory-only failures, preserve that approval. The replacement starts with
fresh review and required checks; approval is never carried forward.
A changes-requested review keeps it
open for `JERYU_AUTO_PIN_SUPERSEDE_AFTER`
(default `6h`; `90m` or seconds also work). The replacement is built and opened
before the old bump closes; a build or create failure leaves the old review open.
Only a successful close gets a superseding comment and `pin.bump_superseded`.
A refused close emits a human-cleanup event without posting repeated comments.
If the old bump closes, receives approval without a finished failing required
gate, recovers its required gate or joins the queue during the build,
the pre-close check leaves it alone and emits a cleanup event for the replacement.
Multiple existing bumps, an unrecorded bump or a diverged pin stop replacement.
A second list before PR creation catches other bumps opened during the build.
When the current head's own bump (recorded in `opened.tsv`, the only open web pin bump, not
queued, no requested changes) has a finished failing exact-head required gate and deploy main has
moved past its base, a tick merges main into it with a normal push, so the pull request keeps its
number. The refreshed tree is main plus the two pin fields; when main changed
`build-web-dist.sh` since the bump was built, the bundle is rebuilt with main's recipe and that
hash is pinned. A bump is refreshed at most `JERYU_AUTO_PIN_MAX_REFRESHES` times (default 3) and
no more often than `JERYU_AUTO_PIN_REFRESH_AFTER` (default `1h`); after that a gate that still
fails is the bump's own, and `pin.bump_failed` asks for a person.
It retries
a failing head once, then posts `pin.bump_failed` for a human and leaves that head alone. What is
pinned, how far behind it is and whether a bump is open: `GET /api/v1/pins` and the inbox item
`pin_behind` (docs/pipeline-events.md).

Every tick that resolves both mains, idle or not, first deletes the `auto/pin-web-*` branches
whose pull request is closed, landed or superseded by a newer bump. Only a pull request of this
repository counts (not a fork's), and a branch goes only while it is still that pull request's head;
the current web head's branch, any branch an open pull request uses and one that moved stay. The
commit is read again just before the delete, which narrows a race with a new push but cannot close
it: the atomic `--force-with-lease` is a force mutation the audit refuses. The pull requests are
listed only when the set of bump branches changed since the last clean pass. A refused delete logs
git's reason and is tried again next tick; it never fails the tick.

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

## Tagging hosted crates

`auto-tag.sh` runs on the release host from `jeryu-auto-tag.timer`, beside auto-pin and
auto-stage, and never on a gate host: pull request code runs there and must not be near this
token. Crate tags used to be cut in batches, so merged crate work waited a median of 108 hours for
its first tag while server work reached production 2.5 hours after merge.

Per repository, in the dependency order of `hosted-crates.tsv` (which must name exactly the
repositories `.cargo/hosted-pin-refs.tsv` pins):

1. the next tag is `split.<n+1>` after the newest `<repo>-v<x.y.z>-split.<n>` tag the forge
   advertises; a new version line is a person's release, and a repository with no split tag is left
   alone;
2. a newest tag whose `refs/heads/preserve/hosted-cargo/<tag>` branch is missing or moved is
   repaired first. That branch is what keeps a tagged commit fetchable for cargo, and
   `ops/ci/dependency-sources.sh` reads it back, so a tag without it breaks the consumer's gate;
3. nothing is cut when main is already the tagged commit, when main does not descend from it
   (`tag.diverged` asks for a person), when every commit since the tag only moved documentation or
   was a Jeryu Split Bot re-render, when the newest tag is younger than
   `JERYU_AUTO_TAG_COALESCE` (1h: at most one tag per repository per hour), or when the forge's
   latest `<repo>/required` status on main's head is not `success`;
4. the annotated tag and its preserve branch are pushed in one atomic push, so neither can exist
   without the other, and `tagged.tsv` records the tag only after the forge has it;
5. there is no VERSION stamp commit. The version is the tag; a stamp would add an ungated commit to
   main for every tag.

The decision is made in the timer, from the forge's answers, never in the tree being tagged.

Then one bump stack: for every repository whose newest tag is ahead of the pin in jeryu-deploy
main, `release: pin <repo> <tag>` from `auto/pin-<repo>-<n>-<sha12>`, in dependency order and each
based on the previous branch, so the merge queue cannot land a mismatched pair. A stack is built
only when none of its pull requests is already open. The edit itself is `bump-hosted-pin.sh` taken
from jeryu-deploy main (`git archive`), so the recipe is always the reviewed one; it moves the
`tag =` lines whose `git` URL names the repository, the `Cargo.lock` git source identity and the
`.cargo/hosted-pin-refs.tsv` row, reads every edit back, and refuses anything else — including a
`-split.0` pin, which is also what `[patch]` redirects. `jeryu-split.lock.toml` is not a cargo pin
and is not touched. auto-tag only proposes: review, pr-redteam and the merge queue land the bump,
and deploying stays a person's release.

```sh
scripts/release/auto-tag.sh --dry-run        # what it would tag and bump; writes nothing
scripts/release/install-auto-tag.sh          # install and enable the timer on the release host
cd <a jeryu-deploy checkout> && scripts/release/bump-hosted-pin.sh <repo> <tag> <commit>
```

Settings are an installation's, so none has a default: `JERYU_TAG_TOKEN_FILE` (releasebot's agent
token, a regular mode-0600 file), `JERYU_TAG_GIT_NAME` and `JERYU_TAG_GIT_EMAIL`. The unit reads
them from `~/.config/jeryu/auto-tag.env`. Each tick ends with one runner heartbeat
(`<host>/auto-tag`, label `automation`) and posts `tag.*` and `pin.bump_opened` events; both are
best-effort and never fail a tick, and a dry run sends neither. Contract:
[`docs/pipeline-events.md`](../../docs/pipeline-events.md).

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

`auto-stage.sh` runs every 5 minutes on the release host from
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

## Deploying from the forge (release actions)

`release-action.sh` is the release host's side of the attention inbox's **Deploy** and **Run
auto-pin now** buttons, and also the one-line way to run those steps by hand from any directory:

```sh
~/.local/share/jeryu-release-actions/release-action.sh deploy <release> [--dry-run]
~/.local/share/jeryu-release-actions/release-action.sh auto-pin
```

It fills unset settings from `~/.config/jeryu/release.env` itself, takes `deploy-release.sh` from
the immutable staged commit after verifying its ancestry on protected main, holds a host deploy lock,
and waits for auto-stage's lock (keeping history for the downgrade check),
so no shell needs `cd`, `set -a; . release.env` or a check that auto-stage is idle.
Install it with `scripts/release/install-release-actions.sh`: that also enables
`jeryu-release-actions.timer`, which every 30 seconds sends an authenticated repository poller check-in
and runs one step a deployer asked for. The forge offers the buttons only while that check-in is
fresh, and auto-stage names the installed script in its "deploy with" line once it exists. The
forge never sends a command, only one of three named steps, which this script checks again. How the
asks work is in [`docs/release-actions.md`](../../docs/release-actions.md).
