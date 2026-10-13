#!/usr/bin/env bash
# verify-pin-bump.sh [--json] BASE HEAD — decide whether a jeryu-deploy change is exactly a
# mechanical jeryu-web pin bump. This is a standalone diagnostic; no gate calls it or skips
# any lane because of its result. Design and risks: docs/pin-bump-fast-lane.md.
#
# BASE and HEAD are revisions of the repository in the current directory (the target branch tip
# and the pull request head). The change is eligible only when ALL of these hold:
#   1. the switch is on: JERYU_PIN_FAST_LANE=1 (default off; anything else is the normal path);
#   2. the only file that differs is jeryu-split.lock.toml, with its mode unchanged;
#   3. that file differs only in the jeryu-web block's `commit` and `web_dist_sha256` lines, which
#      each appear exactly once in the one jeryu-web block of both revisions;
#   4. the new commit is 40-hex, differs from the pinned one, descends from it, and is an ancestor
#      of jeryu-web main (it was merged through jeryu-web's own gate and review);
#   5. the forge reports that commit's combined status `success` including the lock's
#      `required_check` for jeryu-web;
#   6. build-web-dist.sh, taken from BASE (never from HEAD), rebuilds that commit to exactly the
#      web_dist_sha256 in HEAD.
# Anything else, and any doubt (a failed command, an unreachable forge, a missing field), is the
# normal path. Only exit status 0 means eligible; 10 is "normal path" with the reason on stdout;
# 64 is a usage error. It never approves, merges, comments or writes to the forge.
#
# Stdout: one line `eligible <commit> <web_dist_sha256>` or `normal-path: <reason>`; with --json
# one object {"eligible": bool, "reason": ..., "commit": ..., "web_dist_sha256": ...}.
#
# Env: JERYU_PIN_FAST_LANE (1 to turn the lane on); JERYU_WEB_REMOTE (git URL of jeryu-web);
# JERYU_WEB_REPO (owner/name on the forge; default: from the remote);
# JERYU_BASE (must be https://git.neverhuman.org: the token is sent nowhere else);
# JERYU_PIN_TOKEN_FILE (required: a regular 0600 file holding a read token for the status API);
# JERYU_FAST_LANE_STATE (a directory kept between runs for the clone and the build; default: a
# temporary one, removed on exit).
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -Eeuo pipefail
lock_file=jeryu-split.lock.toml
as_json=0
if [[ "${1:-}" == --json ]]; then as_json=1; shift; fi
[[ $# == 2 ]] || { echo "usage: verify-pin-bump.sh [--json] BASE HEAD" >&2; exit 64; }
base_rev="$1" head_rev="$2"
commit="" hash=""

verdict() { # eligible|normal-path REASON
  if (( as_json )); then
    jq -cn --arg e "$1" --arg r "$2" --arg c "$commit" --arg h "$hash" \
      '{eligible: ($e == "eligible"), reason: $r, commit: $c, web_dist_sha256: $h}'
  elif [[ "$1" == eligible ]]; then echo "eligible $commit $hash"
  else echo "normal-path: $2"; fi
}
no() { verdict normal-path "$*"; exit 10; }
trap 'no "unexpected failure at line $LINENO; not guessing"' ERR

[[ "${JERYU_PIN_FAST_LANE:-0}" == 1 ]] || no "the fast lane is off (JERYU_PIN_FAST_LANE is not 1)"

work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
state="${JERYU_FAST_LANE_STATE:-$work/state}"; mkdir -p "$state"
base="$(git rev-parse --verify --quiet "$base_rev^{commit}")" || no "BASE $base_rev is not a commit"
head="$(git rev-parse --verify --quiet "$head_rev^{commit}")" || no "HEAD $head_rev is not a commit"
protected_main="$(git rev-parse --verify --quiet 'refs/remotes/origin/main^{commit}')" \
  || no "origin/main is unavailable; fetch protected main before this diagnostic"
git merge-base --is-ancestor "$base" "$protected_main" \
  || no "BASE is not an ancestor of protected origin/main"

# 2. exactly the lock file, same mode.
raw="$(git diff --raw --no-renames --no-abbrev "$base" "$head")"
[[ -n "$raw" ]] || no "no difference between BASE and HEAD"
[[ "$(wc -l <<<"$raw")" == 1 ]] || no "more than one file differs"
re=$'^:100644 100644 [0-9a-f]+ [0-9a-f]+ M\t'"$lock_file"'$'
[[ "$raw" =~ $re ]] || no "the one differing file is not $lock_file with an unchanged regular mode"

# 3. the jeryu-web block: its two pin lines exactly once; the rest of the file byte-identical.
web_field() { # REV KEY -> the value, only when the jeryu-web block has exactly one such line
  git show "$1:$lock_file" | awk -v key="$2" '
    /^\[\[repo\]\]/ { inweb = 0 }
    /^name = "jeryu-web"$/ { inweb = 1 }
    inweb && $1 == key && $2 == "=" { n++; v = $3; gsub(/"/, "", v) }
    END { if (n == 1) print v; else exit 1 }'
}
web_blocks() { git show "$1:$lock_file" | grep -c '^name = "jeryu-web"$' || true; }
rest() { # REV -> the lock with the two pin lines of the jeryu-web block removed
  git show "$1:$lock_file" | awk '
    /^\[\[repo\]\]/ { inweb = 0 }
    /^name = "jeryu-web"$/ { inweb = 1 }
    inweb && $1 == "commit" && $2 == "=" { next }
    inweb && $1 == "web_dist_sha256" && $2 == "=" { next }
    { print }'
}
[[ "$(web_blocks "$base")" == 1 && "$(web_blocks "$head")" == 1 ]] || no "$lock_file has no single jeryu-web block"
old_commit="$(web_field "$base" commit)" || no "BASE lock has no single jeryu-web commit line"
old_hash="$(web_field "$base" web_dist_sha256)" || no "BASE lock has no single jeryu-web web_dist_sha256 line"
commit="$(web_field "$head" commit)" || no "HEAD lock has no single jeryu-web commit line"
hash="$(web_field "$head" web_dist_sha256)" || no "HEAD lock has no single jeryu-web web_dist_sha256 line"
[[ "$(rest "$base")" == "$(rest "$head")" ]] || no "$lock_file changes more than the jeryu-web pin lines"

# 4. a new, well-formed commit, ahead of the pin.
[[ "$old_commit" =~ ^[0-9a-f]{40}$ && "$commit" =~ ^[0-9a-f]{40}$ ]] || no "a jeryu-web commit is not 40-hex"
[[ "$hash" =~ ^[0-9a-f]{64}$ && "$old_hash" =~ ^[0-9a-f]{64}$ ]] || no "a web_dist_sha256 is not 64-hex"
[[ "$commit" != "$old_commit" ]] || no "the jeryu-web commit does not move"

web_remote="${JERYU_WEB_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-web.git}"
web_git="$state/jeryu-web.git"
[[ -d "$web_git" ]] || git init -q --bare "$web_git"
git -C "$web_git" fetch -q "$web_remote" "+refs/heads/main:refs/heads/main" || no "could not fetch jeryu-web main"
git -C "$web_git" cat-file -e "$commit^{commit}" 2>/dev/null || no "$commit is not a jeryu-web commit"
git -C "$web_git" cat-file -e "$old_commit^{commit}" 2>/dev/null || no "the pinned commit is not in jeryu-web"
git -C "$web_git" merge-base --is-ancestor "$commit" refs/heads/main || no "$commit is not on jeryu-web main"
git -C "$web_git" merge-base --is-ancestor "$old_commit" "$commit" || no "$commit does not descend from the pin"

# 5. the forge's word that jeryu-web's required check passed on exactly that commit.
base_url="${JERYU_BASE:-https://git.neverhuman.org}"; base_url="${base_url%/}"
[[ "$base_url" == https://git.neverhuman.org ]] || no "refusing a noncanonical credential origin"
token_file="${JERYU_PIN_TOKEN_FILE:-}"
[[ -n "$token_file" && -f "$token_file" && ! -L "$token_file" && "$(stat -c '%a' "$token_file")" == 600 ]] \
  || no "JERYU_PIN_TOKEN_FILE is not a regular mode-0600 file"
token="$(cat "$token_file")"
[[ "$token" =~ ^[A-Za-z0-9._~+/-]+=*$ ]] || no "the token has an invalid bearer-token shape"
cfg="$work/curl.cfg"; : >"$cfg"; chmod 600 "$cfg"
printf 'header = "Authorization: Bearer %s"\n' "$token" >"$cfg"; unset token
web_path="${JERYU_WEB_REPO:-$(sed -E 's#^https?://[^/]+/git/##; s#\.git$##' <<<"$web_remote")}"
[[ "$web_path" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || no "not an owner/name repository: $web_path"
required_check="$(web_field "$base" required_check)" || no "BASE lock has no jeryu-web required_check"
status="$(curl --silent --show-error --fail --max-time 60 --config "$cfg" -H 'Accept: application/json' \
  "$base_url/api/v3/repos/$web_path/commits/$commit/status")" || no "could not read the status of $commit"
jq -e --arg c "$required_check" '
  .state == "success" and ([(.statuses // [])[] | select(.context == $c and .state == "success")] | length) >= 1' \
  <<<"$status" >/dev/null || no "$required_check is not success on $commit"

# 6. the reproducible build, with the recipe from BASE: a pull request cannot change what proves it.
mkdir -p "$work/recipe"
git archive "$base" scripts/release "$lock_file" | tar -x -C "$work/recipe"
pair="$(JERYU_WEB_REMOTE="$web_remote" "$work/recipe/scripts/release/build-web-dist.sh" \
  --commit "$commit" "$state/web-dist" | tail -n 1)" || no "the reproducible build failed"
read -r built built_hash <<<"$pair"
[[ "$built" == "$commit" ]] || no "the build did not report $commit"
[[ "$built_hash" == "$hash" ]] || no "the build of $commit hashes to $built_hash, not the pinned $hash"

verdict eligible "mechanical jeryu-web pin bump, verified"
