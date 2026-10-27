#!/usr/bin/env bash
# auto-pin.sh — when jeryu-web's green ref is newer than the pin in jeryu-deploy main's
# jeryu-split.lock.toml, open the pull request that bumps the pin. Run by a timer on the release
# host (node-0). A bump is mechanical (build the dist, hash it, change two lock fields), and until
# now it happened only when somebody remembered, so merged UI work sat unreleasable.
#
# It only proposes. Review and landing belong to pr-redteam and the merge queue, staging to
# auto-stage.sh, the deploy to a person. It never merges, approves, stages or deploys.
#
#   1. Resolve jeryu-deploy main and jeryu-web's `refs/green/main`: the newest commit of jeryu-web
#      main the gate proved green, which the forge moves there itself (the green ref; see
#      crates/jeryu-api/src/green_ref.rs), read by its exact name, never as an ls-remote tail
#      pattern a branch could also match. That commit is what this pins, not main's head: a
#      landing that rebased leaves main's head without a status, which is what used to leave a
#      green jeryu-web unpinned. The ref is not trusted alone: before building, the commit must
#      be on jeryu-web main and its newest `jeryu-web/main-health` must be `success`
#      (a wait otherwise). Nothing to do when the pin already is that commit.
#   2. Nothing to do when the green commit does not descend from the pin (a human rewrote history),
#      when any bump pull request for jeryu-web is open (its landing moves the pin; the next tick
#      proposes whatever is newer), or when this head already has its branch and pull request.
#      An open bump that pins an OLDER commit on the way to this head, and is neither queued nor
#      approved (unless its exact-head required checks finished with failure), and has no
#      changes-requested review inside JERYU_AUTO_PIN_SUPERSEDE_AFTER,
#      is closed after the replacement exists, and the new bump
#      opens in the same tick (`pin.bump_superseded`). A queued bump is never closed; a bump pinning a
#      commit that is not an ancestor of the head is left alone with a `pin.bump_diverged` event, and
#      so is one this script did not open.
#      An owned same-head bump with a finished failing required gate is refreshed
#      with current deploy main when behind, without rewriting its branch history. Only when it is
#      the only open web pin bump: beside another one, both are left for a person. When main
#      changed the bundle recipe (build-web-dist.sh) since the bump's base, the refresh rebuilds
#      with main's recipe and pins that hash. A bump is refreshed at most
#      JERYU_AUTO_PIN_MAX_REFRESHES times and no more often than JERYU_AUTO_PIN_REFRESH_AFTER;
#      then `pin.bump_failed` asks for a person, since its gate fails on the bump itself.
#   3. Build the dist with build-web-dist.sh taken from jeryu-deploy main itself (git archive), so
#      the recipe is always the reviewed one. It prints "<commit> <web_dist_sha256>".
#   4. On branch auto/pin-web-<sha12> cut from main: set exactly the two lock fields, commit as
#      rel-bot with the web commits the bump ships, push, open the pull request, and post a
#      `pin.bump_opened` pipeline event (docs/pipeline-events.md).
#   5. A head that fails is retried once; then `pin.bump_failed` asks for a human and the head is
#      left alone. A newer head starts fresh. A branch pushed without its pull request (the API
#      call failed) gets the pull request on the next tick, without rebuilding.
#
#   6. Every tick that resolves both mains, idle or not, first deletes the auto/pin-web-* branches
#      whose pull request is closed (merged, or superseded by a newer bump): only a pull request of
#      this repository, only while the branch is still its head, never one an open pull request
#      uses and never the current web head's. A refused delete is logged with git's reason.
#
#      It then closes its own open bump pull requests that have nothing left to land: every commit
#      of the bump is already on jeryu-deploy main as another commit, and merging it would leave
#      main's tree as it is. #184 and #193 stayed open after the same pin reached main by another
#      route; #193 was gated anyway, hit a flaky test and asked for a human about a change that had
#      already shipped. The close names the commit the work landed as, in a comment first so the
#      reason survives the close. Only this repository's auto/pin-web-* pull requests, and only
#      while git says both of those things; the branch goes on a later tick's prune. Which branches
#      are candidates is asked of git alone, so a tick with no landed bump sends no extra request.
#
#   7. Every tick ends with one runner heartbeat (`<host>/auto-pin`, label `automation`), so
#      /runners shows the timer alive and the last thing it did: opened a bump, is waiting behind
#      one or for the web gate, or failed on a head. Best-effort: it never fails or delays a tick.
#
# State in $JERYU_AUTO_PIN_STATE (~/.local/state/jeryu-auto-pin): opened.tsv (web sha, PR, time),
# failures/<web sha>, refresh/<PR> (refreshes so far and when), prune-seen (the bump branches the last prune pass left), waiting (what the
# newest wait is for, and since when), git caches, web-dist/ (build-web-dist.sh's OUT_ROOT), logs/
# (newest 20).
#
# Env: JERYU_DEPLOY_REMOTE, JERYU_WEB_REMOTE (git URLs; pushing uses this host's git credential);
# JERYU_DEPLOY_REPO, JERYU_WEB_REPO (owner/name on the forge; default: from the remotes);
# JERYU_BASE (must be https://git.neverhuman.org: the token is sent nowhere else);
# JERYU_PIN_TOKEN_FILE (required, no default: the pull request author's token, also used for
# the status API and the events; a regular 0600 file);
# JERYU_PIN_GIT_NAME / JERYU_PIN_GIT_EMAIL (required, no default: the identity the bump is
# committed as. A credential path and an automation identity belong to one installation, and
# this source is public, so none of the three has a default);
# JERYU_AUTO_PIN_MAX_FAILURES (2); JERYU_AUTO_PIN_SUPERSEDE_AFTER (6h; also 90m or seconds);
# JERYU_AUTO_PIN_MAX_REFRESHES (3); JERYU_AUTO_PIN_REFRESH_AFTER (1h); JERYU_AUTO_PIN_EVENTS=0 turns the events off;
# JERYU_AUTO_PIN_BEAT=0 turns the heartbeat off.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
remote="${JERYU_DEPLOY_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-deploy.git}"
web_remote="${JERYU_WEB_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-web.git}"
base="${JERYU_BASE:-https://git.neverhuman.org}"; base="${base%/}"
token_file="${JERYU_PIN_TOKEN_FILE:-}"
git_name="${JERYU_PIN_GIT_NAME:-}" git_email="${JERYU_PIN_GIT_EMAIL:-}"
max_failures="${JERYU_AUTO_PIN_MAX_FAILURES:-2}"
state="${JERYU_AUTO_PIN_STATE:-$HOME/.local/state/jeryu-auto-pin}"
lock_file=jeryu-split.lock.toml
say() { echo "[auto-pin $(date -u +%H:%M:%S)] $*" >&2; }
repo_of() { sed -E 's#^https?://[^/]+/git/##; s#\.git$##' <<<"$1"; }
repo_path="${JERYU_DEPLOY_REPO:-$(repo_of "$remote")}"
web_path="${JERYU_WEB_REPO:-$(repo_of "$web_remote")}"

