#!/usr/bin/env bash
# auto-stage.sh — stage the forge's current jeryu-deploy main once its gate is green, so a
# release "go" is only the switch. Run by a timer on the release host (node-0); never deploys.
#
#   1. Resolve main's tip. Nothing to do when it is already live or already staged.
#   2. Wait until the tip's combined status is `success` (the gate ran on the PR head that
#      fast-forwarded main, or on the queue commit that landed it).
#   3. Run stage-release.sh from THAT commit (git archive of scripts/release), so the staging
#      recipe is always the one that was reviewed with the code it stages.
#   4. Record the result. A commit that fails to stage is retried at most twice, then left for a
#      human; a newer main commit starts fresh.
#
#   5. Tell the forge: a `release.staged` or `release.stage_failed` pipeline event
#      (docs/pipeline-events.md), so the attention inbox can say "a release is staged and waiting
#      for a deploy" with the command to run. Best-effort: a failed POST never fails staging.
#
#   6. Every tick ends with one runner heartbeat (`<host>/auto-stage`, label `automation`), so
#      /runners shows the timer alive and the last thing it did: staged a commit, is waiting for
#      main's gate, or failed to stage. Best-effort like the events.
#
# State in $JERYU_AUTO_STAGE_STATE (~/.local/state/jeryu-auto-stage): staged.tsv (sha, release,
# time), failures/<sha> (attempt count), latest (the newest staged release id), waiting (the
# commit whose gate is awaited, and since when), a git cache, logs/<sha12>-attempt<N>.log (the
# staging output, newest 20 kept).
#
# Env: JERYU_DEPLOY_REMOTE, JERYU_BUILD_HOST, JERYU_FORGE_HOST (as the staging
# script; the last two required with no default);
# JERYU_DEPLOY_REPO (owner/name on the forge; default: from the remote);
# JERYU_BASE (https://git.neverhuman.org); JERYU_STATUS_TOKEN_FILE (required, no
# default: the token for the commit-status API and for posting events, which needs
# a global admin or a JERYU_EVENT_REPORTERS login);
# JERYU_AUTO_STAGE_MAX_FAILURES (2); JERYU_AUTO_STAGE_EVENTS=0 turns the events off;
# JERYU_AUTO_STAGE_BEAT=0 turns the heartbeat off.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
remote="${JERYU_DEPLOY_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-deploy.git}"
# A machine name and a credential path are one installation's data, so this
# public source carries no default for either.
build_host="${JERYU_BUILD_HOST:-}"
forge_host="${JERYU_FORGE_HOST:-}"
base="${JERYU_BASE:-https://git.neverhuman.org}"
token_file="${JERYU_STATUS_TOKEN_FILE:-}"
max_failures="${JERYU_AUTO_STAGE_MAX_FAILURES:-2}"
state="${JERYU_AUTO_STAGE_STATE:-$HOME/.local/state/jeryu-auto-stage}"
say() { echo "[auto-stage $(date -u +%H:%M:%S)] $*" >&2; }
die() { say "$*"; exit 64; }
[ -n "$build_host" ] || die "JERYU_BUILD_HOST is unset: set it to this installation's release build host"
[ -n "$forge_host" ] || die "JERYU_FORGE_HOST is unset: set it to this installation's forge host"
[ -n "$token_file" ] || die "JERYU_STATUS_TOKEN_FILE is unset: set it to the status/events credential file"
repo_path="${JERYU_DEPLOY_REPO:-$(sed -E 's#^https?://[^/]+/git/##; s#\.git$##' <<<"$remote")}"

