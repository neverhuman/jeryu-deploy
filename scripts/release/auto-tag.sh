#!/usr/bin/env bash
# auto-tag.sh — cut the next split tag of every hosted crate repository whose green main is ahead
# of its newest tag, and open the jeryu-deploy pin bumps that follow. Run by a timer on the
# release host, beside auto-pin.sh and auto-stage.sh, and never on a gate host: pull request code
# runs there and must never be near this token.
#
# Crate tags used to be cut in batches, so merged crate work waited a median of 108 hours for its
# first tag while server work reached production 2.5 hours after merge. A tag per green main
# commit removes that wait without weakening anything: the tag still only ever names a commit the
# repository's own required gate passed on main.
#
# Per repository, in the dependency order of scripts/release/hosted-crates.tsv:
#   1. Resolve main and the newest `<repo>-v<x.y.z>-split.<n>` tag of the forge repository. The
#      next tag is `split.<n+1>` on the same version line; a new version line is a person's
#      release, and a repository with no split tag at all is left alone.
#   2. A tag whose preserve/hosted-cargo branch is missing or moved is repaired first (the branch
#      is what keeps a tagged commit fetchable for cargo, and a tag without it breaks the
#      consumer's dependency-source gate), before anything new is cut.
#   3. Nothing is cut when main is the tagged commit, when main does not descend from it (somebody
#      rewrote history: `tag.diverged` asks for a person), when the commits since the tag changed
#      no crate source (documentation only, or a Jeryu Split Bot re-render), when the newest tag is
#      younger than the coalescing window (at most one tag per repository per hour), or when main's
#      head does not have `<repo>/required` = success as its latest status.
#   4. The tag and its preserve branch are pushed in one atomic push, so neither can exist without
#      the other, and `tagged.tsv` records the tag only once the forge has it.
#   5. There is no VERSION stamp commit: the version is the tag, and a stamp would add an ungated
#      commit to main for every tag.
# The decision is made here, from the forge's answers, never in the tree being tagged.
#
# Then one bump stack: for every repository whose newest tag is ahead of the pin in jeryu-deploy
# main, a pull request in dependency order, each based on the previous one's branch, so the queue
# lands them in an order main can never see a mismatched pair in. The bump itself is
# bump-hosted-pin.sh taken from jeryu-deploy main (git archive), so the recipe is always the
# reviewed one. It only proposes: review, pr-redteam and the merge queue land it, and deploying
# stays a person's release. A stack is built only when none of its pull requests is already open.
#
# --dry-run prints what it would tag and bump and changes nothing: no tag, no push, no pull
# request, no event and no heartbeat. Only the gate and pull request reads happen, so a dry run
# needs the same read token as a live one.
#
# State in $JERYU_AUTO_TAG_STATE (~/.local/state/jeryu-auto-tag): tagged.tsv (repo, tag, commit,
# time), opened.tsv (repo, tag, PR, time), git caches, logs/ (newest 20).
#
# Env: JERYU_DEPLOY_REMOTE (git URL), JERYU_DEPLOY_REPO (owner/name on the forge; default: from
# the remote); JERYU_CRATE_REMOTE_PREFIX (where `<repo>.git` lives; default the hosted forge);
# JERYU_BASE (must be https://git.neverhuman.org: the token is sent nowhere else);
# JERYU_TAG_TOKEN_FILE (required, no default: releasebot's agent token, a regular 0600 file);
# JERYU_TAG_GIT_NAME / JERYU_TAG_GIT_EMAIL (required, no default: the identity tags and bump
# commits are made as. A credential path and an automation identity belong to one installation,
# and this source is public, so none of the three has a default);
# JERYU_AUTO_TAG_COALESCE (1h; also 90m or seconds); JERYU_AUTO_TAG_RERENDER_EMAIL
# (split-bot@localhost: the author whose re-render commits do not earn a tag);
# JERYU_AUTO_TAG_EVENTS=0 turns the events off; JERYU_AUTO_TAG_BEAT=0 turns the heartbeat off.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
dry=0
case "${1:-}" in
  --dry-run) dry=1; shift ;;
  "") ;;
  *) echo "usage: auto-tag.sh [--dry-run]" >&2; exit 64 ;;
