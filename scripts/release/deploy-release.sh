#!/usr/bin/env bash
# deploy-release.sh RELEASE_ID — switch the forge host to a release staged by
# stage-release.sh, and record it on the forge as a GitHub-shaped deployment of
# jeryu/jeryu-deploy to environment "production":
#
#   1. POST /repos/jeryu/jeryu-deploy/deployments (sha, payload: release id,
#      previous release, binary digest, the pinned jeryu-web commit and dist
#      hash, host), then an in_progress status.
#   2. Run the staged switch.sh on the forge host.
#   3. Append success, or failure (switch.sh prints its own rollback command).
#      The forge's auto_inactive retires the previous production deployment.
#   4. After a success, refresh every family's release board, because the forge
#      that just started holds none: wait for its health endpoint (up to
#      JERYU_BOARD_HEALTH_TRIES tries, default 60, one a second) so the push is
#      not lost on a starting forge, then run the collector
#      (JERYU_RELEASE_BOARD, default ~/.local/share/jeryu-release-board/collect.sh)
#      under a JERYU_BOARD_REFRESH_TIMEOUT (default 300s) limit. The wait and the
#      collector's output go to a 0600 log (newest 30) in
#      ~/.local/state/jeryu-release-board/logs/ (JERYU_RELEASE_BOARD_STATE
#      overrides the directory), and a receipt says whether the refresh landed.
#      None of this can fail the deploy: the release is already live.
#
# Everything switch.sh prints is shown live and kept in
# ~/.local/state/jeryu-release/logs/<release>-<UTC stamp>.log (0600, newest 30;
# JERYU_RELEASE_LOG_DIR overrides the directory). Both statuses carry the log's
# path (log_path); a failure's description adds the log's last meaningful line
# and the status carries its last 20 lines (log_tail), which the forge puts in
# the deploy.status event's detail. Both statuses also carry log_url, the
# deploy.status events on the forge where that tail can be read
# (JERYU_RELEASE_LOG_URL overrides it).
#
# Deploying the release production already runs is a no-op: the script says
# "already live" and exits 0, recording no deployment and no failure.
#
# The record never decides the deploy. If the forge that is live before the
# switch cannot record it (for instance the release that introduces the
# deployments API), it is recorded right after the switch instead; if it still
# cannot be, the switch stands and the script says so.
#
# Recording needs an admin token: JERYU_DEPLOY_TOKEN_FILE (default
# ~/.config/jeryu/credentials/git-neverhuman-org-alton2.pat). It is read into a
# 0600 curl config, never put on argv. Env: JERYU_BUILD_HOST (xbabe2),
# JERYU_FORGE_HOST (atomicsoul), JERYU_FORGE_URL (https://git.neverhuman.org).
#
# --dry-run reads the staged metadata and prints the deployment it would record,
# then stops: nothing is recorded, switched or logged.
# --json prints exactly one JSON line on stdout (switch output and receipts go
# to stderr): {"release","deployment_id","log_path","dry_run","already_live",
# "board_refresh":{"state","log_path"}} on success, where state is pushed,
# failed, timeout, unhealthy (the forge never answered) or skipped (no collector
# installed); the same line without board_refresh and with null ids on an
# already live release (nothing switched, so no board to refresh);
# {"release","deployment","dry_run":true} on a dry run, or the API's error
# envelope {"code","message","exit_code"} on a refusal or a failed switch.
# Exit codes: 0 live, 64 usage (bad argument), 65 state (the release is not
# staged, or its metadata is incomplete), 69 unreachable (the hosts did not
# answer), 77 credential (the deploy token is not readable), 1 anything else.
# A failed switch exits with switch.sh's own code (envelope code switch_failed).
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
json=0; dry_run=0; args=()
[[ " $* " != *" --json "* ]] || json=1
exec 3>&1
envelope() { # CODE MESSAGE EXIT — the API's error envelope, on stdout under --json
  [[ $json == 0 ]] || jq -cn --arg c "$1" --arg m "$2" --argjson e "$3" '{code:$c, message:$m, exit_code:$e}' >&3
}
refuse() { # CLASS MESSAGE — exit with the class's code
  local code
  case "$1" in usage) code=64 ;; state) code=65 ;; unreachable) code=69 ;; credential) code=77 ;; *) code=1 ;; esac
  echo "$2" >&2
  envelope "$1" "$2" "$code"
  exit "$code"
}
for a in "$@"; do
  case "$a" in
    --json) ;;
    --dry-run) dry_run=1 ;;
    -*) refuse usage "unknown option '$a'" ;;
    *) args+=("$a") ;;
  esac
