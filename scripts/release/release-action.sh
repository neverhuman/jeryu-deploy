#!/usr/bin/env bash
# release-action.sh — the release steps a person asks for, run on the release host: by hand from
# any directory, or by the jeryu-release-actions timer when someone presses the button on the
# forge's attention inbox (GET /api/v1/release-actions, docs/release-actions.md).
#
#   release-action.sh deploy RELEASE_ID [--dry-run]
#       Switch production using deploy-release.sh from the immutable staged commit
#       (git archive of scripts/release, the recipe the release README says to run), after
#       waiting for auto-stage to finish if it is staging (its lock), so the two never share the
#       release build directory.
#   release-action.sh auto-pin
#       Start jeryu-auto-pin.service now and wait for it.
#   release-action.sh auto-pin-retry HEAD
#       Forget auto-pin's failure count for one 40-hex jeryu-web head, then start it.
#   release-action.sh poll
#       One tick: advertise this repository's poller with a deployer-authenticated check-in,
#       read pending asks, claim the oldest for this repository, run it and report a redacted tail.
#
# The forge never sends a command. An ask names one of three actions and an argument, and this
# script checks both again and maps each to the fixed steps above; anything else is refused. The
# deploy restarts the forge, so the result is retried for a while and a forge that forgot the
# ask (it keeps asks in memory) answers 404, which is fine: deploy-release.sh's deployment
# record is the durable answer.
#
# Settings: every unset JERYU_* setting is filled from $JERYU_RELEASE_ENV
# (~/.config/jeryu/release.env, KEY=VALUE lines, never run as shell), the same file the
# release timers load, so no shell needs `set -a; . release.env` first. deploy-release.sh's own
# settings apply (JERYU_BUILD_HOST, JERYU_FORGE_HOST, JERYU_DEPLOY_TOKEN_FILE, JERYU_HEALTH_URL).
# The poll reads and claims asks with JERYU_DEPLOY_TOKEN_FILE: the forge serves them only to a
# deployer (an admin named in JERYU_DEPLOYERS). JERYU_FORGE_URL (https://git.neverhuman.org);
# JERYU_DEPLOY_REMOTE (jeryu-deploy's clone URL); JERYU_DEPLOY_REPO (owner/name; default from the
# remote); JERYU_RELEASE_ACTIONS_STATE (~/.local/state/jeryu-release-actions: git cache, logs);
# JERYU_AUTO_STAGE_STATE and JERYU_AUTO_PIN_STATE as those timers use them.
# Exit codes: 0 done, 64 usage, otherwise the step's own exit code.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help|"") awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail

env_file="${JERYU_RELEASE_ENV:-$HOME/.config/jeryu/release.env}"
if [ -r "$env_file" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    [[ "$line" =~ ^[[:space:]]*(JERYU_[A-Za-z0-9_]*)=(.*)$ ]] || continue
    key="${BASH_REMATCH[1]}" value="${BASH_REMATCH[2]}"
    value="${value%\"}"; value="${value#\"}"; value="${value%\'}"; value="${value#\'}"
    [ -n "${!key+set}" ] || export "$key=$value"
  done <"$env_file"
fi

forge="${JERYU_FORGE_URL:-https://git.neverhuman.org}"
remote="${JERYU_DEPLOY_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-deploy.git}"
repo_path="${JERYU_DEPLOY_REPO:-$(sed -E 's#^https?://[^/]+/git/##; s#\.git$##' <<<"$remote")}"
state="${JERYU_RELEASE_ACTIONS_STATE:-$HOME/.local/state/jeryu-release-actions}"
stage_lock="${JERYU_AUTO_STAGE_STATE:-$HOME/.local/state/jeryu-auto-stage}/lock"
pin_failures="${JERYU_AUTO_PIN_STATE:-$HOME/.local/state/jeryu-auto-pin}/failures"
pin_unit="${JERYU_AUTO_PIN_UNIT:-jeryu-auto-pin.service}"
release_re='^prod-[0-9]{8}T[0-9]{6}Z-[0-9a-f]{7,40}-unsigned$'
say() { echo "[release-action $(date -u +%H:%M:%S)] $*" >&2; }
usage() { say "$*"; exit 64; }
mkdir -p "$state/logs"
chmod 700 "$state"

# deploy RELEASE [--dry-run]
deploy() {
  local rel="${1:-}" dry="${2:-}" work rc revision short
  [[ "$rel" =~ $release_re ]] || { say "not a release id: '$rel'"; return 64; }
  [[ -z "$dry" || "$dry" == --dry-run ]] || { say "deploy takes only --dry-run after the release id"; return 64; }
  # Hold before touching the shared cache and through the whole deployment.
  # An expired forge claim does not allow another tick to overlap this host step.
  exec 9>"$state/deploy.lock"
  flock -n 9 || { say "another deployment is still running on this host"; return 75; }
  # The ancestry check in deploy-release.sh needs jeryu-deploy's history; keep a cache of it.
  [ -d "$state/git" ] || git init -q --bare "$state/git"
  git -C "$state/git" fetch -q "$remote" "+refs/heads/main:refs/heads/main"
  work="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$work'" RETURN
  short="${rel%-unsigned}"; short="${short##*-}"
  revision="$(git -C "$state/git" rev-parse --verify "$short^{commit}")" || {
    say "release commit is missing or ambiguous in protected main's history"; return 65;
  }
  git -C "$state/git" merge-base --is-ancestor "$revision" main || {
    say "release commit is not on protected main"; return 65;
  }
  git -C "$state/git" archive "$revision" scripts/release | tar -x -C "$work"
  # auto-stage builds in a shared directory; wait for a staging in progress (up to 45 min, its
  # own limit) rather than race it.
  mkdir -p "$(dirname "$stage_lock")"
  exec 8>"$stage_lock"
  say "waiting for auto-stage to be idle"
  flock -w 2700 8 || { say "auto-stage held its lock for 45 minutes; not deploying"; return 75; }
  say "deploying $rel with deploy-release.sh from staged commit ${revision:0:12}"
  rc=0
  JERYU_DEPLOY_GIT="$state/git" bash "$work/scripts/release/deploy-release.sh" ${dry:+"$dry"} "$rel" || rc=$?
  exec 8>&-
  return "$rc"
}

auto_pin() {
  say "starting $pin_unit"
  systemctl --user start "$pin_unit"
  systemctl --user status "$pin_unit" --no-pager -n 20 || true
}

auto_pin_retry() {
  local head="${1:-}"
  [[ "$head" =~ ^[0-9a-f]{40}$ ]] || { say "not a 40-hex head: '$head'"; return 64; }
  rm -f "$pin_failures/$head"
  say "forgot auto-pin's failures for $head"
  auto_pin
}

# --- poll: one timer tick ---
cfg=""
api() { # METHOD PATH [JSON] -> body on stdout; fails on a non-2xx answer
  local args=(--silent --show-error --fail-with-body --max-time 30 -X "$1" --config "$cfg"
    -H 'Accept: application/json')
  [ -z "${3:-}" ] || args+=(-H 'Content-Type: application/json' --data-binary "$3")
  curl "${args[@]}" "$forge$2"
}

beat() { # deployer-authenticated repository-specific poller check-in
  local body
  body="$(jq -cn --arg repo "$repo_path" '{repo: $repo}')"
  api POST /api/v1/release-actions/poller "$body" >/dev/null 2>&1 || say "poller check-in: not accepted (ignored)"
}

report() { # ID OK MESSAGE LOG — retried while a deploy restarts the forge
  local id="$1" body code
  body="$(jq -cn --argjson ok "$2" --arg message "$3" --rawfile log "$4" \
    -f "$(dirname "${BASH_SOURCE[0]}")/redact-release-result.jq")"
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12; do
    code="$(curl --silent --max-time 30 -o /dev/null -w '%{http_code}' -X POST --config "$cfg" \
      -H 'Content-Type: application/json' --data-binary "$body" \
      "$forge/api/v1/release-actions/$id/result")" || code=000
    case "$code" in
      2??) return 0 ;;
      404) say "the forge no longer knows $id (it restarted); the deployment record stands"; return 0 ;;
      409) say "the forge refused the result for $id (not claimed)"; return 0 ;;
    esac
    sleep 10
  done
  say "could not report the result of $id (last answer $code)"
}