[[ "$base" == https://git.neverhuman.org ]] || { say "refusing a noncanonical credential origin"; exit 2; }
[[ -n "$token_file" ]] || { say "JERYU_PIN_TOKEN_FILE is unset: set it to the bump author's 0600 credential file"; exit 2; }
[[ -n "$git_name" && -n "$git_email" ]] \
  || { say "JERYU_PIN_GIT_NAME and JERYU_PIN_GIT_EMAIL are unset: set them to the identity the bump is committed as"; exit 2; }
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
# waiting_for PR: this head waits (behind bump #PR, or for the gate when PR is empty: no green
# ref yet, or a green commit whose main-health is not success). The time the wait began is kept
# across ticks, so the row says since when and not "just now".
waiting_for() {
  local key="$head ${1:-gate}" since
  since="$(awk -F'\t' -v key="$key" '$1 == key { print $2 }' "$state/waiting" 2>/dev/null)" || since=""
  [ -n "$since" ] || { since="$(date -u +%FT%TZ)"; printf '%s\t%s\n' "$key" "$since" >"$state/waiting"; }
  beat_last="$(did waiting "${1:-}" "$head" "$since")" || beat_last=""
}
# remote_ref REMOTE REF -> the oid of exactly REF. ls-remote matches a pattern against the tail of
# every ref, so `refs/green/main` alone also matches a branch named "refs/green/main"
# (refs/heads/refs/green/main), which any writer can create; only the exact name counts.
remote_ref() { git ls-remote "$1" "$2" | awk -v ref="$2" '$2 == ref { print $1 }'; }
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

api() { # METHOD PATH [JSON on stdin] -> body on stdout
  local method="$1" path="$2"; shift 2
  curl --silent --show-error --fail --max-time 60 --config "$cfg" -X "$method" \
    -H 'Accept: application/json' -H 'Content-Type: application/json' "$@" "$base$path"
}

# list_pulls: every pull request of jeryu-deploy into $pulls. Every page: the forge lists oldest
# first and ignores sort and head filters, so page 1 alone stops showing new pull requests once the
# repo has more than 100, and every tick opened a duplicate. Returns 1, having said why, on failure.
pulls=""
list_pulls() {
  if [ "${1:-}" != fresh ] && [ -n "$pulls" ]; then return 0; fi
  local page batch all='[]'
  for page in $(seq 1 50); do
    batch="$(api GET "/api/v3/repos/$repo_path/pulls?state=all&per_page=100&page=$page")" \
      || { say "could not list $repo_path pull requests"; return 1; }
    jq -e 'type == "array"' <<<"$batch" >/dev/null || { say "the pull request list is not JSON"; return 1; }
    all="$(jq -s 'add' <(printf '%s' "$all") <(printf '%s' "$batch"))"
    (( $(jq length <<<"$batch") < 100 )) && { pulls="$all"; return 0; }
  done
  say "more than 5000 $repo_path pull requests; not guessing which exist"; return 1
}

# prune_branches: delete the auto/pin-web-* branches whose pull request is closed, merged or
# superseded by a newer bump, so they do not pile up on the forge. A branch goes only when a closed
# pull request from this repository (not a fork) has it as its head at its current commit, and no
# open pull request uses it; the current web head's branch always stays. The pull requests are
# listed only when the set of bump branches changed since the last clean pass (prune-seen), so a
# branch that may not go costs one ls-remote per tick, not a full listing. Runs early, so a head
# auto-pin gave up on still prunes. Best-effort: logged, never fails a tick.
bump_refs() { # the remote auto/pin-web-* branches except the current web head's, "<sha> <ref>" lines
  git ls-remote "$remote" 'refs/heads/auto/pin-web-*' 2>/dev/null \
    | grep -v "[[:space:]]refs/heads/auto/pin-web-${head:0:12}\$" | tr '\t' ' '
}
prune_branches() {
  local refs sha ref name pr err refused=""
  refs="$(bump_refs)" || return 0
  [ -z "$token_problem" ] || return 0
  [ -n "$pulls" ] || [ "$refs" != "$(cat "$state/prune-seen" 2>/dev/null)" ] || return 0
  [ -n "$pulls" ] || list_pulls || return 0
  while read -r sha ref; do
    name="${ref#refs/heads/}"
    pr="$(jq -r --arg b "$name" --arg sha "$sha" --arg repo "$repo_path" '
      [.[] | select(.head.ref == $b and .head.repo.full_name == $repo)] as $rows
      | if any($rows[]; .state == "open") then empty
        else [$rows[] | select(.state == "closed" and .head.sha == $sha)][0].number // empty end' <<<"$pulls")"
    [ -n "$pr" ] || continue
    # Read again just before the delete: a branch pushed to since the listing is no longer the
    # closed pull request's head and stays. This narrows the window, it does not close it: the
    # atomic form (--force-with-lease) is a force mutation the repository's audit refuses, and
    # these are bot branches whose commits stay reachable from their pull requests.
    [ "$(git ls-remote "$remote" "$ref" 2>/dev/null | cut -f1)" == "$sha" ] || continue
    # From the cache: git push needs a repository, and the timer starts outside one.
    if err="$(git -C "$cache" push -q "$remote" --delete "$ref" 2>&1)"; then
      say "deleted $name (#$pr is closed)"
    else
      refused=1
      say "could not delete $name (#$pr is closed; ignored): $(tail -n 1 <<<"$err")"
    fi
  done <<<"$refs"
  # A refused delete is tried, and logged, again next tick rather than going quiet.
  if [ -n "$refused" ]; then rm -f "$state/prune-seen"
  else bump_refs >"$state/prune-seen" || : >"$state/prune-seen"; fi
}

# landed_sha REPO_DIR UPSTREAM REF -> the commit of UPSTREAM that already carries REF's work, when
# merging REF would land nothing; exit 1 otherwise. REF is a no-op only when it has commits of its
# own, every one of them is already upstream (git cherry marks it "-"), AND merging it produces
# exactly upstream's tree. Both, because a commit whose patch is upstream may also take away
# something upstream has. Anything git cannot answer is "not a no-op", so nothing is closed on a
# guess. The landing commit is found by patch id over what upstream gained since REF's base; with no
# twin there (a squash, a rebase that reworded) upstream's tip is named instead.
# jeryu-ci-runner's gate decides the same thing the same way (gate_noop_landing in
# ops/pr-gate/bin/pr-gate-state.sh) and answers such a head green without a build.
patch_id() { # REPO_DIR COMMIT -> its patch identity, or nothing (a merge commit has none)
  local identity
  identity="$(git -C "$1" diff-tree --patch --no-commit-id --root "$2" 2>/dev/null | git patch-id --stable)" || return 1
  [ -n "$identity" ] || return 1
  printf '%s' "${identity%% *}"
}
landed_sha() {
  local dir="$1" upstream="$2" ref="$3" own line newest="" tree merged identity candidate landing=""
  own="$(git -C "$dir" cherry "$upstream" "$ref" 2>/dev/null)" || return 1
  [ -n "$own" ] || return 1
  while read -r line; do
    case "$line" in "- "*) newest="${line#- }" ;; *) return 1 ;; esac
  done <<<"$own"
  tree="$(git -C "$dir" rev-parse --verify "$upstream^{tree}" 2>/dev/null)" || return 1
  merged="$(git -C "$dir" merge-tree --write-tree "$upstream" "$ref" 2>/dev/null)" || return 1
  [ "$merged" = "$tree" ] || return 1
  identity="$(patch_id "$dir" "$newest")" || identity=""
  if [ -n "$identity" ]; then
    while read -r candidate; do
      [ "$(patch_id "$dir" "$candidate")" = "$identity" ] || continue
      landing="$candidate"; break
    done < <(git -C "$dir" rev-list --max-count=2000 "$ref..$upstream" 2>/dev/null)
  fi
  [ -n "$landing" ] || landing="$(git -C "$dir" rev-parse --verify "$upstream" 2>/dev/null)" || return 1
  printf '%s' "$landing"
}

