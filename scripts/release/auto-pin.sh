#!/usr/bin/env bash
# auto-pin.sh — when jeryu-web main is green and newer than the pin in jeryu-deploy main's
# jeryu-split.lock.toml, open the pull request that bumps the pin. Run by a timer on the release
# host (xbabe0). A bump is mechanical (build the dist, hash it, change two lock fields), and until
# now it happened only when somebody remembered, so merged UI work sat unreleasable.
#
# It only proposes. Review and landing belong to pr-redteam and the merge queue, staging to
# auto-stage.sh, the deploy to a person. It never merges, approves, stages or deploys.
#
#   1. Resolve jeryu-deploy main and jeryu-web main. Nothing to do when the pin is the web head.
#   2. Nothing to do when the head does not descend from the pin (a human rewrote history), when
#      any bump pull request for jeryu-web is open (its landing moves the pin; the next tick
#      proposes whatever is newer), when this head already has its branch and pull request, or
#      when the head's combined status is not `success`.
#   3. Build the dist with build-web-dist.sh taken from jeryu-deploy main itself (git archive), so
#      the recipe is always the reviewed one. It prints "<commit> <web_dist_sha256>".
#   4. On branch auto/pin-web-<sha12> cut from main: set exactly the two lock fields, commit as
#      alton2 with the web commits the bump ships, push, open the pull request, and post a
#      `pin.bump_opened` pipeline event (docs/pipeline-events.md).
#   5. A head that fails is retried once; then `pin.bump_failed` asks for a human and the head is
#      left alone. A newer head starts fresh. A branch pushed without its pull request (the API
#      call failed) gets the pull request on the next tick, without rebuilding.
#
#   6. Every tick ends with one runner heartbeat (`<host>/auto-pin`, label `automation`), so
#      /runners shows the timer alive and the last thing it did: opened a bump, is waiting behind
#      one or for the web gate, or failed on a head. Best-effort: it never fails or delays a tick.
#
# State in $JERYU_AUTO_PIN_STATE (~/.local/state/jeryu-auto-pin): opened.tsv (web sha, PR, time),
# failures/<web sha>, waiting (what the newest wait is for, and since when), git caches, web-dist/
# (build-web-dist.sh's OUT_ROOT), logs/ (newest 20).
#
# Env: JERYU_DEPLOY_REMOTE, JERYU_WEB_REMOTE (git URLs; pushing uses this host's git credential);
# JERYU_DEPLOY_REPO, JERYU_WEB_REPO (owner/name on the forge; default: from the remotes);
# JERYU_BASE (must be https://git.neverhuman.org: the token is sent nowhere else);
# JERYU_PIN_TOKEN_FILE (the pull request author's token, also used for the status API and the
# events; default ~/.config/jeryu/credentials/git-neverhuman-org-alton2.pat; a regular 0600 file);
# JERYU_PIN_GIT_NAME / JERYU_PIN_GIT_EMAIL (alton2 / alton@veox.ai);
# JERYU_AUTO_PIN_MAX_FAILURES (2); JERYU_AUTO_PIN_EVENTS=0 turns the events off;
# JERYU_AUTO_PIN_BEAT=0 turns the heartbeat off.
set -euo pipefail
remote="${JERYU_DEPLOY_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-deploy.git}"
web_remote="${JERYU_WEB_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-web.git}"
base="${JERYU_BASE:-https://git.neverhuman.org}"; base="${base%/}"
token_file="${JERYU_PIN_TOKEN_FILE:-$HOME/.config/jeryu/credentials/git-neverhuman-org-alton2.pat}"
git_name="${JERYU_PIN_GIT_NAME:-alton2}" git_email="${JERYU_PIN_GIT_EMAIL:-alton@veox.ai}"
max_failures="${JERYU_AUTO_PIN_MAX_FAILURES:-2}"
state="${JERYU_AUTO_PIN_STATE:-$HOME/.local/state/jeryu-auto-pin}"
lock_file=jeryu-split.lock.toml
say() { echo "[auto-pin $(date -u +%H:%M:%S)] $*" >&2; }
repo_of() { sed -E 's#^https?://[^/]+/git/##; s#\.git$##' <<<"$1"; }
repo_path="${JERYU_DEPLOY_REPO:-$(repo_of "$remote")}"
web_path="${JERYU_WEB_REPO:-$(repo_of "$web_remote")}"

