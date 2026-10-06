#!/usr/bin/env bash
# stage-release.sh [COMMIT] — build jeryu-deploy at COMMIT (default: the forge's
# current main) and stage it on the forge host, ready for deploy-release.sh.
#
#   1. On the build host: a dedicated checkout at COMMIT; build-web-dist.sh
#      builds the jeryu-web commit pinned in that checkout's
#      jeryu-split.lock.toml (pinned node image, network allowed) into
#      ~/jeryu-release-build/web-dist/<web commit>/dist and refuses a hash
#      that differs from the lock; `cargo fetch --locked` through the hosted
#      transport overlay; then `cargo build --release --locked --offline` in the
#      glibc 2.35 builder image with no network and JERYU_WEB_DIST mounted
#      read-only, so jeryu-api's build.rs re-verifies the dist hash against the
#      lock before embedding it, and the binary runs on the forge host's glibc.
#      JERYU_BUILD_COMMIT=<COMMIT> is passed in too, so the binary reports the
#      commit it was built from (/api/v1/version, /runners) without relying on
#      git working inside the container.
#   2. Refuse a binary that needs a newer glibc than the forge host has.
#   3. Stage bundle/jeryu, that same web dist, RELEASE.txt (with the jeryu-web
#      commit and dist hash), RELEASE.env
#      (REL and PREV, where PREV is read from the live symlink on the forge
#      host), switch.sh, rollback.sh, install-forge-unit.sh and the
#      jeryu.service.in it renders, all from the same commit, and SHA256SUMS.
#   4. Copy to the forge host's ~/.jeryu/incoming/<REL>/ and verify the sums
#      and `jeryu --version` there.
#
# Nothing on the forge host changes except the new incoming directory. Prints
# the release id on its last line.
#
# --dry-run resolves the commit, reads the live release and prints the release
# id it would stage, then stops: nothing is built, copied or started.
# --json prints exactly one JSON line on stdout instead (everything else goes to
# stderr): {"release","commit","previous_release","dry_run"} on success, or the
# API's error envelope {"code","message","exit_code"} on a refusal.
# --prev RELEASE (or --prev=RELEASE) accepts a live release whose name is not in
# the usual prod-<UTC stamp>-<sha7>-unsigned form (a hand-installed build, say),
# which staging otherwise refuses. RELEASE must still name exactly what is live.
# Exit codes: 0 staged, 64 usage (bad argument or env value), 65 state (the
# live release or the commit is not what staging expects), 69 unreachable (a
# host or the remote did not answer), 70 build (the binary needs a newer glibc),
# 1 anything else.
#
# Env: JERYU_BUILD_HOST and JERYU_FORGE_HOST (both required, no default: this
# source is public and a machine name is one installation's data; the forge host
# is reached through the build host), JERYU_BUILD_ROOT (jeryu-release-build under the build host's
# $HOME; an absolute path, or one starting with ~/ for the build host's home),
# JERYU_BUILDER_IMAGE (jeryu-builder:rust1.95-glibc2.35-r2, built from
# scripts/release/builder.Dockerfile on the build host when missing), JERYU_MAX_GLIBC (2.35),
# JERYU_DEPLOY_REMOTE (https://git.neverhuman.org/git/jeryu/jeryu-deploy.git).
# jeryu-web is cloned on the build host from build-web-dist.sh's default remote.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
json=0; dry_run=0; prev_override=""; args=()
[[ " $* " != *" --json "* ]] || json=1
exec 3>&1
refuse() { # CLASS MESSAGE — exit with the class's code; under --json also print the error envelope
  local code
  case "$1" in usage) code=64 ;; state) code=65 ;; unreachable) code=69 ;; build) code=70 ;; *) code=1 ;; esac
  echo "$2" >&2
  [[ $json == 0 ]] || jq -cn --arg c "$1" --arg m "$2" --argjson e "$code" '{code:$c, message:$m, exit_code:$e}' >&3
  exit "$code"
}
while (($#)); do
  case "$1" in
    --json) ;;
    --dry-run) dry_run=1 ;;
    --prev=*) prev_override="${1#--prev=}" ;;
    --prev) (($# >= 2)) || refuse usage "--prev needs a release name"; prev_override="$2"; shift ;;
    -*) refuse usage "unknown option '$1'" ;;
    *) args+=("$1") ;;
  esac
  shift
