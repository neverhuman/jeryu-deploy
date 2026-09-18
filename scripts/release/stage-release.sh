#!/usr/bin/env bash
# stage-release.sh [COMMIT] — build jeryu-deploy at COMMIT (default: the forge's
# current main) and stage it on the forge host, ready for deploy-release.sh.
#
#   1. On the build host: a dedicated checkout at COMMIT, `cargo fetch --locked`
#      through the hosted transport overlay (the build itself is offline), then
#      `cargo build --release --locked --offline` in the glibc 2.35 builder image
#      with no network, so the binary runs on the forge host's glibc.
#   2. Refuse a binary that needs a newer glibc than the forge host has.
#   3. Stage bundle/jeryu, the vendored web dist, RELEASE.txt, RELEASE.env
#      (REL and PREV, where PREV is read from the live symlink on the forge
#      host), switch.sh and rollback.sh from the same commit, and SHA256SUMS.
#   4. Copy to the forge host's ~/.jeryu/incoming/<REL>/ and verify the sums
#      and `jeryu --version` there.
#
# Nothing on the forge host changes except the new incoming directory. Prints
# the release id on its last line.
#
# Env: JERYU_BUILD_HOST (xbabe2), JERYU_FORGE_HOST (atomicsoul, reached through
# the build host), JERYU_BUILD_ROOT (~/jeryu-release-build on the build host),
# JERYU_BUILDER_IMAGE (jeryu-builder:rust1.95-glibc2.35), JERYU_MAX_GLIBC (2.35),
# JERYU_DEPLOY_REMOTE (https://git.neverhuman.org/git/jeryu/jeryu-deploy.git).
set -euo pipefail
build_host="${JERYU_BUILD_HOST:-xbabe2}"
forge_host="${JERYU_FORGE_HOST:-atomicsoul}"
build_root="${JERYU_BUILD_ROOT:-~/jeryu-release-build}"
image="${JERYU_BUILDER_IMAGE:-jeryu-builder:rust1.95-glibc2.35}"
max_glibc="${JERYU_MAX_GLIBC:-2.35}"
remote="${JERYU_DEPLOY_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-deploy.git}"
commit="${1:-}"
if [[ -z "$commit" ]]; then
  commit="$(git ls-remote "$remote" refs/heads/main | cut -f1)"
fi
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || { echo "commit must be a full 40-hex sha, got '$commit'" >&2; exit 1; }

prev="$(ssh "$build_host" "ssh -n $forge_host 'readlink ~/.jeryu/bin/jeryu'")"
prev="${prev#jeryu-}"
[[ "$prev" =~ ^prod-[0-9]{8}T[0-9]{6}Z-[0-9a-f]+-unsigned$ ]] || { echo "unexpected live release '$prev'; refusing" >&2; exit 1; }
rel="prod-$(date -u +%Y%m%dT%H%M%SZ)-${commit:0:7}-unsigned"
echo "[stage] $rel from $commit (rollback target $prev)" >&2

# shellcheck disable=SC2087 # expand locally on purpose: every value is validated above
ssh "$build_host" bash -s <<EOF
set -euo pipefail
root=$build_root; mkdir -p "\$root"
[[ -d "\$root/jeryu-deploy/.git" ]] || git clone -q "$remote" "\$root/jeryu-deploy"
cd "\$root/jeryu-deploy"
git fetch -q origin
git checkout -q --detach "$commit"
[[ "\$(git rev-parse HEAD)" == "$commit" ]]
GIT_CONFIG_GLOBAL="\$PWD/.cargo/hosted-gitconfig" PATH="\$HOME/.cargo/bin:\$PATH" cargo fetch --locked >/dev/null
docker run --rm --network none --user "\$(id -u):\$(id -g)" \
  -e HOME=/tmp -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/src/target-release -e CARGO_INCREMENTAL=0 \
  -e PATH=/usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/bin:/usr/local/cargo/bin:/usr/bin:/bin \
  -e RUSTUP_HOME=/usr/local/rustup -e RUSTUP_TOOLCHAIN=1.95.0 \
  -v "\$root":/src -v "\$HOME/.cargo/registry":/cargo/registry -v "\$HOME/.cargo/git":/cargo/git \
  -w /src/jeryu-deploy "$image" \
  cargo build --release --locked --offline -p jeryu-cli --bin jeryu >&2
bin="\$root/target-release/release/jeryu"
glibc="\$(objdump -T "\$bin" | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1)"
echo "[stage] binary needs \$glibc (max $max_glibc)" >&2
[[ "\$(printf '%s\n%s\n' "\${glibc#GLIBC_}" "$max_glibc" | sort -V | tail -1)" == "$max_glibc" ]] \
  || { echo "binary needs \$glibc, newer than $max_glibc; refusing" >&2; exit 1; }

stage="\$root/stage/$rel"; rm -rf "\$stage"; mkdir -p "\$stage/bundle" "\$stage/web-dist"
cp "\$bin" "\$stage/bundle/jeryu"
tmp="\$(mktemp -d)"; git archive "$commit" apps/web/dist | tar -x -C "\$tmp"
cp -a "\$tmp/apps/web/dist/." "\$stage/web-dist/"; rm -rf "\$tmp"
cp scripts/release/switch.sh scripts/release/rollback.sh "\$stage/"
printf 'REL=%s\nPREV=%s\n' "$rel" "$prev" > "\$stage/RELEASE.env"
printf 'release=%s\njeryu_deploy_commit=%s\nsigned=false (owner decision)\nbuilder=%s, --network none, cargo --locked --offline\nrollback_target=%s\nbinary_glibc=%s\n' \
  "$rel" "$commit" "$image" "$prev" "\$glibc" > "\$stage/RELEASE.txt"
(cd "\$stage" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
rsync -a "\$stage" $forge_host:.jeryu/incoming/
ssh -n $forge_host "cd ~/.jeryu/incoming/$rel && sha256sum --quiet -c SHA256SUMS && chmod +x switch.sh rollback.sh && ./bundle/jeryu --version" >&2
EOF
echo "[stage] staged on $forge_host:~/.jeryu/incoming/$rel; deploy with: scripts/release/deploy-release.sh $rel" >&2
echo "$rel"