[[ "$base" == https://git.neverhuman.org ]] || { say "refusing a noncanonical credential origin"; exit 2; }
for path in "$repo_path" "$web_path"; do
  [[ "$path" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || { say "not an owner/name repository: $path"; exit 2; }
done

mkdir -p "$state/failures" "$state/logs"
exec 9>"$state/lock"
flock -n 9 || { say "another run holds the lock"; exit 0; }
touch "$state/opened.tsv"

# The token reaches curl through a 0600 config file, never through argv, and is never printed.
# It is read here, before the idle exits, so that every tick can beat; a bad token file only stops
# a tick where it always did, at the first forge request.
work="$(mktemp -d)"; cfg="$work/curl.cfg"; : >"$cfg"; chmod 600 "$cfg"
head="" beat_last="" token_problem=""
trap 'beat || true; rm -rf "$work"' EXIT
if [[ -f "$token_file" && ! -L "$token_file" && "$(stat -c '%a' "$token_file")" == 600 ]]; then
  token="$(cat "$token_file")"
  if [[ "$token" =~ ^[A-Za-z0-9._~+/-]+=*$ ]]; then printf 'header = "Authorization: Bearer %s"\n' "$token" >"$cfg"
  else token_problem="the token has an invalid bearer-token shape"; fi
  unset token
else
  token_problem="the token must be a regular mode-0600 file"
fi

# did CONCLUSION PR SHA FINISHED_AT -> a heartbeat `last`: what this timer most recently did.
did() {
  jq -cn --arg conclusion "$1" --arg pr "$2" --arg sha "$3" --arg at "$4" --arg repo "$repo_path" '
    {repo: $repo, sha: $sha, recipe: "auto-pin", conclusion: $conclusion, seconds: 0, finishedAt: $at}
    + (if $pr != "" then {pr: ($pr | tonumber)} else {} end)'
}
# waiting_for PR: this head waits (behind bump #PR, or for its own gate when PR is empty). The
# time the wait began is kept across ticks, so the row says since when and not "just now".
waiting_for() {
  local key="$head ${1:-gate}" since
  since="$(awk -F'\t' -v key="$key" '$1 == key { print $2 }' "$state/waiting" 2>/dev/null)" || since=""
  [ -n "$since" ] || { since="$(date -u +%FT%TZ)"; printf '%s\t%s\n' "$key" "$since" >"$state/waiting"; }
  beat_last="$(did waiting "${1:-}" "$head" "$since")" || beat_last=""
}
# beat: one runner heartbeat (POST /api/v1/runners/heartbeat, docs/pipeline-events.md), sent by
# the EXIT trap so idle ticks, early exits and finished work all report. `last` is a wait noted
# this tick, else a failure recorded for the current head, else the newest opened bump; with no
# history there is no `last`. Best-effort: at most one line, and never the tick's exit status.
beat() {
  [ "${JERYU_AUTO_PIN_BEAT:-1}" = 1 ] || return 0
  [ -z "$token_problem" ] || { say "heartbeat: skipped ($token_problem)"; return 0; }
  local host last="$beat_last" sha pr at body
  host="$(hostname -s)" || return 0
  if [ -z "$last" ] && [ -n "$head" ] && [ -s "$state/failures/$head" ]; then
    last="$(did failed "" "$head" "$(date -u -r "$state/failures/$head" +%FT%TZ)")" || last=""
  elif [ -z "$last" ] && IFS=$'\t' read -r sha pr at < <(tail -n 1 "$state/opened.tsv"); then
    last="$(did opened "$pr" "$sha" "$at")" || last=""
  fi
  body="$(jq -cn --arg host "$host" --argjson last "${last:-null}" '
    {runnerId: ($host + "/auto-pin"), host: $host, slot: 0, labels: ["automation"], intervalSeconds: 300}
    + (if $last != null then {last: $last} else {} end)')" || { say "heartbeat: could not build (ignored)"; return 0; }
  curl --silent --fail --max-time 10 --config "$cfg" -X POST -H 'Accept: application/json' \
    -H 'Content-Type: application/json' --data-binary @- \
    "$base/api/v1/runners/heartbeat" <<<"$body" >/dev/null 2>&1 || say "heartbeat: not accepted (ignored)"
}

main="$(git ls-remote "$remote" refs/heads/main | cut -f1)"
head="$(git ls-remote "$web_remote" refs/heads/main | cut -f1)"
[[ "$main" =~ ^[0-9a-f]{40}$ ]] || { say "could not resolve jeryu-deploy main ($main)"; exit 1; }
[[ "$head" =~ ^[0-9a-f]{40}$ ]] || { say "could not resolve jeryu-web main ($head)"; exit 1; }

cache="$state/jeryu-deploy.git"
[ -d "$cache" ] || git clone -q --bare "$remote" "$cache"
git -C "$cache" fetch -q "$remote" "+refs/heads/main:refs/heads/main"
# The jeryu-web [[repo]] block of the lock, as build-web-dist.sh reads it.
pin="$(git -C "$cache" show "$main:$lock_file" | awk '
  /^\[\[repo\]\]/ { inweb = 0 }
  /^name = "jeryu-web"$/ { inweb = 1 }
  inweb && $1 == "commit" && $2 == "=" { gsub(/"/, "", $3); print $3; exit }')"
[[ "$pin" =~ ^[0-9a-f]{40}$ ]] || { say "$lock_file at main ${main:0:12} has no 40-hex jeryu-web commit ($pin)"; exit 1; }
[ "$pin" != "$head" ] || exit 0

web_cache="$state/jeryu-web.git"
[ -d "$web_cache" ] || git clone -q --bare "$web_remote" "$web_cache"
git -C "$web_cache" fetch -q "$web_remote" "+refs/heads/main:refs/heads/main"
if ! git -C "$web_cache" merge-base --is-ancestor "$pin" "$head" 2>/dev/null; then
  say "jeryu-web main ${head:0:12} does not descend from the pin ${pin:0:12}; leaving it for a human"; exit 0
fi
failures="$(cat "$state/failures/$head" 2>/dev/null || echo 0)"
if (( failures >= max_failures )); then
  say "bumping to ${head:0:12} failed $failures times; leaving it for a human"; exit 0
fi

[ -z "$token_problem" ] || { say "$token_problem"; exit 2; }
api() { # METHOD PATH [JSON on stdin] -> body on stdout
  local method="$1" path="$2"; shift 2
  curl --silent --show-error --fail --max-time 60 --config "$cfg" -X "$method" \
    -H 'Accept: application/json' -H 'Content-Type: application/json' "$@" "$base$path"
}

# emit KIND NEEDS_HUMAN EVENT_ID SUMMARY DETAIL_JSON [PR] [REASON] [LOG_FILE]
# One pipeline event; event_id makes a repeat a no-op on the forge. Best-effort: logged, ignored.
emit() {
  [ "${JERYU_AUTO_PIN_EVENTS:-1}" = 1 ] || return 0
  local kind="$1" needs_human="$2" event_id="$3" summary="$4" detail="$5" pr="${6:-}" \
    reason="${7:-}" log="${8:-}" tail_text="" body
  [ -n "$log" ] && [ -r "$log" ] && tail_text="$(tail -c 12000 "$log")"
  body="$(jq -n --arg kind "$kind" --argjson needs_human "$needs_human" --arg event_id "$event_id" \
    --arg summary "$summary" --argjson detail "$detail" --arg pr "$pr" --arg reason "$reason" \
    --arg log_tail "$tail_text" --arg repo "$repo_path" --arg sha "$head" '
    {source: "auto-pin", kind: $kind, event_id: $event_id, needs_human: $needs_human,
     summary: $summary, repo: $repo, sha: $sha, detail: $detail, actor: "jeryu-auto-pin"}
    + (if $pr != "" then {pr: ($pr | tonumber)} else {} end)
    + (if $reason != "" then {reason: $reason} else {} end)
    + (if $log_tail != "" then {log_tail: $log_tail} else {} end)')" || { say "event: could not build $kind (ignored)"; return 0; }
  api POST /api/v1/events --data-binary @- <<<"$body" 2>/dev/null | jq -e '.ok == true' >/dev/null 2>&1 \
    || say "event: posting $kind was not acknowledged (ignored)"
}

branch="auto/pin-web-${head:0:12}"
title="release: pin jeryu-web ${head:0:7}"
pulls="$(api GET "/api/v3/repos/$repo_path/pulls?state=all&per_page=100")" \
  || { say "could not list $repo_path pull requests"; exit 1; }
jq -e 'type == "array"' <<<"$pulls" >/dev/null || { say "the pull request list is not JSON"; exit 1; }
mine="$(jq -r --arg b "$branch" '[.[] | select(.head.ref == $b)][0].number // empty' <<<"$pulls")"
if [ -n "$mine" ]; then exit 0; fi # this head already has its pull request, open or decided
other="$(jq -r '[.[] | select(.state == "open" and (.title | startswith("release: pin jeryu-web")))][0].number // empty' <<<"$pulls")"
if [ -n "$other" ]; then waiting_for "$other"; say "bump #$other is open; ${head:0:12} waits for it to land"; exit 0; fi

pushed="$(git ls-remote "$remote" "refs/heads/$branch" | cut -f1)"
if [ -z "$pushed" ]; then
  gate="$(api GET "/api/v3/repos/$web_path/commits/$head/status" | jq -r '.state // "unknown"')" || gate=unknown
  if [ "$gate" != success ]; then waiting_for ""; say "jeryu-web ${head:0:12} gate is $gate; waiting"; exit 0; fi
fi

attempt=$((failures + 1))
log="$state/logs/${head:0:12}-attempt$attempt.log"; : >"$log"
give_up() { # REASON: count the failure, tell the forge, stop this tick
  echo "$attempt" >"$state/failures/$head"
  say "bumping to ${head:0:12} failed (attempt $attempt of $max_failures): $1"
  local asks=false; (( attempt >= max_failures )) && asks=true
  emit pin.bump_failed "$asks" "auto-pin:failed:${head:0:12}:$attempt" \
    "bumping the jeryu-web pin to ${head:0:12} failed (attempt $attempt of $max_failures)" \
    "$(jq -n --arg dep "$web_path" --arg from "$pin" --arg to "$head" --argjson attempt "$attempt" \
      --argjson max "$max_failures" '{dependency: $dep, from: $from, to: $to, attempt: $attempt, max_attempts: $max}')" \
    "" "$1; $([ "$asks" = true ] && echo 'no further attempt will be made for this commit' || echo 'it will be retried on the next tick')" "$log"
  exit 1
}

shipped="$(git -C "$web_cache" log --format='- %h %s' -n 30 "$pin..$head")"
if [ -z "$pushed" ]; then
  say "building jeryu-web ${head:0:12} (gate success; pin is ${pin:0:12})"
  git -C "$cache" archive "$main" scripts/release "$lock_file" | tar -x -C "$work"
  pair="$(JERYU_WEB_REMOTE="$web_remote" "$work/scripts/release/build-web-dist.sh" --commit "$head" "$state/web-dist" \
    2> >(tee -a "$log" >&2) | tee -a "$log" | tail -1)" || give_up "build-web-dist.sh failed"
  wait 2>/dev/null || true # let the tee drain before the log is read
  read -r built hash <<<"$pair"
  [[ "$built" == "$head" && "$hash" =~ ^[0-9a-f]{64}$ ]] || give_up "build-web-dist.sh did not print '<commit> <hash>' for ${head:0:12}"

  co="$work/checkout"
  git clone -q --no-hardlinks "$cache" "$co" 2>>"$log" && git -C "$co" checkout -q -b "$branch" "$main" 2>>"$log" \
    || give_up "could not cut $branch from main ${main:0:12}"
  pinned_hash="$(awk '
    /^\[\[repo\]\]/ { inweb = 0 }
    /^name = "jeryu-web"$/ { inweb = 1 }
    inweb && $1 == "web_dist_sha256" && $2 == "=" { gsub(/"/, "", $3); print $3; exit }' "$co/$lock_file")"
  awk -v commit="$head" -v hash="$hash" '
    /^\[\[repo\]\]/ { inweb = 0 }
    /^name = "jeryu-web"$/ { inweb = 1 }
    inweb && $1 == "commit" && $2 == "=" { print "commit = \"" commit "\""; next }
    inweb && $1 == "web_dist_sha256" && $2 == "=" { print "web_dist_sha256 = \"" hash "\""; next }
    { print }' "$co/$lock_file" >"$work/lock.new" && cat "$work/lock.new" >"$co/$lock_file"
  # The bump is exactly the pin fields in exactly one file, or it is not a bump. A web commit that
  # touches only tests or docs builds the bundle that is already pinned: then the commit moves and
  # the hash line stays as it is, which is one changed line, not two. Refusing that left such a
  # head unpinned, and the inbox asking for a human, until some later commit changed the bundle.
  changed=2 same_bundle=""
  if [[ "$hash" == "$pinned_hash" ]]; then
    changed=1
    same_bundle="
The bundle is byte-identical to the one already pinned (these commits change no shipped file), so
only \`commit\` moves."
  fi
  [[ "$(git -C "$co" diff --numstat | tr '\t' ' ')" == "$changed $changed $lock_file" ]] \
    || give_up "the lock edit is not exactly the pin fields"
  git -C "$co" -c user.name="$git_name" -c user.email="$git_email" commit -q -am "$title

Ships these jeryu-web commits (${pin:0:7}..${head:0:7}):
$shipped

web_dist_sha256 $hash, from scripts/release/build-web-dist.sh --commit $head.$same_bundle
Opened by auto-pin.sh once jeryu-web main was green; only $lock_file changes." 2>>"$log" \
    || give_up "could not commit the bump"
  git -C "$co" push -q "$remote" "HEAD:refs/heads/$branch" 2>>"$log" || give_up "could not push $branch"
else
  say "$branch is already pushed without a pull request; opening it"
  hash="$(git -C "$cache" fetch -q "$remote" "+refs/heads/$branch:refs/auto-pin/branch" 2>>"$log" \
    && git -C "$cache" show "refs/auto-pin/branch:$lock_file" | awk '
      /^\[\[repo\]\]/ { inweb = 0 }
      /^name = "jeryu-web"$/ { inweb = 1 }
      inweb && $1 == "web_dist_sha256" && $2 == "=" { gsub(/"/, "", $3); print $3; exit }')" || hash=""
fi

body="Pins jeryu-web main \`${head:0:7}\` so the next staged release ships it. Only \`$lock_file\` changes (\`commit\`, \`web_dist_sha256\`).

Ships (${pin:0:7}..${head:0:7}):
$shipped

Opened by \`scripts/release/auto-pin.sh\` because jeryu-web main is green and ahead of the pin. It never merges: review and the merge queue land this, auto-stage then stages main, and a person deploys."
opened="$(jq -n --arg title "$title" --arg head "$branch" --arg body "$body" \
  '{title: $title, head: $head, base: "main", body: $body}' \
  | api POST "/api/v3/repos/$repo_path/pulls" --data-binary @- 2>>"$log")" || give_up "the forge refused the pull request"
number="$(jq -r '.number // empty' <<<"$opened" 2>/dev/null)" || number=""
[[ "$number" =~ ^[0-9]+$ ]] || give_up "the forge did not answer with a pull request number"

printf '%s\t%s\t%s\n' "$head" "$number" "$(date -u +%FT%TZ)" >>"$state/opened.tsv"
rm -f "$state/failures/$head"
find "$state/logs" -maxdepth 1 -type f -name '*.log' -printf '%T@ %p\n' | sort -rn | tail -n +21 \
  | cut -d' ' -f2- | xargs -r rm -f
say "OPENED $repo_path#$number: $title"
emit pin.bump_opened false "auto-pin:opened:${head:0:12}" \
  "opened $repo_path#$number: pin jeryu-web ${head:0:7} ($(grep -c . <<<"$shipped") commits)" \
  "$(jq -n --arg dep "$web_path" --arg from "$pin" --arg to "$head" --arg hash "$hash" \
    '{dependency: $dep, from: $from, to: $to, web_dist_sha256: $hash}')" "$number"
