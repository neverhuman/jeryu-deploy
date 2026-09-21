# Release

**Run releases on xbabe0 only.** It is the release host: the release scripts
reach xbabe2 (build) and atomicsoul (forge) from there, and the auto-stage and
auto-pin timers run there.

The release procedure is [`scripts/release/README.md`](../scripts/release/README.md):
`stage-release.sh` builds and stages a release, `deploy-release.sh` switches to
it and records the deployment, and the staged `rollback.sh` undoes a switch.

The governed Jankurai binary that release and score lanes use is described in
[`docs/governed-jankurai.md`](governed-jankurai.md).