# emit KIND NEEDS_HUMAN EVENT_ID SUMMARY DETAIL_JSON [SECONDS] [REASON] [LOG_FILE]
# Post one pipeline event. The token stays in the curl config file ($cfg) and is never printed.
# event_id makes a retried POST a no-op on the forge. Any failure is logged and ignored.
emit() {
  [ "${JERYU_AUTO_STAGE_EVENTS:-1}" = 1 ] || return 0
  local kind="$1" needs_human="$2" event_id="$3" summary="$4" detail="$5" seconds="${6:-}" \
    reason="${7:-}" log="${8:-}" tail_text="" body
  [ -n "$log" ] && [ -r "$log" ] && tail_text="$(tail -c 12000 "$log")"
  body="$(jq -n --arg kind "$kind" --argjson needs_human "$needs_human" --arg event_id "$event_id" \
    --arg summary "$summary" --argjson detail "$detail" --arg seconds "$seconds" \
    --arg reason "$reason" --arg log_tail "$tail_text" --arg repo "$repo_path" --arg sha "$main" '
    {source: "auto-stage", kind: $kind, event_id: $event_id, needs_human: $needs_human,
     summary: $summary, sha: $sha, detail: $detail, actor: "jeryu-auto-stage"}
    + (if ($repo | test("^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$")) then {repo: $repo} else {} end)
    + (if $seconds != "" then {seconds: ($seconds | tonumber)} else {} end)
    + (if $reason != "" then {reason: $reason} else {} end)
    + (if $log_tail != "" then {log_tail: $log_tail} else {} end)')" || { say "event: could not build $kind (ignored)"; return 0; }
  curl --silent --show-error --max-time 10 --config "$cfg" -H 'Content-Type: application/json' \
    -H 'Accept: application/json' --data-binary @- -o /dev/null "$base/api/v1/events" <<<"$body" \
    || say "event: posting $kind failed (ignored)"
}

