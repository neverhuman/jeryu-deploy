# Releasing the forge

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
  first through `.cargo/hosted-gitconfig`), refuses a binary needing a newer glibc
  than the forge host has, and stages `bundle/jeryu`, the web dist,
  `RELEASE.txt` (with the jeryu-web commit and dist hash), `RELEASE.env` (`REL`,
  and `PREV` read from the live symlink),
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

To ship a UI change, merge it to jeryu-web main, then change only the lock entry:

```sh
scripts/release/build-web-dist.sh --commit <jeryu-web sha> /tmp/web-dist   # prints "<sha> <hash>"
# set commit = "<sha>" and web_dist_sha256 = "<hash>" in jeryu-split.lock.toml; land it
```

jeryu-web keeps its build reproducible (same commit, same hash) and checks it
with its `scripts/check-reproducible-web-build.sh`.

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
