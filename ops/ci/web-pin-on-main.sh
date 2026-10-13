#!/usr/bin/env bash
# The jeryu-web commit pinned in jeryu-split.lock.toml must be on jeryu-web main.
#
# The pin is all a web bump changes: one commit id (and the bundle hash built from
# it). verify-lock checks only its shape, and the bundle hash is rebuilt from the
# commit, so without this a pin to any commit the jeryu-web repository holds (a
# side branch that never passed jeryu-web/required or a review) would build, embed
# and ship. jeryu-web main takes only reviewed, gated merges, so a pin on main is
# a pin to reviewed, gated code, and a review of the bump can rely on that.
#
# JERYU_WEB_REMOTE overrides where jeryu-web is fetched from (as in
# scripts/release/build-web-dist.sh); JERYU_SPLIT_LOCK the lock file; the fetched
# history is kept in target/ci/jeryu-web.git between runs.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
lock="${JERYU_SPLIT_LOCK:-$root/jeryu-split.lock.toml}"
remote="${JERYU_WEB_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-web.git}"
cache="${JERYU_WEB_PIN_CACHE:-$root/target/ci/jeryu-web.git}"

# The jeryu-web [[repo]] block's commit, as build-web-dist.sh and auto-pin.sh read it.
pin="$(awk '
  /^\[\[repo\]\]/ { inweb = 0 }
  /^name = "jeryu-web"$/ { inweb = 1 }
  inweb && $1 == "commit" && $2 == "=" { gsub(/"/, "", $3); print $3; exit }' "$lock")"
if ! [[ "$pin" =~ ^[0-9a-f]{40}$ ]]; then
  echo "web pin: $lock has no 40-hex jeryu-web commit (${pin:-none})" >&2
  exit 1
fi

if [ -e "$cache" ]; then
  if [ "$(git -C "$cache" rev-parse --is-bare-repository 2>/dev/null || true)" != true ]; then
    echo "web pin: invalid local cache $cache; choose a new JERYU_WEB_PIN_CACHE (existing data retained)" >&2
    exit 1
  fi
else
  git init -q --bare "$cache"
fi
if ! GIT_TERMINAL_PROMPT=0 git -C "$cache" fetch --quiet --no-tags "$remote" \
    "+refs/heads/main:refs/heads/main"; then
  echo "web pin: could not fetch jeryu-web main from $remote; the pin cannot be checked" >&2
  exit 1
fi
main="$(git -C "$cache" rev-parse refs/heads/main)"

if ! git -C "$cache" cat-file -e "$pin^{commit}" 2>/dev/null; then
  echo "web pin: jeryu-web ${pin:0:12} is absent from the fetched main history" >&2
  exit 1
fi
if ! git -C "$cache" merge-base --is-ancestor "$pin" "$main" 2>/dev/null; then
  echo "web pin: jeryu-web ${pin:0:12} is not on jeryu-web main (${main:0:12})." >&2
  echo "web pin: pin a commit that reached main through a reviewed, gated merge." >&2
  exit 1
fi
echo "web pin: jeryu-web ${pin:0:12} is on jeryu-web main (${main:0:12})"