done
set -- ${args[@]+"${args[@]}"}
(($# <= 1)) || refuse usage "usage: stage-release.sh [--json] [--dry-run] [--prev RELEASE] [COMMIT]"
[[ -z "$prev_override" || "$prev_override" =~ ^[A-Za-z0-9._-]+$ ]] || refuse usage "bad --prev '$prev_override'"
[[ $json == 0 ]] || exec 1>&2
build_host="${JERYU_BUILD_HOST:-}"
forge_host="${JERYU_FORGE_HOST:-}"
[[ -n "$build_host" ]] || refuse usage "JERYU_BUILD_HOST is unset: set it to this installation's release build host"
[[ -n "$forge_host" ]] || refuse usage "JERYU_FORGE_HOST is unset: set it to this installation's forge host"
# Expanded by the build host's shell, never here: the default and a leading ~/ become its $HOME.
build_root="${JERYU_BUILD_ROOT:-\$HOME/jeryu-release-build}"
[[ "$build_root" != "~/"* ]] || build_root="\$HOME/${build_root#"~/"}"
[[ "$build_root" =~ ^(/|\$HOME/)[A-Za-z0-9._/-]+$ ]] || refuse usage "JERYU_BUILD_ROOT must be absolute or start with ~/, got '${JERYU_BUILD_ROOT:-}'"
image="${JERYU_BUILDER_IMAGE:-jeryu-builder:rust1.95-glibc2.35-r2}"
[[ "$image" =~ ^[a-z0-9._/-]+:[A-Za-z0-9._-]+$ ]] || refuse usage "bad builder image '$image'"
here="$(cd "$(dirname "$0")" && pwd)"
max_glibc="${JERYU_MAX_GLIBC:-2.35}"
remote="${JERYU_DEPLOY_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-deploy.git}"
commit="${1:-}"
if [[ -z "$commit" ]]; then
  commit="$(git ls-remote "$remote" refs/heads/main | cut -f1)" || refuse unreachable "cannot read main from $remote"
fi
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || refuse usage "commit must be a full 40-hex sha, got '$commit'"

prev="$(ssh "$build_host" "ssh -n $forge_host 'readlink ~/.jeryu/bin/jeryu'")" \
  || refuse unreachable "cannot read the live release on $forge_host through $build_host"
prev="${prev#jeryu-}"
if [[ -n "$prev_override" ]]; then
  [[ "$prev" == "$prev_override" ]] || refuse state "--prev '$prev_override' is not the live release '$prev'; refusing"
else
  [[ "$prev" =~ ^prod-[0-9]{8}T[0-9]{6}Z-[0-9a-f]+-unsigned$ ]] \
    || refuse state "unexpected live release '$prev'; refusing (pass --prev '$prev' if it is the intended rollback target)"
fi
rel="prod-$(date -u +%Y%m%dT%H%M%SZ)-${commit:0:7}-unsigned"
done_json() { # the one success line under --json
  [[ $json == 0 ]] || jq -cn --arg rel "$rel" --arg commit "$commit" --arg prev "$prev" --argjson dry "$dry_run" \
    '{release:$rel, commit:$commit, previous_release:$prev, dry_run:($dry == 1)}' >&3
}
if [[ $dry_run == 1 ]]; then
  echo "[stage] dry run: would stage $rel from $commit (rollback target $prev); nothing built or copied" >&2
  done_json
  [[ $json == 1 ]] || echo "$rel"
  exit 0
fi

# The builder is defined here, not borrowed: build it on the build host when its tag is missing.
if ! ssh -n "$build_host" "docker image inspect $image >/dev/null 2>&1"; then
  echo "[stage] building $image on $build_host from builder.Dockerfile" >&2
  ssh "$build_host" "docker build -q -t $image -" < "$here/builder.Dockerfile" >&2
fi
echo "[stage] $rel from $commit (rollback target $prev)" >&2

# The remote refusals exit 65 (state) and 70 (build) so they keep their class here.
set +e
# shellcheck disable=SC2087 # expand locally on purpose: every value is validated above
ssh "$build_host" bash -s <<EOF
set -euo pipefail
root="$build_root"; mkdir -p "\$root"
[[ -d "\$root/jeryu-deploy/.git" ]] || git clone -q "$remote" "\$root/jeryu-deploy"
cd "\$root/jeryu-deploy"
git fetch -q origin
git checkout -q --detach "$commit"
[[ "\$(git rev-parse HEAD)" == "$commit" ]]
[[ -f scripts/release/build-web-dist.sh ]] || { echo "$commit predates the pinned jeryu-web dist; refusing" >&2; exit 65; }
web="\$(JERYU_WEB_SRC="\$root/jeryu-web" bash scripts/release/build-web-dist.sh "\$root/web-dist" | tail -1)"
web_commit="\${web%% *}"; web_sha="\${web##* }"
web_dist="\$root/web-dist/\$web_commit/dist"
GIT_CONFIG_GLOBAL="\$PWD/.cargo/hosted-gitconfig" PATH="\$HOME/.cargo/bin:\$PATH" cargo fetch --locked >/dev/null
docker run --rm --network none --user "\$(id -u):\$(id -g)" \
  -v "\$web_dist":/web-dist:ro -e JERYU_WEB_DIST=/web-dist -e JERYU_BUILD_COMMIT="$commit" \
  -e HOME=/tmp -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/src/target-release -e CARGO_INCREMENTAL=0 \
  -e PATH=/opt/rust/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/bin:/usr/bin:/bin \
  -e RUSTUP_HOME=/opt/rust/rustup -e RUSTUP_TOOLCHAIN=1.95.0 \
  -v "\$root":/src -v "\$HOME/.cargo/registry":/cargo/registry -v "\$HOME/.cargo/git":/cargo/git \
  -w /src/jeryu-deploy "$image" \
  cargo build --release --locked --offline -p jeryu-cli --bin jeryu >&2
bin="\$root/target-release/release/jeryu"
glibc="\$(objdump -T "\$bin" | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1)"
echo "[stage] binary needs \$glibc (max $max_glibc)" >&2
[[ "\$(printf '%s\n%s\n' "\${glibc#GLIBC_}" "$max_glibc" | sort -V | tail -1)" == "$max_glibc" ]] \
  || { echo "binary needs \$glibc, newer than $max_glibc; refusing" >&2; exit 70; }

stage="\$root/stage/$rel"; rm -rf "\$stage"; mkdir -p "\$stage/bundle" "\$stage/web-dist"
cp "\$bin" "\$stage/bundle/jeryu"
cp -a "\$web_dist/." "\$stage/web-dist/"; chmod -R u+w "\$stage/web-dist"
cp scripts/release/switch.sh scripts/release/rollback.sh scripts/release/install-forge-unit.sh \
  scripts/release/systemd/jeryu.service.in "\$stage/"
printf 'REL=%s\nPREV=%s\n' "$rel" "$prev" > "\$stage/RELEASE.env"
printf 'release=%s\njeryu_deploy_commit=%s\njeryu_web_commit=%s\nweb_dist_sha256=%s\nsigned=false (owner decision)\nbuilder=%s, --network none, cargo --locked --offline\nrollback_target=%s\nbinary_glibc=%s\n' \
  "$rel" "$commit" "\$web_commit" "\$web_sha" "$image" "$prev" "\$glibc" > "\$stage/RELEASE.txt"
(cd "\$stage" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
rsync -a "\$stage" $forge_host:.jeryu/incoming/
ssh -n $forge_host "cd ~/.jeryu/incoming/$rel && sha256sum --quiet -c SHA256SUMS && chmod +x switch.sh rollback.sh install-forge-unit.sh && ./bundle/jeryu --version" >&2
EOF
rc=$?
set -e
case $rc in
  0) ;;
  65) refuse state "$commit predates the pinned jeryu-web dist" ;;
  70) refuse build "the binary needs a newer glibc than $max_glibc" ;;
  255) refuse unreachable "ssh to $build_host failed while staging $rel" ;;
  *) refuse failed "staging $rel on $build_host exited $rc" ;;
esac
echo "[stage] staged on $forge_host:~/.jeryu/incoming/$rel; deploy with: scripts/release/deploy-release.sh $rel" >&2
done_json
[[ $json == 1 ]] || echo "$rel"
