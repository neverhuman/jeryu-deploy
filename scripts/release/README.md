# Releasing the forge

Production is `jeryu serve` on atomicsoul (systemd user unit `jeryu.service`),
reached through xbabe2. Releases are unsigned (owner decision) and built from a
commit of this repository.

```sh
rel=$(scripts/release/stage-release.sh)        # build main, stage on atomicsoul
scripts/release/deploy-release.sh "$rel"       # switch, and record the deployment
```

- **`stage-release.sh [COMMIT]`** builds `jeryu-cli` in the glibc 2.35 builder
  image on xbabe2 with no network (`cargo --locked --offline`, dependencies fetched
  first through `.cargo/hosted-gitconfig`), refuses a binary needing a newer glibc
  than the forge host has, and stages `bundle/jeryu`, the vendored web dist,
  `RELEASE.txt`, `RELEASE.env` (`REL`, and `PREV` read from the live symlink),
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