# did CONCLUSION SHA FINISHED_AT -> a heartbeat `last`: what this timer most recently did. A
# staging is of a commit, so there is no `pr`.
did() {
  jq -cn --arg conclusion "$1" --arg sha "$2" --arg at "$3" --arg repo "$repo_path" \
    '{repo: $repo, sha: $sha, recipe: "auto-stage", conclusion: $conclusion, seconds: 0, finishedAt: $at}'
}
# beat: one runner heartbeat (POST /api/v1/runners/heartbeat, docs/pipeline-events.md), sent by
# the EXIT trap so idle ticks, early exits and finished work all report. `last` is a wait noted
# this tick, else a failure recorded for main's tip, else the newest staged commit; with no
# history (or a remote that names no owner/name repository) there is no `last`. The token stays
# in $cfg. Best-effort: at most one line, and never the tick's exit status.
beat() {
  [ "${JERYU_AUTO_STAGE_BEAT:-1}" = 1 ] || return 0
  [ -n "$cfg" ] || { say "heartbeat: skipped (status token $token_file is not readable)"; return 0; }
  local host last="$beat_last" sha rel at body
  host="$(hostname -s)" || return 0
  if [ -z "$last" ] && [ -n "$main" ] && [ -s "$state/failures/$main" ]; then
    last="$(did failed "$main" "$(date -u -r "$state/failures/$main" +%FT%TZ)")" || last=""
  elif [ -z "$last" ] && IFS=$'\t' read -r sha rel at < <(tail -n 1 "$state/staged.tsv"); then
    last="$(did staged "$sha" "$at")" || last=""
  fi
  [[ "$repo_path" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || last=""
  body="$(jq -cn --arg host "$host" --argjson last "${last:-null}" '
    {runnerId: ($host + "/auto-stage"), host: $host, slot: 0, labels: ["automation"], intervalSeconds: 300}
    + (if $last != null then {last: $last} else {} end)')" || { say "heartbeat: could not build (ignored)"; return 0; }
  curl --silent --fail --max-time 10 --config "$cfg" -X POST -H 'Content-Type: application/json' \
    -H 'Accept: application/json' --data-binary @- \
    "$base/api/v1/runners/heartbeat" <<<"$body" >/dev/null 2>&1 || say "heartbeat: not accepted (ignored)"
}

mkdir -p "$state/failures"
exec 9>"$state/lock"
flock -n 9 || { say "another run holds the lock"; exit 0; }
touch "$state/staged.tsv"

# The token is read here, before the idle exits, so that every tick can beat; an unreadable token
# only stops a tick where it always did, at the status request.
main="" beat_last="" cfg="" work=""
trap 'beat || true; rm -f "$cfg"; rm -rf "$work"' EXIT
if [ -r "$token_file" ]; then
  cfg="$(mktemp)"; chmod 600 "$cfg"
  printf 'header = "Authorization: Bearer %s"\n' "$(cat "$token_file")" >"$cfg"
fi

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

[ -n "$cfg" ] || { say "status token $token_file is not readable"; exit 1; }
gate="$(curl --silent --show-error --max-time 60 --config "$cfg" -H 'Accept: application/json' \
  "$base/api/v3/repos/$repo_path/commits/$main/status" | jq -r '.state // "unknown"')"
if [ "$gate" != success ]; then
  # The time the wait began is kept across ticks, so the row says since when and not "just now".
  since="$(awk -F'\t' -v sha="$main" '$1 == sha { print $2 }' "$state/waiting" 2>/dev/null)" || since=""
  [ -n "$since" ] || { since="$(date -u +%FT%TZ)"; printf '%s\t%s\n' "$main" "$since" >"$state/waiting"; }
  beat_last="$(did waiting "$main" "$since")" || beat_last=""
  say "main ${main:0:12} gate is $gate; waiting"; exit 0
fi

cache="$state/jeryu-deploy.git"
[ -d "$cache" ] || git clone -q --bare "$remote" "$cache"
git -C "$cache" fetch -q origin "+refs/heads/main:refs/heads/main"
work="$(mktemp -d)"
git -C "$cache" archive "$main" scripts/release | tar -x -C "$work"

# The staging output goes to the journal as before and to a per-attempt log file, whose tail
# rides on the failure event so the reason is readable without shell access to this host.
attempt=$((failures + 1))
mkdir -p "$state/logs"
log="$state/logs/${main:0:12}-attempt$attempt.log"
: >"$log"
say "staging main ${main:0:12} (gate success)"
started="$(date +%s)"
rc=0
# A pipe, not 2> >(tee …): a pipeline waits for its tee, a process substitution does not (even
# after `wait`), so a fast failure was reported with an empty log tail. pipefail keeps the rc.
{ "$work/scripts/release/stage-release.sh" "$main" 2>&1 >"$work/stage.out"; } | tee -a "$log" >&2 || rc=$?
cat "$work/stage.out" >>"$log"
rel="$(tail -1 "$work/stage.out")"
seconds=$(( $(date +%s) - started ))
find "$state/logs" -maxdepth 1 -type f -name '*.log' -printf '%T@ %p\n' | sort -rn | tail -n +21 \
  | cut -d' ' -f2- | xargs -r rm -f

if (( rc == 0 )) && [[ "$rel" =~ ^prod-[0-9]{8}T[0-9]{6}Z-[0-9a-f]{7}-unsigned$ ]]; then
  printf '%s\t%s\t%s\n' "$main" "$rel" "$(date -u +%FT%TZ)" >>"$state/staged.tsv"
  echo "$rel" >"$state/latest"
  rm -f "$state/failures/$main"
  say "STAGED $rel; deploy with: scripts/release/deploy-release.sh $rel"
  emit release.staged false "auto-stage:staged:$rel" \
    "staged $rel from main ${main:0:12}; waiting for a deploy" \
    "$(jq -n --arg release "$rel" --arg previous "${live#jeryu-}" \
      --arg command "scripts/release/deploy-release.sh $rel" \
      '{release: $release, previous_release: $previous, deploy_command: $command}')" "$seconds"
else
  echo "$attempt" >"$state/failures/$main"
  say "staging main ${main:0:12} failed (attempt $attempt of $max_failures)"
  gave_up=false; (( attempt >= max_failures )) && gave_up=true
  emit release.stage_failed "$gave_up" "auto-stage:failed:${main:0:12}:$attempt" \
    "staging main ${main:0:12} failed (attempt $attempt of $max_failures)" \
    "$(jq -n --argjson attempt "$attempt" --argjson max "$max_failures" --argjson rc "$rc" \
      '{attempt: $attempt, max_attempts: $max, exit_code: $rc}')" "$seconds" \
    "stage-release.sh exited $rc; $([ "$gave_up" = true ] && echo 'no further attempt will be made for this commit' || echo 'it will be retried on the next tick')" \
    "$log"
  exit 1
fi
