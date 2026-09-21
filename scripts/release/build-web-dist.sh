#!/usr/bin/env bash
# build-web-dist.sh [--commit SHA] OUT_ROOT — build the jeryu-web SPA pinned in
# jeryu-split.lock.toml, outside any checkout, for the offline release build.
#
#   1. Read the jeryu-web `commit` and `web_dist_sha256` from the lock (or take
#      --commit to build a candidate for a lock bump; the hash is then printed,
#      not checked).
#   2. Check out jeryu-web at exactly that commit in JERYU_WEB_SRC (cloned from
#      JERYU_WEB_REMOTE when missing); refuse a dirty tree or a HEAD that is not
#      the locked commit.
#   3. `npm ci && vite build` in the pinned node image (network allowed here,
#      and only here) after `git clean -fdx`, so nothing from an earlier
#      build leaks in.
#   4. Copy the dist to OUT_ROOT/<commit>/dist, write the sorted-path sha256
#      manifest (`sha256sum` format) to OUT_ROOT/<commit>/MANIFEST.sha256, and
#      the manifest's own sha256 to OUT_ROOT/<commit>/DIST.sha256.
#   5. Without --commit, refuse a hash that differs from the lock.
#
# The last line is `<commit> <sha256>`. crates/jeryu-api/build.rs recomputes the
# same hash from JERYU_WEB_DIST=OUT_ROOT/<commit>/dist and fails closed on a
# mismatch.
#
# Env: JERYU_WEB_SRC (OUT_ROOT/src/jeryu-web), JERYU_WEB_REMOTE
# (https://git.neverhuman.org/git/jeryu/jeryu-web.git), JERYU_WEB_NODE_IMAGE
# (node 20.20.1 by digest), JERYU_SPLIT_LOCK (the lock next to this script).
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
lock="${JERYU_SPLIT_LOCK:-$here/../../jeryu-split.lock.toml}"
node_image="${JERYU_WEB_NODE_IMAGE:-node:20.20.1-bookworm-slim@sha256:17281e8d1dc4d671976c6b89a12f47a44c2f390b63a989e2e327631041f544fd}"
node_version=v20.20.1 npm_version=10.8.2
remote="${JERYU_WEB_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-web.git}"

candidate=""
if [[ "${1:-}" == --commit ]]; then candidate="${2:?--commit needs a sha}"; shift 2; fi
out_root="${1:?usage: build-web-dist.sh [--commit SHA] OUT_ROOT}"
mkdir -p "$out_root"; out_root="$(cd "$out_root" && pwd)"
src="${JERYU_WEB_SRC:-$out_root/src/jeryu-web}"

# The jeryu-web [[repo]] block of the lock: key = "value" lines until the next block.
lock_field() {
  awk -v key="$1" '
    /^\[\[repo\]\]/ { inweb = 0 }
    /^name = "jeryu-web"$/ { inweb = 1 }
    inweb && $1 == key && $2 == "=" { gsub(/"/, "", $3); print $3; exit }
  ' "$lock"
}
grep -qx 'web_artifact = "pinned"' "$lock" || { echo "$lock: web_artifact must be \"pinned\"" >&2; exit 1; }
expected=""
if [[ -n "$candidate" ]]; then
  commit="$candidate"
else
  commit="$(lock_field commit)"; expected="$(lock_field web_dist_sha256)"
  [[ "$expected" =~ ^[0-9a-f]{64}$ ]] || { echo "$lock: jeryu-web web_dist_sha256 must be 64-hex, got '$expected'" >&2; exit 1; }
fi
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || { echo "jeryu-web commit must be a full 40-hex sha, got '$commit'" >&2; exit 1; }

[[ -d "$src/.git" ]] || git clone -q "$remote" "$src"
git -C "$src" cat-file -e "$commit^{commit}" 2>/dev/null || git -C "$src" fetch -q origin
[[ -z "$(git -C "$src" status --porcelain)" ]] || { echo "jeryu-web checkout $src is dirty; refusing" >&2; exit 1; }
git -C "$src" checkout -q --detach "$commit"
head="$(git -C "$src" rev-parse HEAD)"
[[ "$head" == "$commit" ]] || { echo "jeryu-web HEAD $head is not the locked $commit; refusing" >&2; exit 1; }
[[ -z "$(git -C "$src" status --porcelain)" ]] || { echo "jeryu-web checkout is dirty at $commit; refusing" >&2; exit 1; }
# Nothing from an earlier build may leak into this one.
git -C "$src" clean -qfdx

echo "[web] building jeryu-web $commit in $node_image" >&2
docker run --rm --user "$(id -u):$(id -g)" \
  -e HOME=/tmp -e npm_config_cache=/tmp/npm-cache -e CI=1 \
  -e NODE_ENV=production -e SOURCE_DATE_EPOCH=0 -e TZ=UTC -e LC_ALL=C \
  -v "$src":/src/jeryu-web -w /src/jeryu-web "$node_image" \
  sh -euc "
    [ \"\$(node --version)\" = $node_version ] && [ \"\$(npm --version)\" = $npm_version ]
    npm ci --include=dev --no-audit --no-fund --ignore-scripts >&2
    cd apps/web && npx --no-install vite build --logLevel warn >&2
  "

dest="$out_root/$commit"
tmp="$(mktemp -d "$out_root/.build.XXXXXX")"
trap 'chmod -R u+w "$tmp" 2>/dev/null; rm -rf "$tmp"' EXIT
cp -a "$src/apps/web/dist" "$tmp/dist"
if find "$tmp/dist" ! -type f ! -type d | grep -q .; then
  echo "[web] dist contains something other than regular files and directories; refusing" >&2; exit 1
fi
chmod -R a-w "$tmp/dist"
(cd "$tmp/dist" && find . -type f -printf '%P\0' | LC_ALL=C sort -z | xargs -0 sha256sum) >"$tmp/MANIFEST.sha256"
hash="$(sha256sum "$tmp/MANIFEST.sha256" | cut -c1-64)"
printf '%s\n' "$hash" >"$tmp/DIST.sha256"
git -C "$src" clean -qfdx

if [[ -n "$expected" && "$hash" != "$expected" ]]; then
  echo "[web] dist for $commit hashes to $hash, the lock expects $expected; refusing" >&2
  exit 1
fi
[[ ! -e "$dest" ]] || { chmod -R u+w "$dest"; rm -rf "$dest"; }
mv "$tmp" "$dest"; trap - EXIT
echo "[web] $dest/dist ($(wc -l <"$dest/MANIFEST.sha256") files)" >&2
echo "$commit $hash"
