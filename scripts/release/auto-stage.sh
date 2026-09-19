#!/usr/bin/env bash
# auto-stage.sh — stage the forge's current jeryu-deploy main once its gate is green, so a
# release "go" is only the switch. Run by a timer on the release host (xbabe0); never deploys.
#
#   1. Resolve main's tip. Nothing to do when it is already live or already staged.
#   2. Wait until the tip's combined status is `success` (the gate ran on the PR head that
#      fast-forwarded main, or on the queue commit that landed it).
#   3. Run stage-release.sh from THAT commit (git archive of scripts/release), so the staging
#      recipe is always the one that was reviewed with the code it stages.
#   4. Record the result. A commit that fails to stage is retried at most twice, then left for a
#      human; a newer main commit starts fresh.
#
# State in $JERYU_AUTO_STAGE_STATE (~/.local/state/jeryu-auto-stage): staged.tsv (sha, release,
# time), failures/<sha> (attempt count), latest (the newest staged release id), a git cache.
#
# Env: JERYU_DEPLOY_REMOTE, JERYU_BUILD_HOST, JERYU_FORGE_HOST (as stage-release.sh);
# JERYU_BASE (https://git.neverhuman.org); JERYU_STATUS_TOKEN_FILE (a read token for the
# commit-status API, default ~/.config/jeryu/credentials/git-neverhuman-org-alton2.pat);
# JERYU_AUTO_STAGE_MAX_FAILURES (2).
set -euo pipefail
remote="${JERYU_DEPLOY_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-deploy.git}"
build_host="${JERYU_BUILD_HOST:-xbabe2}"
forge_host="${JERYU_FORGE_HOST:-atomicsoul}"
base="${JERYU_BASE:-https://git.neverhuman.org}"
token_file="${JERYU_STATUS_TOKEN_FILE:-$HOME/.config/jeryu/credentials/git-neverhuman-org-alton2.pat}"
max_failures="${JERYU_AUTO_STAGE_MAX_FAILURES:-2}"
state="${JERYU_AUTO_STAGE_STATE:-$HOME/.local/state/jeryu-auto-stage}"
say() { echo "[auto-stage $(date -u +%H:%M:%S)] $*" >&2; }

mkdir -p "$state/failures"
exec 9>"$state/lock"
flock -n 9 || { say "another run holds the lock"; exit 0; }
touch "$state/staged.tsv"

main="$(git ls-remote "$remote" refs/heads/main | cut -f1)"
[[ "$main" =~ ^[0-9a-f]{40}$ ]] || { say "could not resolve main ($main)"; exit 1; }

if cut -f1 "$state/staged.tsv" | grep -qx "$main"; then exit 0; fi
live="$(ssh "$build_host" "ssh -n $forge_host 'readlink ~/.jeryu/bin/jeryu'")"
if [[ "$live" == *"-${main:0:7}-unsigned" ]]; then
  say "main ${main:0:12} is already live ($live)"
  printf '%s\t%s\t%s\n' "$main" "${live#jeryu-}" "$(date -u +%FT%TZ)" >>"$state/staged.tsv"
  exit 0
fi
failures="$(cat "$state/failures/$main" 2>/dev/null || echo 0)"
if (( failures >= max_failures )); then
  say "main ${main:0:12} failed to stage $failures times; leaving it for a human"; exit 0
fi

[ -r "$token_file" ] || { say "status token $token_file is not readable"; exit 1; }
cfg="$(mktemp)"; trap 'rm -f "$cfg"' EXIT; chmod 600 "$cfg"
printf 'header = "Authorization: Bearer %s"\n' "$(cat "$token_file")" >"$cfg"
repo_path="$(sed -E 's#^https?://[^/]+/git/##; s#\.git$##' <<<"$remote")"
gate="$(curl --silent --show-error --max-time 60 --config "$cfg" -H 'Accept: application/json' \
  "$base/api/v3/repos/$repo_path/commits/$main/status" | jq -r '.state // "unknown"')"
if [ "$gate" != success ]; then say "main ${main:0:12} gate is $gate; waiting"; exit 0; fi

cache="$state/jeryu-deploy.git"
[ -d "$cache" ] || git clone -q --bare "$remote" "$cache"
git -C "$cache" fetch -q origin "+refs/heads/main:refs/heads/main"
work="$(mktemp -d)"; trap 'rm -f "$cfg"; rm -rf "$work"' EXIT
git -C "$cache" archive "$main" scripts/release | tar -x -C "$work"

say "staging main ${main:0:12} (gate success)"
if rel="$("$work/scripts/release/stage-release.sh" "$main" | tail -1)" \
   && [[ "$rel" =~ ^prod-[0-9]{8}T[0-9]{6}Z-[0-9a-f]{7}-unsigned$ ]]; then
  printf '%s\t%s\t%s\n' "$main" "$rel" "$(date -u +%FT%TZ)" >>"$state/staged.tsv"
  echo "$rel" >"$state/latest"
  rm -f "$state/failures/$main"
  say "STAGED $rel; deploy with: scripts/release/deploy-release.sh $rel"
else
  echo $((failures + 1)) >"$state/failures/$main"
  say "staging main ${main:0:12} failed (attempt $((failures + 1)) of $max_failures)"
  exit 1
fi