esac
[[ $# == 0 ]] || { echo "usage: auto-tag.sh [--dry-run]" >&2; exit 64; }

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
remote="${JERYU_DEPLOY_REMOTE:-https://git.neverhuman.org/git/jeryu/jeryu-deploy.git}"
crate_prefix="${JERYU_CRATE_REMOTE_PREFIX:-https://git.neverhuman.org/git/jeryu}"; crate_prefix="${crate_prefix%/}"
base="${JERYU_BASE:-https://git.neverhuman.org}"; base="${base%/}"
token_file="${JERYU_TAG_TOKEN_FILE:-}"
git_name="${JERYU_TAG_GIT_NAME:-}" git_email="${JERYU_TAG_GIT_EMAIL:-}"
rerender_email="${JERYU_AUTO_TAG_RERENDER_EMAIL:-split-bot@localhost}"
state="${JERYU_AUTO_TAG_STATE:-$HOME/.local/state/jeryu-auto-tag}"
pin_policy=.cargo/hosted-pin-refs.tsv
order_file=scripts/release/hosted-crates.tsv
say() { echo "[auto-tag $(date -u +%H:%M:%S)] $*" >&2; }
repo_of() { sed -E 's#^https?://[^/]+/git/##; s#\.git$##' <<<"$1"; }
repo_path="${JERYU_DEPLOY_REPO:-$(repo_of "$remote")}"
owner="${repo_path%%/*}"

[[ "$base" == https://git.neverhuman.org ]] || { say "refusing a noncanonical credential origin"; exit 2; }
[[ -n "$token_file" ]] || { say "JERYU_TAG_TOKEN_FILE is unset: set it to releasebot's 0600 credential file"; exit 2; }
[[ -n "$git_name" && -n "$git_email" ]] \
  || { say "JERYU_TAG_GIT_NAME and JERYU_TAG_GIT_EMAIL are unset: set them to the identity tags are made as"; exit 2; }
[[ "$repo_path" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || { say "not an owner/name repository: $repo_path"; exit 2; }

# duration_seconds 1h|90m|3600 -> seconds; prints nothing for a malformed value.
duration_seconds() {
  case "$1" in
    *h) [[ "${1%h}" =~ ^[0-9]+$ ]] && echo $(( ${1%h} * 3600 )) ;;
    *m) [[ "${1%m}" =~ ^[0-9]+$ ]] && echo $(( ${1%m} * 60 )) ;;
    *) [[ "$1" =~ ^[0-9]+$ ]] && echo "$1" ;;
  esac
  return 0
}
coalesce="$(duration_seconds "${JERYU_AUTO_TAG_COALESCE:-1h}")"
[ -n "$coalesce" ] || coalesce=3600

mkdir -p "$state/logs"
exec 9>"$state/lock"
flock -n 9 || { say "another run holds the lock"; exit 0; }
touch "$state/tagged.tsv" "$state/opened.tsv"

# The token reaches curl through a 0600 config file, never through argv, and is never printed.
work="$(mktemp -d)"; cfg="$work/curl.cfg"; : >"$cfg"; chmod 600 "$cfg"
beat_last="" token_problem=""
trap 'beat || true; rm -rf "$work"' EXIT
if [[ -f "$token_file" && ! -L "$token_file" && "$(stat -c '%a' "$token_file")" == 600 ]]; then
  token="$(cat "$token_file")"
  if [[ "$token" =~ ^[A-Za-z0-9._~+/-]+=*$ ]]; then printf 'header = "Authorization: Bearer %s"\n' "$token" >"$cfg"
  else token_problem="the token has an invalid bearer-token shape"; fi
  unset token
else
  token_problem="the token must be a regular mode-0600 file"
fi

api() { # METHOD PATH [curl args] [JSON on stdin] -> body on stdout
  local method="$1" path="$2"; shift 2
  curl --silent --show-error --fail --max-time 60 --config "$cfg" -X "$method" \
    -H 'Accept: application/json' -H 'Content-Type: application/json' "$@" "$base$path"
}

# did REPO CONCLUSION PR SHA FINISHED_AT -> a heartbeat `last`: what this timer most recently did.
did() {
  jq -cn --arg repo "$1" --arg conclusion "$2" --arg pr "$3" --arg sha "$4" --arg at "$5" '
    {repo: $repo, sha: $sha, recipe: "auto-tag", conclusion: $conclusion, seconds: 0, finishedAt: $at}
    + (if $pr != "" then {pr: ($pr | tonumber)} else {} end)'
}
# beat: one runner heartbeat (POST /api/v1/runners/heartbeat, docs/pipeline-events.md), sent by
# the EXIT trap so idle ticks, early exits and finished work all report. `last` is what this tick
# did, else the newest tag it cut; with no history there is no `last`. A dry run beats nothing.
# Best-effort: at most one line, and never the tick's exit status.
beat() {
  [ "${JERYU_AUTO_TAG_BEAT:-1}" = 1 ] && (( ! dry )) || return 0
  [ -z "$token_problem" ] || { say "heartbeat: skipped ($token_problem)"; return 0; }
  local host last="$beat_last" repo tag sha at body
  host="$(hostname -s)" || return 0
  if [ -z "$last" ] && IFS=$'\t' read -r repo tag sha at < <(tail -n 1 "$state/tagged.tsv"); then
    last="$(did "$owner/$repo" tagged "" "$sha" "$at")" || last=""
  fi
  body="$(jq -cn --arg host "$host" --argjson last "${last:-null}" '
    {runnerId: ($host + "/auto-tag"), host: $host, slot: 0, labels: ["automation"], intervalSeconds: 600}
    + (if $last != null then {last: $last} else {} end)')" || { say "heartbeat: could not build (ignored)"; return 0; }
  curl --silent --fail --max-time 10 --config "$cfg" -X POST -H 'Accept: application/json' \
    -H 'Content-Type: application/json' --data-binary @- \
    "$base/api/v1/runners/heartbeat" <<<"$body" >/dev/null 2>&1 || say "heartbeat: not accepted (ignored)"
}

# emit KIND NEEDS_HUMAN EVENT_ID SUMMARY DETAIL_JSON SHA [PR] [REASON]
# One pipeline event; event_id makes a repeat a no-op on the forge. A dry run emits nothing.
# Best-effort: logged, ignored.
emit() {
  [ "${JERYU_AUTO_TAG_EVENTS:-1}" = 1 ] && (( ! dry )) || return 0
  [ -z "$token_problem" ] || return 0
  local kind="$1" needs_human="$2" event_id="$3" summary="$4" detail="$5" sha="$6" pr="${7:-}" reason="${8:-}" body
  body="$(jq -n --arg kind "$kind" --argjson needs_human "$needs_human" --arg event_id "$event_id" \
    --arg summary "$summary" --argjson detail "$detail" --arg pr "$pr" --arg reason "$reason" \
    --arg sha "$sha" --arg repo "$event_repo" '
    {source: "auto-tag", kind: $kind, event_id: $event_id, needs_human: $needs_human,
     summary: $summary, repo: $repo, sha: $sha, detail: $detail, actor: "jeryu-auto-tag"}
    + (if $pr != "" then {pr: ($pr | tonumber)} else {} end)
    + (if $reason != "" then {reason: $reason} else {} end)')" \
    || { say "event: could not build $kind (ignored)"; return 0; }
  api POST /api/v1/events --data-binary @- <<<"$body" 2>/dev/null | jq -e '.ok == true' >/dev/null 2>&1 \
    || say "event: posting $kind was not acknowledged (ignored)"
}
event_repo="$repo_path"

main="$(git ls-remote "$remote" refs/heads/main | cut -f1)"
[[ "$main" =~ ^[0-9a-f]{40}$ ]] || { say "could not resolve jeryu-deploy main ($main)"; exit 1; }
cache="$state/jeryu-deploy.git"
[ -d "$cache" ] || git clone -q --bare "$remote" "$cache"
git -C "$cache" fetch -q "$remote" "+refs/heads/main:refs/heads/main"

# The repositories to tag, and the order their bumps stack in, both from jeryu-deploy main: the
# reviewed pin policy and the reviewed order must name exactly the same set, or this tick does
# nothing rather than guess which of the two is right.
order_rows="$(git -C "$cache" show "$main:$order_file" | awk '!/^[[:space:]]*(#|$)/ { print $1, $2 }')"
pin_rows="$(git -C "$cache" show "$main:$pin_policy" | awk -F '|' '!/^[[:space:]]*(#|$)/ { print $1, $2, $3 }')"
[ -n "$order_rows" ] && [ -n "$pin_rows" ] || { say "$order_file or $pin_policy at main ${main:0:12} is empty"; exit 1; }
repos=()
while read -r repo rank; do
  [[ "$repo" =~ ^jeryu-[a-z0-9-]+$ ]] || { say "$order_file names a malformed repository: $repo"; exit 1; }
  [[ "$rank" =~ ^[0-9]+$ ]] || { say "$order_file gives $repo a malformed rank: $rank"; exit 1; }
  repos+=("$rank $repo")
done <<<"$order_rows"
mapfile -t ordered < <(printf '%s\n' "${repos[@]}" | sort -k1,1n -k2,2 | awk '{ print $2 }')
[[ "$(printf '%s\n' "${ordered[@]}" | LC_ALL=C sort)" == "$(awk '{ print $1 }' <<<"$pin_rows" | LC_ALL=C sort)" ]] \
  || { say "$order_file and $pin_policy name different repositories; leaving them for a person"; exit 1; }
pinned_tag_of() { awk -v repo="$1" '$1 == repo { print $2 }' <<<"$pin_rows"; }
pinned_commit_of() { awk -v repo="$1" '$1 == repo { print $3 }' <<<"$pin_rows"; }

[ -z "$token_problem" ] || { say "$token_problem"; exit 2; }

# tag_rows REPO CACHE -> "<version> <n> <tag> <commit>" for every split tag of the repository,
# peeled to the commit an annotated tag names. The ls-remote answer is the forge's, so a tag is
# never read from a working tree.
tag_rows() {
  local repo="$1" refs
  refs="$(git ls-remote --tags "$crate_prefix/$repo.git" 2>/dev/null)" || return 1
  awk -v repo="$repo" '
    { ref = $2; sha = $1
      if (ref ~ /\^\{\}$/) { sub(/\^\{\}$/, "", ref); peel[ref] = sha; next }
      obj[ref] = sha; order[++k] = ref }
    END {
      pattern = "^refs/tags/" repo "-v[0-9]+\\.[0-9]+\\.[0-9]+-split\\.[0-9]+$"
      for (i = 1; i <= k; i++) {
        ref = order[i]
        if (ref !~ pattern) continue
        name = substr(ref, 11)
        rest = substr(name, length(repo) + 3)
        split(rest, part, "-split.")
        print part[1], part[2] + 0, name, (ref in peel) ? peel[ref] : obj[ref]
      }
    }' <<<"$refs"
}

# source_changed CACHE FROM TO -> 0 when the commits in FROM..TO changed a crate source. A commit
# that only moves documentation, and a Jeryu Split Bot re-render, earn no tag; anything git cannot
# answer counts as changed, so a tag is never skipped on a guess.
source_changed() {
  local dir="$1" from="$2" to="$3" commit author subject path
  local commits
  commits="$(git -C "$dir" rev-list --max-count=500 "$from..$to" 2>/dev/null)" || return 0
  [ -n "$commits" ] || return 1
  for commit in $commits; do
    author="$(git -C "$dir" log -1 --format='%ae' "$commit" 2>/dev/null)" || return 0
    subject="$(git -C "$dir" log -1 --format='%s' "$commit" 2>/dev/null)" || return 0
    if [ "$author" = "$rerender_email" ] && [[ "$subject" == "chore(jankurai): re-render"* ]]; then continue; fi
    while read -r path; do
      [ -n "$path" ] || continue
      case "$path" in
        docs/*|*.md|LICENSE|images/*) ;;
        *) return 0 ;;
      esac
    done < <(git -C "$dir" diff-tree --no-commit-id --name-only -r "$commit" 2>/dev/null || echo UNREADABLE)
  done
  return 1
}

# gate_success REPO SHA: the forge's latest `<repo>/required` status on that commit is success.
# The latest per context, by its own timestamp, so an older green never outvotes a newer red.
gate_success() {
  local repo="$1" sha="$2" status
  status="$(api GET "/api/v3/repos/$owner/$repo/commits/$sha/status" 2>/dev/null)" || return 1
  jq -e -L "$here" --arg context "$repo/required" 'include "latest-statuses";
    .state == "success" and any(latest_statuses[]; .context == $context and .state == "success")' \
    <<<"$status" >/dev/null 2>&1
}

tagged_this_tick=() behind=()
for repo in "${ordered[@]}"; do
  event_repo="$owner/$repo"
  remote_repo="$crate_prefix/$repo.git"
  head="$(git ls-remote "$remote_repo" refs/heads/main 2>/dev/null | cut -f1)" || head=""
  [[ "$head" =~ ^[0-9a-f]{40}$ ]] || { say "$repo: could not resolve main; skipping"; continue; }
  rows="$(tag_rows "$repo")" || { say "$repo: could not list its tags; skipping"; continue; }
  newest="$(sort -k1,1V -k2,2n <<<"$rows" | tail -n 1)"
  [ -n "$newest" ] || { say "$repo: has no <repo>-v<x.y.z>-split.<n> tag; leaving it for a person"; continue; }
  read -r version number last_tag last_commit <<<"$newest"
  [[ "$last_commit" =~ ^[0-9a-f]{40}$ ]] || { say "$repo: $last_tag does not name a commit; leaving it for a person"; continue; }
  crate_cache="$state/$repo.git"
  [ -d "$crate_cache" ] || git clone -q --bare "$remote_repo" "$crate_cache"
  git -C "$crate_cache" fetch -q --prune --prune-tags "$remote_repo" \
    "+refs/heads/main:refs/heads/main" "+refs/tags/*:refs/tags/*" 2>/dev/null \
    || { say "$repo: could not fetch main and tags; skipping"; continue; }

  # A tag whose support branch is missing or moved is repaired before anything new is cut: the
  # consumer's dependency-source gate reads that branch back, and cargo fetches the tag through it.
  support="refs/heads/preserve/hosted-cargo/$last_tag"
  advertised="$(git ls-remote "$remote_repo" "$support" 2>/dev/null | cut -f1)" || advertised=""
  if [ "$advertised" != "$last_commit" ]; then
    if (( dry )); then
      say "$repo: would push $support at ${last_commit:0:12} (it is ${advertised:-absent})"
    elif git -C "$crate_cache" push -q "$remote_repo" "$last_commit:$support" 2>/dev/null; then
      say "$repo: pushed the missing $support at ${last_commit:0:12}"
      emit tag.preserve_repaired false "auto-tag:preserve:$repo:$last_tag" \
        "$owner/$repo $last_tag was missing its preserve branch; it now points at ${last_commit:0:7}" \
        "$(jq -n --arg tag "$last_tag" --arg ref "$support" '{tag: $tag, support_ref: $ref}')" "$last_commit"
    else
      say "$repo: could not push $support for $last_tag; leaving it for a person"
      emit tag.preserve_missing true "auto-tag:preserve-failed:$repo:$last_tag" \
        "$owner/$repo $last_tag has no usable preserve branch, and it could not be pushed" \
        "$(jq -n --arg tag "$last_tag" --arg ref "$support" '{tag: $tag, support_ref: $ref}')" "$last_commit" \
        "" "a hosted crate pin needs refs/heads/preserve/hosted-cargo/<tag> to stay fetchable"
      continue
    fi
  fi

  if [ "$head" != "$last_commit" ]; then
    if ! git -C "$crate_cache" merge-base --is-ancestor "$last_commit" "$head" 2>/dev/null; then
      say "$repo: main ${head:0:12} does not descend from $last_tag; leaving it for a person"
      emit tag.diverged true "auto-tag:diverged:$repo:${head:0:12}" \
        "$owner/$repo main ${head:0:7} does not descend from $last_tag; no tag was cut" \
        "$(jq -n --arg tag "$last_tag" --arg from "$last_commit" --arg to "$head" '{tag: $tag, from: $from, to: $to}')" \
        "$head" "" "a rewritten history is never tagged automatically"
    elif ! source_changed "$crate_cache" "$last_commit" "$head"; then
      say "$repo: nothing but documentation or a re-render since $last_tag; no tag"
    else
      age=""
      tagged_at="$(git -C "$crate_cache" for-each-ref --format='%(taggerdate:unix)' "refs/tags/$last_tag" 2>/dev/null)" || tagged_at=""
      [ -n "$tagged_at" ] || tagged_at="$(git -C "$crate_cache" log -1 --format='%ct' "$last_commit" 2>/dev/null)" || tagged_at=""
      if [[ "$tagged_at" =~ ^[0-9]+$ ]]; then age=$(( $(date -u +%s) - tagged_at )); fi
      if [ -n "$age" ] && (( age < coalesce )); then
        say "$repo: $last_tag is ${age}s old; coalescing until the window (${coalesce}s) passes"
      elif ! gate_success "$repo" "$head"; then
        say "$repo: main ${head:0:12} does not have $repo/required success; waiting"
      else
        next_tag="$repo-v$version-split.$(( number + 1 ))"
        if [ -n "$(git ls-remote "$remote_repo" "refs/tags/$next_tag" 2>/dev/null)" ]; then
          say "$repo: $next_tag already exists on the forge; leaving this repository for a person"
        elif (( dry )); then
          say "$repo: would tag ${head:0:12} as $next_tag, with $support's successor"
          tagged_this_tick+=("$repo $next_tag $head")
          # Say what the bump after it would be, not what the stale pin would be.
          last_tag="$next_tag" last_commit="$head"
        else
          support_next="refs/heads/preserve/hosted-cargo/$next_tag"
          if git -C "$crate_cache" -c user.name="$git_name" -c user.email="$git_email" \
               tag -a -m "$next_tag: $repo main at ${head:0:12}, cut by the release automation once $repo/required passed." \
               "$next_tag" "$head" 2>/dev/null \
             && git -C "$crate_cache" push -q --atomic "$remote_repo" \
               "$head:$support_next" "refs/tags/$next_tag:refs/tags/$next_tag" 2>/dev/null; then
            printf '%s\t%s\t%s\t%s\n' "$repo" "$next_tag" "$head" "$(date -u +%FT%TZ)" >>"$state/tagged.tsv"
            say "TAGGED $owner/$repo ${head:0:12} as $next_tag"
            beat_last="$(did "$owner/$repo" tagged "" "$head" "$(date -u +%FT%TZ)")" || beat_last=""
            emit tag.cut false "auto-tag:cut:$repo:$next_tag" \
              "cut $owner/$repo $next_tag at ${head:0:7}" \
              "$(jq -n --arg tag "$next_tag" --arg from "$last_tag" --arg ref "$support_next" \
                '{tag: $tag, from_tag: $from, support_ref: $ref}')" "$head"
            last_tag="$next_tag" last_commit="$head"
            tagged_this_tick+=("$repo $next_tag $head")
          else
            git -C "$crate_cache" tag -d "$next_tag" >/dev/null 2>&1 || true
            say "$repo: could not cut $next_tag at ${head:0:12}; nothing was pushed"
            emit tag.failed true "auto-tag:failed:$repo:${head:0:12}" \
              "could not cut $owner/$repo $next_tag at ${head:0:7}" \
              "$(jq -n --arg tag "$next_tag" --arg to "$head" '{tag: $tag, to: $to}')" "$head" \
              "" "the tag and its preserve branch are pushed together; neither landed"
          fi
        fi
      fi
    fi
  fi

  # What jeryu-deploy main pins against what the forge now has.
  pinned_tag="$(pinned_tag_of "$repo")" pinned_commit="$(pinned_commit_of "$repo")"
  [[ "$pinned_tag" =~ ^${repo}-v[0-9]+\.[0-9]+\.[0-9]+-split\.[0-9]+$ && "$pinned_commit" =~ ^[0-9a-f]{40}$ ]] \
    || { say "$repo: $pin_policy at main ${main:0:12} has no well-formed pin; not bumping it"; continue; }
  [ "$pinned_tag" != "$last_tag" ] || continue
  if [ "${last_tag%-split.*}" != "${pinned_tag%-split.*}" ] \
     || (( ${last_tag##*-split.} <= ${pinned_tag##*-split.} )); then
    say "$repo: main pins $pinned_tag and the newest tag is $last_tag; a person decides that bump"
    continue
  fi
  behind+=("$repo $last_tag $last_commit $pinned_tag")
done

event_repo="$repo_path"
[ "${#tagged_this_tick[@]}" -gt 0 ] || say "no tag was cut this tick"
if [ "${#behind[@]}" == 0 ]; then
  (( dry )) && say "dry run: every hosted crate pin is current"
  exit 0
fi

# list_pulls: every pull request of jeryu-deploy. Every page: the forge lists oldest first and
# ignores sort and head filters, so page 1 alone stops showing new pull requests once the repo has
# more than 100. Returns 1, having said why, on failure.
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

list_pulls || exit 1
open_crate_bumps="$(jq -r '[.[] | select(.state == "open" and (.title | startswith("release: pin jeryu-")) and ((.title | startswith("release: pin jeryu-web")) | not))] | length' <<<"$pulls")"
if (( open_crate_bumps > 0 )); then
  say "$open_crate_bumps crate pin bump pull requests are open; the next stack waits for them to land"
  exit 0
fi

say "behind: $(printf '%s ' "${behind[@]}")"
if (( dry )); then
  for row in "${behind[@]}"; do
    read -r repo tag commit pinned <<<"$row"
    say "would open a bump pull request: $repo $pinned -> $tag (${commit:0:12})"
  done
  exit 0
fi

# One checkout for the whole stack: each bump is cut from the previous branch, so the queue lands
# them in dependency order and main never sees a half-bumped pair.
co="$work/checkout"
git clone -q --no-hardlinks "$cache" "$co" 2>/dev/null || { say "could not clone the jeryu-deploy cache"; exit 1; }
git -C "$co" checkout -q "$main" 2>/dev/null || { say "could not check out main ${main:0:12}"; exit 1; }
mkdir -p "$work/recipe"
git -C "$cache" archive "$main" scripts/release | tar -x -C "$work/recipe" \
  || { say "could not take the bump recipe from main"; exit 1; }
recipe="$work/recipe/scripts/release/bump-hosted-pin.sh"
[ -f "$recipe" ] || { say "main ${main:0:12} has no scripts/release/bump-hosted-pin.sh"; exit 1; }

base_ref=main opened_any=0 stack_broke=0
for row in "${behind[@]}"; do
  read -r repo tag commit pinned <<<"$row"
  log="$state/logs/$repo-$tag.log"; : >"$log"
  branch="auto/pin-$repo-${tag##*-split.}-${commit:0:12}"
  if [ -z "$(git ls-remote "$remote" "refs/heads/$branch" 2>/dev/null)" ]; then
    git -C "$co" checkout -q -B "$branch" 2>>"$log" || { say "$repo: could not cut $branch"; stack_broke=1; break; }
    if ! ( cd "$co" && bash "$recipe" "$repo" "$tag" "$commit" ) >>"$log" 2>&1; then
      say "$repo: the bump recipe refused $pinned -> $tag; see $log"
      emit pin.bump_failed true "auto-tag:bump-failed:$repo:$tag" \
        "the $owner/$repo pin bump to $tag could not be prepared" \
        "$(jq -n --arg dep "$owner/$repo" --arg from "$pinned" --arg to "$tag" '{dependency: $dep, from: $from, to: $to}')" \
        "$commit" "" "bump-hosted-pin.sh refused the edit; a person has to move this pin"
      stack_broke=1; break
    fi
    git -C "$co" -c user.name="$git_name" -c user.email="$git_email" commit -q -am "release: pin $repo $tag

Moves the hosted $repo pin from $pinned to $tag (${commit:0:12}): the manifests,
Cargo.lock's git source identity and .cargo/hosted-pin-refs.tsv, and nothing else.

Prepared by scripts/release/bump-hosted-pin.sh, taken from jeryu-deploy main, after
auto-tag.sh cut $tag from a $repo main whose $repo/required passed." 2>>"$log" \
      || { say "$repo: could not commit the bump"; stack_broke=1; break; }
    git -C "$co" push -q "$remote" "HEAD:refs/heads/$branch" 2>>"$log" \
      || { say "$repo: could not push $branch"; stack_broke=1; break; }
  else
    say "$repo: $branch is already pushed; opening its pull request"
    if ! { git -C "$co" fetch -q "$remote" "+refs/heads/$branch:refs/heads/$branch" 2>>"$log" \
           && git -C "$co" checkout -q "$branch" 2>>"$log"; }; then
      say "$repo: could not read the pushed $branch"; stack_broke=1; break
    fi
  fi

  # No open bump of this repository can exist here: the tick exits above while one does. A pull
  # request on this branch is therefore one somebody already decided, and the stack stops rather
  # than reopen it or stack the next bump on a branch that will not land.
  existing="$(jq -r --arg b "$branch" '[.[] | select(.head.ref == $b)][0].number // empty' <<<"$pulls")"
  if [ -n "$existing" ]; then
    say "$repo: #$existing already decided the bump to $tag; leaving it and the rest of the stack for a person"
    stack_broke=1; break
  fi
  body="Moves the hosted \`$repo\` pin from \`$pinned\` to \`$tag\`, at \`$commit\`.

Changed: the \`tag =\` lines whose \`git\` URL names \`$repo\`, the \`Cargo.lock\` git source identity, and the \`.cargo/hosted-pin-refs.tsv\` row (tag, commit, \`refs/heads/preserve/hosted-cargo/$tag\`). \`jeryu-split.lock.toml\` is not a cargo pin and is not touched.

**Evidence for review.**
- The tag: [\`$tag\`]($base/repos/$owner/$repo/src/tag/$tag) at [\`$commit\`]($base/repos/$owner/$repo/commit/$commit). \`auto-tag.sh\` cut it from \`$repo\` main only after the forge reported \`$repo/required\` success on exactly that commit, and pushed the tag and its \`preserve/hosted-cargo\` branch in one atomic push.
- \`ops/ci/dependency-sources.sh\` is the required proof: it reads the tag and the support branch back from the forge and refuses a pin whose Cargo.lock inventory, tag target or support ref disagrees. This body is a pointer, not a gate receipt.
- Commit prose from \`$repo\` is not copied here; read the commits on the forge.

$([ "$base_ref" = main ] && printf 'Based on main.' || printf 'Based on `%s`, the bump before it in dependency order: the queue lands them in that order so main never pins a mismatched pair.' "$base_ref")
Opened by \`scripts/release/auto-tag.sh\`. It never merges: review, pr-redteam and the merge queue land this, and deploying stays a person's release."
  opened="$(jq -n --arg title "release: pin $repo $tag" --arg head "$branch" --arg base "$base_ref" --arg body "$body" \
    '{title: $title, head: $head, base: $base, body: $body}' \
    | api POST "/api/v3/repos/$repo_path/pulls" --data-binary @- 2>>"$log")" \
    || { say "$repo: the forge refused the pull request for $branch"; stack_broke=1; break; }
  number="$(jq -r '.number // empty' <<<"$opened" 2>/dev/null)" || number=""
  [[ "$number" =~ ^[0-9]+$ ]] || { say "$repo: the forge did not answer with a pull request number"; stack_broke=1; break; }
  printf '%s\t%s\t%s\t%s\n' "$repo" "$tag" "$number" "$(date -u +%FT%TZ)" >>"$state/opened.tsv"
  say "OPENED $repo_path#$number: release: pin $repo $tag (base $base_ref)"
  beat_last="$(did "$repo_path" opened "$number" "$commit" "$(date -u +%FT%TZ)")" || beat_last=""
  emit pin.bump_opened false "auto-tag:opened:$repo:$tag" \
    "opened $repo_path#$number: pin $repo $tag" \
    "$(jq -n --arg dep "$owner/$repo" --arg from "$pinned" --arg to "$tag" --arg commit "$commit" --arg base "$base_ref" \
      '{dependency: $dep, from: $from, to: $to, commit: $commit, base: $base}')" "$commit" "$number"
  opened_any=1
  base_ref="$branch"
done

find "$state/logs" -maxdepth 1 -type f -name '*.log' -printf '%T@ %p\n' | sort -rn | tail -n +21 \
  | cut -d' ' -f2- | xargs -r rm -f
(( opened_any )) || say "no bump pull request was opened this tick"
(( ! stack_broke )) || exit 1