done
set -- ${args[@]+"${args[@]}"}
(($# == 1)) || refuse usage "usage: deploy-release.sh [--json] [--dry-run] RELEASE_ID"
[[ $json == 0 ]] || exec 1>&2
rel="$1"
[[ "$rel" =~ ^prod-[0-9]{8}T[0-9]{6}Z-[0-9a-f]+-unsigned$ ]] || refuse usage "not a release id: $rel"
build_host="${JERYU_BUILD_HOST:-xbabe2}"
forge_host="${JERYU_FORGE_HOST:-atomicsoul}"
forge="${JERYU_FORGE_URL:-https://git.neverhuman.org}"
token_file="${JERYU_DEPLOY_TOKEN_FILE:-$HOME/.config/jeryu/credentials/git-neverhuman-org-alton2.pat}"
repo=/repos/jeryu/jeryu-deploy

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
umask 077
[[ -r "$token_file" ]] || refuse credential "deploy token $token_file is not readable"
printf 'header = "Authorization: Bearer %s"\n' "$(cat "$token_file")" >"$tmp/curl.cfg"
api() { # METHOD PATH [JSON-FILE]
  local args=(--silent --show-error --max-time 60 -X "$1" --config "$tmp/curl.cfg" -H 'Accept: application/json')
  [[ -z "${3:-}" ]] || args+=(-H 'Content-Type: application/json' --data "@$3")
  curl "${args[@]}" "$forge$2"
}

meta="$(ssh "$build_host" "ssh -n $forge_host 'set -e; d=~/.jeryu/incoming/$rel; cat \$d/RELEASE.txt; echo binary_sha256=\$(sha256sum \$d/bundle/jeryu | cut -c1-64); echo live=\$(readlink ~/.jeryu/bin/jeryu)'")" || {
  rc=$?
  [[ $rc != 255 ]] || refuse unreachable "cannot reach $forge_host through $build_host"
  refuse state "$rel is not staged on $forge_host"
}
field() { sed -n "s/^$1=//p" <<<"$meta" | head -1; }
sha="$(field jeryu_deploy_commit | cut -d' ' -f1)"
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || refuse state "staged RELEASE.txt has no 40-hex jeryu_deploy_commit"

# Redeploying what production already runs changes nothing, so it is a no-op:
# nothing is recorded, switched or logged, and no failed deployment is left
# behind for the inbox to call a broken pipeline.
if [[ "$(field live)" == "jeryu-$rel" ]]; then
  echo "[deploy] $rel is already live on $forge_host; nothing to do" >&2
  [[ $json == 0 ]] || jq -cn --arg rel "$rel" --argjson dry "$dry_run" \
    '{release:$rel, deployment_id:null, log_path:null, dry_run:($dry == 1), already_live:true}' >&3
  exit 0
fi

# Empty for a release staged before jeryu-web was pinned by commit.
web_commit="$(field jeryu_web_commit)"; web_sha="$(field web_dist_sha256)"

jq -n --arg sha "$sha" --arg rel "$rel" --arg prev "$(field rollback_target)" \
  --arg bin "$(field binary_sha256)" --arg live "$(field live)" --arg host "$forge_host" \
  --arg web "$web_commit" --arg web_sha "$web_sha" \
  '{sha:$sha, ref:"main", environment:"production", description:("release " + $rel),
    payload:{release:$rel, previous_release:$prev, previous_binary:$live, binary_sha256:$bin,
             jeryu_web_commit:$web, web_dist_sha256:$web_sha, host:$host, signed:false}}' >"$tmp/deployment.json"

if [[ $dry_run == 1 ]]; then
  echo "[dry-run] would switch $forge_host to $rel (${sha:0:12}, rollback target $(field rollback_target)); nothing recorded or switched" >&2
  [[ $json == 0 ]] || jq -c --arg rel "$rel" '{release:$rel, deployment:., dry_run:true}' "$tmp/deployment.json" >&3
  exit 0
fi

record() {
  local created
  created="$(api POST "$repo/deployments" "$tmp/deployment.json" || true)"
  deployment_id="$(jq -er '.id | select(type == "number")' <<<"$created" 2>/dev/null)" || {
    deployment_id=""
    echo "[receipt] not recorded: $(jq -r '.message // "no JSON"' <<<"$created" 2>/dev/null | head -c 120)"
    return 0
  }
  echo "[receipt] deployment $deployment_id recorded for $rel at ${sha:0:12}"
}
status() { # STATE DESCRIPTION [LOG_PATH [LOG_TAIL]]
  [[ -n "$deployment_id" ]] || return 0
  jq -n --arg s "$1" --arg d "$2" --arg url "$forge" --arg log "$log_url" --arg path "${3:-}" --arg tail "${4:-}" \
    '{state:$s, description:$d, environment_url:$url, log_url:$log}
     + (if $path != "" then {log_path:$path} else {} end)
     + (if $tail != "" then {log_tail:$tail} else {} end)' >"$tmp/status.json"
  api POST "$repo/deployments/$deployment_id/statuses" "$tmp/status.json" \
    | jq -r '"[receipt] status \(.state // "not recorded: \(.message // "?")")"'
}

# The forge that just started keeps release boards in memory, so it has none: refresh every
# family now (scripts/release-board/) rather than leave /releases empty until the next timer
# tick. The push has to reach the forge that was just switched, so wait for its health endpoint
# first — a push sent while it is still starting is lost. The wait and everything the collector
# prints are kept in a log beside the boards, and the outcome becomes a receipt line, so a
# refresh that did not land is visible in the deploy instead of only in an empty page.
board_state=skipped board_log=""
refresh_boards() {
  local board dir health tries wait i rc
  board="${JERYU_RELEASE_BOARD:-$HOME/.local/share/jeryu-release-board/collect.sh}"
  if [[ ! -x "$board" ]]; then
    echo "[receipt] no board collector at $board; /releases fills at the next timer tick"
    return 0
  fi
  dir="${JERYU_RELEASE_BOARD_STATE:-$HOME/.local/state/jeryu-release-board}/logs"
  mkdir -p "$dir"; chmod 700 "$dir"
  board_log="$dir/refresh-$rel-$(date -u +%Y%m%dT%H%M%SZ).log"
  : >"$board_log"; chmod 600 "$board_log"
  find "$dir" -maxdepth 1 -type f -name 'refresh-*.log' -printf '%T@ %p\n' | sort -rn | tail -n +31 \
    | cut -d' ' -f2- | xargs -r rm -f --
  health="${JERYU_FORGE_HEALTH_URL:-$forge/health}"
  tries="${JERYU_BOARD_HEALTH_TRIES:-60}"
  [[ "$tries" =~ ^[1-9][0-9]*$ ]] || tries=60
  board_state=unhealthy
  for ((i = 1; i <= tries; i++)); do
    if curl -fsS --max-time 5 -o /dev/null "$health" 2>>"$board_log"; then board_state=healthy; break; fi
    ((i == tries)) || sleep 1
  done
  if [[ "$board_state" != healthy ]]; then
    echo "[refresh] $health never answered in $tries tries" >>"$board_log"
    echo "[receipt] boards not refreshed: $health never answered in $tries tries; log $board_log"
    return 0
  fi
  echo "[refresh] $health answered on try $i; collecting every family" >>"$board_log"
  wait="${JERYU_BOARD_REFRESH_TIMEOUT:-300}"
  rc=0
  timeout "$wait" "$board" --push --trigger release all >>"$board_log" 2>&1 || rc=$?
  case $rc in
    0) board_state=pushed
       echo "[receipt] boards refreshed: $(grep -c ': pushed' "$board_log" || true) pushed; log $board_log" ;;
    124) board_state=timeout
       echo "[receipt] board refresh timed out after ${wait}s; log $board_log" ;;
    *) board_state=failed
       echo "[receipt] board refresh exited $rc; log $board_log" ;;
  esac
}