# close_landed_bumps: close this repository's open auto/pin-web-* pull requests whose content is
# already on main (see the header, step 6). git decides which branches are candidates, so the pull
# requests are listed only when there is one; each close is a comment naming the landing commit and
# then the state change, and a refusal is logged and tried again on the next tick.
# Best-effort: it never fails a tick.
close_landed_bumps() {
  local refs sha ref name number landing current rows=""
  [ -z "$token_problem" ] || return 0
  refs="$(git ls-remote "$remote" 'refs/heads/auto/pin-web-*' 2>/dev/null)" || return 0
  [ -n "$refs" ] || return 0
  git -C "$cache" fetch -q --prune "$remote" \
    '+refs/heads/auto/pin-web-*:refs/auto-pin/bumps/pin-web-*' 2>/dev/null || return 0
  while read -r sha ref; do
    name="${ref#refs/heads/}"
    landing="$(landed_sha "$cache" refs/heads/main "refs/auto-pin/bumps/${name#auto/}")" || continue
    rows="$rows$name $sha $landing"$'\n'
  done <<<"$(tr '\t' ' ' <<<"$refs")"
  [ -n "$rows" ] || return 0
  list_pulls || return 0
  while read -r name sha landing; do
    [ -n "$name" ] || continue
    number="$(jq -r --arg b "$name" --arg sha "$sha" --arg repo "$repo_path" '
      [.[] | select(.state == "open" and .head.ref == $b and .head.sha == $sha
        and .head.repo.full_name == $repo and .base.ref == "main"
        and .base.repo.full_name == $repo)][0].number // empty' <<<"$pulls")"
    [ -n "$number" ] || continue
    # A branch prefix is not proof that this timer opened the pull request.
    awk -F '\t' -v n="$number" -v short="${name#auto/pin-web-}" \
      '$2 == n && substr($1, 1, 12) == short {found=1} END {exit !found}' \
      "$state/opened.tsv" || { say "bump #$number has no auto-pin ownership record; leaving it open"; continue; }
    # The comment goes first: it is the reason the pull request was closed, and a close whose
    # comment never arrived would say nothing. A refused comment leaves it open for a person.
    jq -n --arg landing "$landing" --arg name "$name" '{body: ("Closing this bump: its change is already on main as `\($landing)`, and merging it would leave main exactly as it is, so there is nothing left for it to land. Branch `\($name)` is deleted by a later tick.\n\nClosed by the release automation that opened it.")}' \
      | api POST "/api/v3/repos/$repo_path/issues/$number/comments" --data-binary @- >/dev/null 2>&1 \
      || { say "could not comment on #$number before closing it (ignored); leaving it open"; continue; }
    [ "$(git ls-remote "$remote" "refs/heads/$name" | cut -f1)" = "$sha" ] \
      || { say "bump #$number moved after the listing; leaving it open"; continue; }
    # A different PR target is not covered by main's no-op result. Re-read
    # its identity instead of closing from an earlier pull-request listing.
    current="$(api GET "/api/v3/repos/$repo_path/pulls/$number" 2>/dev/null)" \
      || { say "could not re-read #$number before closing it; leaving it open"; continue; }
    jq -e --argjson n "$number" --arg b "$name" --arg sha "$sha" --arg repo "$repo_path" '
      .number == $n and .state == "open" and .head.ref == $b and .head.sha == $sha
      and .head.repo.full_name == $repo and .base.ref == "main"
      and .base.repo.full_name == $repo' <<<"$current" >/dev/null \
      || { say "bump #$number changed after the listing; leaving it open"; continue; }
    [ "$(git ls-remote "$remote" refs/heads/main | cut -f1)" = "$main" ] \
      || { say "main moved after the landing check; leaving #$number open"; continue; }
    # These reads narrow the race, but the current API has no conditional
    # close: identity can change between this check and the PATCH below.
    if printf '{"state":"closed"}' \
      | api PATCH "/api/v3/repos/$repo_path/pulls/$number" --data-binary @- >/dev/null 2>&1; then
      say "closed #$number ($name): already on main as ${landing:0:12}"
    else
      say "could not close #$number ($name; ignored): it is already on main as ${landing:0:12}"
    fi
  done <<<"$rows"
}

main="$(remote_ref "$remote" refs/heads/main)"
# The gate's verdict about jeryu-web main, as a ref: whatever this names was proved green by a
# full gate run of that exact commit. An empty ref is "nothing proved yet", not "not green".
head="$(remote_ref "$web_remote" refs/green/main)"
[[ "$main" =~ ^[0-9a-f]{40}$ ]] || { say "could not resolve jeryu-deploy main ($main)"; exit 1; }
# No green ref at all: the gate has proved no commit of that main yet, so there is nothing to
# pin. The heartbeat says the timer waits for the gate of jeryu-web main's head.
if [ -z "$head" ]; then
  head="$(remote_ref "$web_remote" refs/heads/main)"
  if [[ "$head" =~ ^[0-9a-f]{40}$ ]]; then waiting_for ""; else head=""; fi
  say "$web_path has no refs/green/main yet; waiting for the gate"; exit 0
fi
[[ "$head" =~ ^[0-9a-f]{40}$ ]] || { say "could not resolve $web_path refs/green/main ($head)"; exit 1; }

cache="$state/jeryu-deploy.git"
[ -d "$cache" ] || git clone -q --bare "$remote" "$cache"
git -C "$cache" fetch -q "$remote" "+refs/heads/main:refs/heads/main"
prune_branches
close_landed_bumps
# The jeryu-web [[repo]] block of the lock, as build-web-dist.sh reads it.
pin="$(git -C "$cache" show "$main:$lock_file" | awk '
  /^\[\[repo\]\]/ { inweb = 0 }
  /^name = "jeryu-web"$/ { inweb = 1 }
  inweb && $1 == "commit" && $2 == "=" { gsub(/"/, "", $3); print $3; exit }')"
[[ "$pin" =~ ^[0-9a-f]{40}$ ]] || { say "$lock_file at main ${main:0:12} has no 40-hex jeryu-web commit ($pin)"; exit 1; }
[ "$pin" != "$head" ] || { say "jeryu-web ${head:0:12} is already pinned on main; nothing to do"; exit 0; }

web_cache="$state/jeryu-web.git"
[ -d "$web_cache" ] || git clone -q --bare "$web_remote" "$web_cache"
git -C "$web_cache" fetch -q "$web_remote" "+refs/heads/main:refs/heads/main"
# The ref is not trusted alone. The forge moves it only onto a commit of main, but a bare clone
# carries every branch, so an off-main commit would build here without this check.
if ! git -C "$web_cache" merge-base --is-ancestor "$head" refs/heads/main 2>/dev/null; then
  say "$web_path refs/green/main ${head:0:12} is not a commit of jeryu-web main; refusing to pin it"; exit 1
fi
if ! git -C "$web_cache" merge-base --is-ancestor "$pin" "$head" 2>/dev/null; then
  say "jeryu-web main ${head:0:12} does not descend from the pin ${pin:0:12}; leaving it for a human"; exit 0
fi
failures="$(cat "$state/failures/$head" 2>/dev/null || echo 0)"
if (( failures >= max_failures )); then
  say "bumping to ${head:0:12} failed $failures times; leaving it for a human"; exit 0
fi

[ -z "$token_problem" ] || { say "$token_problem"; exit 2; }

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
list_pulls || exit 1
# This head's own bump: the newest open one of this repository, else any (a decided one).
mine="$(jq -r --arg b "$branch" --arg repo "$repo_path" '[.[] | select(.head.ref == $b)] as $rows
  | ([$rows[] | select(.state == "open" and .head.repo.full_name == $repo)] | max_by(.number) | .number)
    // $rows[0].number // empty' <<<"$pulls")"
other="$(jq -r '[.[] | select(.state == "open" and (.title | startswith("release: pin jeryu-web")))][0].number // empty' <<<"$pulls")"
other_count="$(jq '[.[] | select(.state == "open" and (.title | startswith("release: pin jeryu-web")))] | length' <<<"$pulls")"
if (( other_count > 1 )); then
  waiting_for "$other"; say "multiple web pin bumps are open; leaving them for a human"; exit 0
fi

# supersede_stale N: the open bump #N pins an older commit of jeryu-web main than this head. When it
# is not queued and has no changes-requested review inside the grace period, prepare its
# replacement. An approval preserves a viable bump, but not one whose exact-head required
# checks have finished with failure. Closing waits until the new PR exists; its review starts fresh.
# Returns 1 when it stays. Anything it cannot read counts as stays.
supersede_stale() {
  local n="$1" row ref created pinned_sha queue_rows reviews_json approved changes age queued
  row="$(jq -c --argjson n "$n" --arg repo "$repo_path" '
    [.[] | select(.number == $n and .state == "open" and .head.repo.full_name == $repo)][0] // empty' <<<"$pulls")"
  [ -n "$row" ] || return 1 # no longer open, or not a branch of this repository
  ref="$(jq -r '.head.ref' <<<"$row")"; created="$(jq -r '.created_at // empty' <<<"$row")"
  [[ "$ref" =~ ^auto/pin-web-([0-9a-f]{12})$ ]] || return 1 # only auto-pin's own bumps
  pinned_sha="$(git -C "$web_cache" rev-parse --verify -q "${BASH_REMATCH[1]}^{commit}")" || return 1
  awk -F '\t' -v sha="$pinned_sha" -v n="$n" '$1 == sha && $2 == n { found = 1 } END { exit !found }' \
    "$state/opened.tsv" 2>/dev/null || { say "bump #$n has no auto-pin ownership record; leaving it open"; return 1; }
  if ! git -C "$web_cache" merge-base --is-ancestor "$pinned_sha" "$head" 2>/dev/null; then
    say "bump #$n pins ${pinned_sha:0:12}, which does not lead to jeryu-web main ${head:0:12}; leaving it for a human"
    emit pin.bump_diverged true "auto-pin:diverged:$n:${head:0:12}" \
      "bump $repo_path#$n pins jeryu-web ${pinned_sha:0:7}, which is not an ancestor of main ${head:0:7}; left open" \
      "$(jq -n --arg dep "$web_path" --arg pinned "$pinned_sha" --arg to "$head" '{dependency: $dep, pinned: $pinned, to: $to}')" \
      "$n" "a diverged pin is never closed automatically"
    return 1
  fi
  # Nothing here asks whether the head is green: it came from refs/green/main, which only a green
  # gate run of that commit moves, so the replacement this prepares always has a commit to pin and
  # the old bump is never closed with no new one coming.
  queue_rows="$(api GET "/api/v1/repos/$repo_path/merge-queue?per_page=100" 2>/dev/null)" \
    || { say "bump #$n: could not read the merge queue; not superseding"; return 1; }
  queued="$(jq -r --argjson n "$n" '
    if (.page.has_more // false) then "unknown"
    elif any((.entries // [])[]; .number == $n and (.state | IN("landed", "failed", "dequeued") | not)) then "yes"
    else "no" end' <<<"$queue_rows" 2>/dev/null)" || queued=unknown
  [ "$queued" = no ] || { say "bump #$n is in the merge queue ($queued); not superseding"; return 1; }
  reviews_json="$(api GET "/api/v1/repos/$repo_path/pulls/$n" 2>/dev/null)" \
    || { say "bump #$n: could not read its reviews; not superseding"; return 1; }
  approved="$(jq -r 'any((.reviews // [])[]; .effective == true and .state == "APPROVED")' <<<"$reviews_json" 2>/dev/null)" || return 1
  changes="$(jq -r 'any((.reviews // [])[]; .effective == true and .state == "CHANGES_REQUESTED")' <<<"$reviews_json" 2>/dev/null)" || return 1
  age=0; [ -z "$created" ] || age=$(( $(date -u +%s) - $(date -u -d "$created" +%s 2>/dev/null || date -u +%s) ))
  if [ "$approved" = true ]; then
    if ! required_failed "$n" "$row" "$reviews_json"; then
      say "bump #$n has a current approval without a finished failing required gate; not superseding"; return 1
    fi
  fi
  if [ "$changes" = true ] && (( age <= supersede_after )); then
    say "bump #$n has requested changes inside the grace period; not superseding"; return 1
  fi
  say "bump #$n pins ${pinned_sha:0:12}, behind green main ${head:0:12}, and is not queued (approved=$approved changes_requested=$changes age=${age}s); superseding"
  superseded_pr="$n"; superseded_pinned="$pinned_sha"
  return 0
}
# required_failed N LIST_ROW DETAIL: check only the old bump's required results, bound to its
# listed head. Use native required-context posture: a raw checks list omits required contexts
# that have never reported. Page the native list and bracket it with exact-head reads.
# Missing, unreadable, running or advisory-only failures never discard an approval. This runs
# again immediately before close, after the replacement build and creation have completed.
required_failed() {
  local n="$1" row="$2" detail="$3" sha batch posture current page checks
  sha="$(jq -r '.head.sha // empty' <<<"$row")" || return 1
  [[ "$sha" =~ ^[0-9a-f]{40}$ ]] || return 1
  jq -e --arg sha "$sha" --arg ref "$(jq -r '.head.ref' <<<"$row")" '
    .summary.state == "open" and .summary.base_ref == "main"
    and .summary.head_ref == $ref and .summary.head_sha == $sha' <<<"$detail" >/dev/null || return 1
  posture=""
  for page in $(seq 1 50); do
    batch="$(api GET "/api/v1/repos/$repo_path/pulls?state=open&per_page=100&page=$page" 2>/dev/null)" || return 1
    posture="$(jq -ce --argjson n "$n" '.items[] | select(.number == $n)' <<<"$batch" 2>/dev/null)" && break
    jq -e '.page.has_more == true' <<<"$batch" >/dev/null 2>&1 || return 1
  done
  jq -e --arg sha "$sha" '.head_sha == $sha and (.required_contexts | type == "array")
    and any(.required_contexts[]; .state == "failing")
    and all(.required_contexts[]; .state | IN("passing", "failing"))' <<<"$posture" >/dev/null 2>&1 || return 1
  # A failing posture may mean the forge could not read evidence. Require an
  # actual concluded required failure too, before treating it as a failed gate.
  checks="$(api GET "/api/v1/repos/$repo_path/pulls/$n/checks" 2>/dev/null)" || return 1
  jq -e 'any(.checks[]; .required == true and (.status | IN("failure", "error")))' \
    <<<"$checks" >/dev/null 2>&1 || return 1
  current="$(api GET "/api/v1/repos/$repo_path/pulls/$n" 2>/dev/null)" || return 1
  jq -e --arg sha "$sha" '.summary.state == "open" and .summary.base_ref == "main"
    and .summary.head_sha == $sha' <<<"$current" >/dev/null || return 1
}
# Refresh only an owned, unqueued bump with finished failing required checks. The new
# commit has both the previous bump and main as parents: a normal push preserves
# history and the PR/ownership record, while its tree replays only the pin onto main.
refresh_unqueued() {
  local n="$1" detail="$2" queue
  queue="$(api GET "/api/v1/repos/$repo_path/merge-queue?per_page=100")" \
    && jq -e --argjson n "$n" '.page.has_more == false and (.entries | type == "array")
      and all(.entries[]; .number != $n or (.state | IN("landed", "failed", "dequeued")))' <<<"$queue" >/dev/null \
    || { say "bump #$n is queued or queue evidence is unavailable; skipping"; return 1; }
  jq -e 'all((.reviews // [])[]; .effective != true or .state != "CHANGES_REQUESTED")' <<<"$detail" >/dev/null \
    || { say "bump #$n has requested changes; skipping"; return 1; }
}
# refresh_record N: count one refresh attempt of bump #N, so a bump whose gate keeps failing on its
# own merits is refreshed at most JERYU_AUTO_PIN_MAX_REFRESHES times, no more often than
# JERYU_AUTO_PIN_REFRESH_AFTER, and then left for a person.
refresh_record() {
  local n="$1" count=0 at
  read -r count at <"$state/refresh/$n" 2>/dev/null || count=0
  [[ "$count" =~ ^[0-9]+$ ]] || count=0
  printf '%s %s\n' "$((count + 1))" "$(date -u +%s)" >"$state/refresh/$n"
}
# refresh_due N: 0 when bump #N may be refreshed now; says why not otherwise.
refresh_due() {
  local n="$1" count=0 at=0 age
  read -r count at <"$state/refresh/$n" 2>/dev/null || { count=0; at=0; }
  [[ "$count" =~ ^[0-9]+$ && "$at" =~ ^[0-9]+$ ]] || { count=0; at=0; }
  if (( count >= max_refreshes )); then
    say "bump #$n was refreshed $count times and its required gate still fails; leaving it for a human"
    emit pin.bump_failed true "auto-pin:refresh-exhausted:$n:$count" \
      "bump $repo_path#$n still fails its required gate after $count refreshes onto main; left for a person" \
      "$(jq -n --arg dep "$web_path" --arg to "$head" --argjson refreshes "$count" '{dependency: $dep, to: $to, refreshes: $refreshes}')" \
      "$n" "the gate fails on the bump itself, not only because main moved"
    return 1
  fi
  age=$(( $(date -u +%s) - at ))
  if (( count > 0 && age < refresh_after )); then
    say "bump #$n was refreshed ${age}s ago (refresh $count of $max_refreshes); next refresh after ${refresh_after}s; skipping"
    return 1
  fi
}
# lock_with LOCK_REV COMMIT HASH: the lock at LOCK_REV with only the jeryu-web pin fields set.
lock_with() {
  git -C "$cache" show "$1:$lock_file" | awk -v commit="$2" -v hash="$3" '
    /^\[\[repo\]\]/ { inweb = 0 }
    /^name = "jeryu-web"$/ { inweb = 1 }
    inweb && $1 == "commit" && $2 == "=" { print "commit = \"" commit "\""; next }
    inweb && $1 == "web_dist_sha256" && $2 == "=" { print "web_dist_sha256 = \"" hash "\""; next }
    { print }'
}
# The files that decide the bundle's bytes. When main changed them since the bump's base, the
# carried web_dist_sha256 was made by a recipe main no longer has, so the refresh rebuilds.
recipe_paths=(scripts/release/build-web-dist.sh)
refresh_bump() {
  local n="$1" row sha detail base_sha co hash expected actual current pair built rebuilt=""
  row="$(jq -ce --argjson n "$n" --arg repo "$repo_path" --arg branch "$branch" '
    .[] | select(.number == $n and .state == "open" and .head.ref == $branch
      and .head.repo.full_name == $repo and .base.ref == "main"
      and .base.repo.full_name == $repo)' <<<"$pulls")" \
    || { say "bump #$n is decided or not an open same-repository main bump; skipping"; return 0; }
  awk -F '\t' -v sha="$head" -v n="$n" '$1 == sha && $2 == n {found=1} END {exit !found}' "$state/opened.tsv" \
    || { say "bump #$n has no auto-pin ownership record; skipping"; return 0; }
  sha="$(jq -r '.head.sha' <<<"$row")"
  detail="$(api GET "/api/v1/repos/$repo_path/pulls/$n")" && required_failed "$n" "$row" "$detail" \
    || { say "bump #$n has no finished failing exact-head required gate; skipping"; return 0; }
  refresh_unqueued "$n" "$detail" || return 0
  # A ref clone copies, so the work clone below reaches the bump without object-copy tricks.
  git -C "$cache" fetch -q "$remote" "+refs/heads/$branch:refs/heads/auto-pin-refresh" \
    && [ "$(git -C "$cache" rev-parse refs/heads/auto-pin-refresh)" = "$sha" ] \
    || { say "bump #$n moved or could not be fetched; skipping"; return 0; }
  base_sha="$(git -C "$cache" merge-base "$main" "$sha")" \
    || { say "bump #$n has no common base with main; skipping"; return 0; }
  [ "$base_sha" != "$main" ] \
    || { say "bump #$n already includes current main; skipping"; return 0; }
  refresh_due "$n" || return 0
  # Reconstruct the sole allowed edit from the common base. Do not replay any
  # unrelated file or lock-field changes, even on a branch this timer opened.
  hash="$(git -C "$cache" show "$sha:$lock_file" | awk '
    /^\[\[repo\]\]/ { inweb = 0 }
    /^name = "jeryu-web"$/ { inweb = 1 }
    inweb && $1 == "web_dist_sha256" { gsub(/"/, "", $3); print $3 }')" || hash=""
  [[ "$hash" =~ ^[0-9a-f]{64}$ ]] \
    || { say "bump #$n has no valid bundle hash; skipping"; return 0; }
  lock_with "$base_sha" "$head" "$hash" >"$work/refresh.lock" \
    && expected="$(git -C "$cache" hash-object "$work/refresh.lock")" \
    && actual="$(git -C "$cache" rev-parse "$sha:$lock_file")" \
    || { say "bump #$n has no readable $lock_file; skipping"; return 0; }
  [ "$expected" = "$actual" ] && [ "$(git -C "$cache" diff --name-only "$base_sha" "$sha")" = "$lock_file" ] \
    || { say "bump #$n is not solely the current web pin edit; skipping"; return 0; }
  if ! git -C "$cache" diff --quiet "$base_sha" "$main" -- "${recipe_paths[@]}"; then
    # Same build as the open path: main's own recipe, so the hash is the one staging will rebuild.
    say "main changed the bundle recipe since bump #$n was built; rebuilding jeryu-web ${head:0:12}"
    refresh_record "$n"
    mkdir -p "$work/recipe"
    pair="$(git -C "$cache" archive "$main" scripts/release "$lock_file" | tar -x -C "$work/recipe" \
      && JERYU_WEB_REMOTE="$web_remote" "$work/recipe/scripts/release/build-web-dist.sh" --commit "$head" "$state/web-dist" | tail -1)" \
      || { say "bump #$n: rebuilding with main's recipe failed; skipping"; return 0; }
    read -r built rebuilt <<<"$pair"
    [[ "$built" == "$head" && "$rebuilt" =~ ^[0-9a-f]{64}$ ]] \
      || { say "bump #$n: the rebuild did not print '<commit> <hash>' for ${head:0:12}; skipping"; return 0; }
    [ "$rebuilt" = "$hash" ] || say "bump #$n: main's recipe builds $rebuilt, not the carried $hash"
    hash="$rebuilt"
  fi
  co="$work/refresh"
  lock_with "$main" "$head" "$hash" >"$work/refresh-main.lock" \
    || { say "bump #$n: could not read $lock_file at main; skipping"; return 0; }
  git clone -q --no-hardlinks "$cache" "$co" \
    && git -C "$co" checkout -q --detach "$sha" \
    && git -C "$co" -c user.name="$git_name" -c user.email="$git_email" merge -q --no-ff -m "release: refresh web pin on current main" "$main" \
    || { say "bump #$n could not replay cleanly onto main; skipping"; return 0; }
  # The refreshed tree is exactly main plus the pin: with a rebuilt hash, that hash.
  if ! cmp -s "$work/refresh-main.lock" "$co/$lock_file"; then
    cp "$work/refresh-main.lock" "$co/$lock_file" \
      && git -C "$co" -c user.name="$git_name" -c user.email="$git_email" commit -q -a --amend --no-edit \
      || { say "bump #$n: could not write the rebuilt hash; skipping"; return 0; }
  fi
  [ "$(git -C "$co" diff --name-only "$main" HEAD)" = "$lock_file" ] \
    && cmp -s "$work/refresh-main.lock" "$co/$lock_file" \
    || { say "bump #$n: the refreshed tree is not main plus the pin; skipping"; return 0; }
  # Re-read both identities and required posture after preparing the commit.
  current="$(api GET "/api/v3/repos/$repo_path/pulls/$n")" \
    && jq -e --argjson row "$row" '.state == "open" and .head == $row.head and .base == $row.base' <<<"$current" >/dev/null \
    && detail="$(api GET "/api/v1/repos/$repo_path/pulls/$n")" \
    && required_failed "$n" "$row" "$detail" \
    && refresh_unqueued "$n" "$detail" \
    && [ "$(remote_ref "$remote" "refs/heads/$branch")" = "$sha" ] \
    && [ "$(remote_ref "$remote" refs/heads/main)" = "$main" ] \
    || { say "bump #$n or main changed during refresh; skipping"; return 0; }
  git -C "$co" push -q "$remote" "HEAD:refs/heads/$branch" \
    || { say "bump #$n refresh push refused; will reconsider next tick"; return 1; }
  [ -n "$rebuilt" ] || refresh_record "$n"
  say "refreshed bump #$n onto main ${main:0:12}$([ -z "$rebuilt" ] || echo ", bundle rebuilt"); its new head requires fresh checks"
}

# duration_seconds 6h|90m|3600 -> seconds; prints nothing for a malformed value.
duration_seconds() {
  case "$1" in
    *h) [[ "${1%h}" =~ ^[0-9]+$ ]] && echo $(( ${1%h} * 3600 )) ;;
    *m) [[ "${1%m}" =~ ^[0-9]+$ ]] && echo $(( ${1%m} * 60 )) ;;
    *) [[ "$1" =~ ^[0-9]+$ ]] && echo "$1" ;;
  esac
  return 0
}
supersede_after="$(duration_seconds "${JERYU_AUTO_PIN_SUPERSEDE_AFTER:-6h}")"
[ -n "$supersede_after" ] || supersede_after=21600
superseded_pr="" superseded_pinned=""
max_refreshes="${JERYU_AUTO_PIN_MAX_REFRESHES:-3}"
[[ "$max_refreshes" =~ ^[0-9]+$ ]] || max_refreshes=3
refresh_after="$(duration_seconds "${JERYU_AUTO_PIN_REFRESH_AFTER:-1h}")"
[ -n "$refresh_after" ] || refresh_after=3600
mkdir -p "$state/refresh"
if [ -n "$mine" ]; then
  waiting_for "$mine"
  refresh_bump "$mine"
  exit $?
fi

if [ -n "$other" ]; then
  if supersede_stale "$other"; then other=""
  else waiting_for "$other"; say "bump #$other is open; ${head:0:12} waits for it to land"; exit 0; fi
fi

pushed="$(remote_ref "$remote" "refs/heads/$branch")"
if [ -z "$pushed" ]; then
  # And it must carry the verdict that moved it: a green <name>/main-health as the newest status
  # of that context. Anything else, or an unreadable status, is a wait, not a pin.
  health_context="${web_path##*/}/main-health"
  health="$(api GET "/api/v3/repos/$web_path/commits/$head/status" \
    | jq -r -L "$here" --arg context "$health_context" 'include "latest-statuses";
        [latest_statuses[] | select(.context == $context)][0].state // "missing"')" || health=unreadable
  if [ "$health" != success ]; then
    waiting_for ""; say "jeryu-web ${head:0:12} has $health_context $health; waiting"; exit 0
  fi
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

shipped="$(git -C "$web_cache" log --format='- %h %s' -n 30 "$pin..$head")" || give_up "could not read shipped commits"
# Keep cross-repository prose and file names out of the review body. Exact
# forge links let the reviewer inspect the full source, without truncating
# messages or terminating a large diff producer early under pipefail.
review_commit_pointers() {
  local commits sha changed
  commits="$(git -C "$web_cache" rev-list -n 30 "$pin..$head")" || return 1
  for sha in $commits; do
    [[ "$sha" =~ ^[0-9a-f]{40}$ ]] || return 1
    changed="$(git -C "$web_cache" diff-tree --no-commit-id --name-only -r "$sha" | awk 'END { print NR }')" || return 1
    printf -- '- [`%s`](%s/repos/jeryu/%s/commit/%s): %s changed paths.\n' "$sha" "$base" "$web_path" "$sha" "$changed"
  done
}
shipped_detail="$(review_commit_pointers)" || give_up "could not prepare shipped commit review pointers"
if [ -z "$pushed" ]; then
  say "building jeryu-web ${head:0:12} (green ref; pin is ${pin:0:12})"
  git -C "$cache" archive "$main" scripts/release "$lock_file" | tar -x -C "$work"
  # Pipes, not 2> >(tee …): a process substitution can still be writing when give_up reads the log.
  pair="$( { JERYU_WEB_REMOTE="$web_remote" "$work/scripts/release/build-web-dist.sh" --commit "$head" "$state/web-dist" \
    2>&1 >&3 | tee -a "$log" >&2; } 3>&1 | tee -a "$log" | tail -1)" || give_up "build-web-dist.sh failed"
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
Opened by auto-pin.sh once jeryu-web's refs/green/main named this commit; only $lock_file changes." 2>>"$log" \
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

contexts="$(api GET "/api/v3/repos/$web_path/commits/$head/status" 2>/dev/null \
  | jq -r -L "$here" 'include "latest-statuses";
      "combined: \(.state); latest contexts: " + (latest_statuses | map("\(.context): \(.state)") | join(", "))' 2>/dev/null)" || contexts="unavailable: native status could not be read or summarized"
body="Pins jeryu-web \`${head:0:7}\`, the commit jeryu-web's \`refs/green/main\` names, so the next staged release ships it. Only \`$lock_file\` changes (\`commit\`, \`web_dist_sha256\`).

Ships (${pin:0:7}..${head:0:7}):
$shipped_detail

**Evidence for review.**
- Verify \`$head\` against jeryu-web main and its native checks/review. The required proof is \`ops/ci/web-pin-proof.sh\` (including \`ops/ci/web-pin-on-main.sh\`), called by \`ops/ci/web.sh\`; check the exact deploy required result and any changes to those scripts or their environment. This template is a pointer, not a gate receipt.
- jeryu-web status snapshot at that exact commit: ${contexts:-not reported}. The commit was taken from jeryu-web's \`refs/green/main\`, which the forge moves only on a green gate run of that commit.
- \`web_dist_sha256\` is not trusted from this diff: staging rebuilds the pinned commit with \`scripts/release/build-web-dist.sh\` and refuses a mismatch; \`crates/jeryu-api/build.rs\` validates the supplied bundle. The required web lane tests the hash guard; it does not perform this real bundle rebuild.

The links above cover at most 30 shipped commits. Full messages and changed files remain on the forge; commit prose is not copied into this review body. The required proof checks forward ancestry from the Deploy base pin and the exact web commit's latest required status. Rollback uses the release rollback procedure.
Opened by \`scripts/release/auto-pin.sh\` because jeryu-web's green ref is ahead of the pin. It never merges: review and the merge queue land this, auto-stage then stages main, and a person deploys."
# Building and pushing may take time: check every open bump again before creating.
list_pulls fresh || give_up "could not re-read open pull requests"
unexpected="$(jq -r --arg old "$superseded_pr" '[.[] | select(.state == "open" and (.title | startswith("release: pin jeryu-web")) and (.number | tostring) != $old)] | length' <<<"$pulls")"
if (( unexpected > 0 )); then
  say "another web pin bump appeared while building; leaving the existing bumps open"; exit 0
fi
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
if [ -n "$superseded_pr" ]; then
  # No close on a failed build or create, and no repeated comment on a refused close.
  if ! list_pulls fresh || ! supersede_stale "$superseded_pr"; then
    say "bump #$superseded_pr is no longer eligible to close; replacement #$number exists, human cleanup required"
    emit pin.bump_failed true "auto-pin:close-ineligible:$superseded_pr:$number" \
      "replacement $repo_path#$number exists; old bump #$superseded_pr changed before close, verify the open bumps" \
      "$(jq -n --arg dep "$web_path" --arg to "$head" '{dependency: $dep, to: $to}')" "$superseded_pr"
    superseded_pr=""
  elif ! echo '{"state":"closed"}' | api PATCH "/api/v3/repos/$repo_path/pulls/$superseded_pr" --data-binary @- >/dev/null 2>&1; then
    say "bump #$superseded_pr could not close after replacement #$number opened; human cleanup required"
    emit pin.bump_failed true "auto-pin:close-refused:$superseded_pr:$number" \
      "replacement $repo_path#$number exists, but old bump #$superseded_pr could not close" \
      "$(jq -n --arg dep "$web_path" --arg to "$head" '{dependency: $dep, to: $to}')" "$superseded_pr"
    superseded_pr=""
  fi
fi
if [ -n "$superseded_pr" ]; then
  jq -n --arg b "Superseded by ${head:0:7} in #$number: the replacement pull request exists and requires its own review and checks. This older bump was not queued, had no requested changes inside the grace period, and either had no approval or its exact-head required gate finished with failure." \
    '{body: $b}' | api POST "/api/v3/repos/$repo_path/issues/$superseded_pr/comments" --data-binary @- >/dev/null 2>&1 \
    || say "bump #$superseded_pr: the comment was not accepted (ignored)"
  emit pin.bump_superseded false "auto-pin:superseded:$superseded_pr:${head:0:12}" \
    "closed $repo_path#$superseded_pr, superseded by #$number (jeryu-web ${head:0:7})" \
    "$(jq -n --arg dep "$web_path" --arg from "$superseded_pinned" --arg to "$head" --argjson old "$superseded_pr" --argjson new "$number" \
      '{dependency: $dep, from: $from, to: $to, superseded_pr: $old, superseding_pr: $new}')" "$number"
fi
emit pin.bump_opened false "auto-pin:opened:${head:0:12}" \
  "opened $repo_path#$number: pin jeryu-web ${head:0:7} ($(grep -c . <<<"$shipped") commits)" \
  "$(jq -n --arg dep "$web_path" --arg from "$pin" --arg to "$head" --arg hash "$hash" \
    '{dependency: $dep, from: $from, to: $to, web_dist_sha256: $hash}')" "$number"
