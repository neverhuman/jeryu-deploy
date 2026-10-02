# Release

**Run releases on the release host only.** The release scripts reach the build
host (`JERYU_BUILD_HOST`) and the forge host (`JERYU_FORGE_HOST`) from there, and
the auto-stage and auto-pin timers run there.

The release procedure is [`scripts/release/README.md`](../scripts/release/README.md):
`stage-release.sh` builds and stages a release, `deploy-release.sh` switches to
it and records the deployment, and the staged `rollback.sh` undoes a switch.

The governed Jankurai binary that release and score lanes use is described in
[`docs/governed-jankurai.md`](governed-jankurai.md).

## Hosted forge releases and Git tags

Jeryu does not yet store hosted release resources or uploaded release assets.
Authenticated, repository-authorized `POST /repos/{owner}/{repo}/releases`
returns `501 Not Implemented`; it creates neither a release nor a Git tag.
The compatibility list is empty. Git tags can be pushed and fetched through
Git independently and do not imply a hosted release resource. Durable release
resources, assets and lifecycle operations remain in the parity program.