# Where a reader can see what switch.sh printed: the deploy.status events, whose
# detail carries the status's log tail and the log's path on the release host.
log_url="${JERYU_RELEASE_LOG_URL:-$forge/api/v1/events?kind=deploy.status&repo=jeryu/jeryu-deploy}"
log_dir="${JERYU_RELEASE_LOG_DIR:-$HOME/.local/state/jeryu-release/logs}"
mkdir -p "$log_dir"; chmod 700 "$log_dir"
log="$log_dir/$rel-$(date -u +%Y%m%dT%H%M%SZ).log"
: >"$log"; chmod 600 "$log"
find "$log_dir" -maxdepth 1 -type f -name '*.log' -printf '%T@ %p\n' | sort -rn | tail -n +31 \
  | cut -d' ' -f2- | xargs -r rm -f --
# The log without colour codes or blank lines, and never the deploy token.
clean_log() {
  local text
  text="$(sed -E $'s/\x1b\\[[0-9;?]*[A-Za-z]//g; s/\r//g' "$log" | grep -v '^[[:space:]]*$' || true)"
  printf '%s\n' "${text//"$(cat "$token_file")"/[redacted]}"
}

deployment_id=""
record
status in_progress "switching $forge_host to $rel"
set +e
ssh "$build_host" "ssh $forge_host 'bash ~/.jeryu/incoming/$rel/switch.sh'" 2>&1 | tee -a "$log"
rc=${PIPESTATUS[0]}
set -e
echo "[receipt] switch output kept in $log"
if [[ $rc == 0 ]]; then
  if [[ -z "$deployment_id" ]]; then
    echo "[receipt] retrying against the forge that just started"
    record
  fi
  status success "live on $forge_host" "$log"
  refresh_boards
  [[ $json == 0 ]] || jq -cn --arg rel "$rel" --arg id "$deployment_id" --arg log "$log" \
    --arg state "$board_state" --arg blog "$board_log" \
    '{release:$rel, deployment_id:($id | tonumber? // null), log_path:$log, dry_run:false,
      already_live:false,
      board_refresh:{state:$state, log_path:(if $blog == "" then null else $blog end)}}' >&3
else
  why="$(clean_log | grep -v 'rollback: bash ' | tail -n 1 | cut -c1-300)"
  status failure "switch.sh exited $rc${why:+: $why}" "$log" "$(clean_log | tail -n 20)"
  envelope switch_failed "switch.sh exited $rc${why:+: $why}; log $log" "$rc"
  exit "$rc"
fi