poll() {
  local token_file="${JERYU_DEPLOY_TOKEN_FILE:-}" asks ask id action repo release head log rc ok msg
  [ -n "$token_file" ] || usage "JERYU_DEPLOY_TOKEN_FILE is unset: set it in $env_file"
  [ -r "$token_file" ] || { say "deploy token $token_file is not readable"; exit 77; }
  umask 077
  cfg="$(mktemp)"
  # shellcheck disable=SC2064
  trap "rm -f '$cfg'" EXIT
  printf 'header = "Authorization: Bearer %s"\n' "$(cat "$token_file")" >"$cfg"
  beat
  asks="$(api GET '/api/v1/release-actions?state=pending')" || { say "could not read the asks"; return 0; }
  # One ask per tick, oldest first: a deploy restarts the forge, and the next tick reads afresh.
  ask="$(jq -c --arg repo "$repo_path" '.actions | map(select(.repo == $repo)) | sort_by(.requested_at) | .[0] // empty' <<<"$asks")"
  [ -n "$ask" ] || return 0
  id="$(jq -r .id <<<"$ask")" action="$(jq -r .action <<<"$ask")" repo="$(jq -r .repo <<<"$ask")"
  release="$(jq -r '.release // ""' <<<"$ask")" head="$(jq -r '.head // ""' <<<"$ask")"
  [[ "$id" =~ ^ra-[0-9A-Za-z]+-[0-9]+$ ]] || { say "ignoring an ask with a malformed id"; return 0; }
  if [ "$repo" != "$repo_path" ]; then
    say "ignoring $id: it is for $repo, and this host releases $repo_path"
    return 0
  fi
  api POST "/api/v1/release-actions/$id/claim" >/dev/null 2>&1 || { say "$id was claimed elsewhere"; return 0; }
  log="$state/logs/$id.log"
  say "claimed $id: $action ${release}${head} (asked by $(jq -r .requested_by <<<"$ask"))"
  rc=0
  case "$action" in
    deploy) deploy "$release" >"$log" 2>&1 || rc=$? ;;
    auto_pin) auto_pin >"$log" 2>&1 || rc=$? ;;
    auto_pin_retry) auto_pin_retry "$head" >"$log" 2>&1 || rc=$? ;;
    *) echo "unknown action '$action'; this host runs deploy, auto_pin and auto_pin_retry" >"$log"; rc=64 ;;
  esac
  ok=true; ((rc == 0)) || ok=false
  msg="$(grep -v '^[[:space:]]*$' "$log" | tail -n 1 | cut -c1-400)"
  ((rc == 0)) || msg="exit $rc: $msg"
  say "$id finished: exit $rc (details kept in host log)"
  report "$id" "$ok" "$msg" "$log"
  find "$state/logs" -maxdepth 1 -type f -name '*.log' -printf '%T@ %p\n' | sort -rn | tail -n +31 \
    | cut -d' ' -f2- | xargs -r rm -f
}

sub="$1"; shift
case "$sub" in
  deploy) deploy "$@" ;;
  auto-pin) (($# == 0)) || usage "auto-pin takes no argument"; auto_pin ;;
  auto-pin-retry) (($# == 1)) || usage "auto-pin-retry takes the 40-hex head"; auto_pin_retry "$1" ;;
  poll) (($# == 0)) || usage "poll takes no argument"; poll ;;
  *) usage "unknown step '$sub': deploy, auto-pin, auto-pin-retry or poll" ;;
esac
