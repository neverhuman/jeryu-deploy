# shellcheck shell=bash disable=SC2154  # lanes_file, main_specs and forge_cfg belong to collect.sh
# lib.sh — the pieces every family adapter of collect.sh shares: git mirrors, forge reads, the
# board's JSON builders (jeryu.release_board.v1, docs/release-board.md) and the todo join.
#
# Every read is best-effort. A source that cannot be read adds one line to the board's
# `problems` and the stage it feeds says `unverified`; nothing here may stop the rest of the board.

base="${JERYU_BASE:-}"
state="${JERYU_RELEASE_BOARD_STATE:-$HOME/.local/state/jeryu-release-board}"
problems_file=""   # set by collect.sh per family run

say() { printf '%s release-board: %s\n' "$(date -u +%FT%TZ)" "$*" >&2; }

# problem SOURCE MESSAGE — record one unreadable source for this family's board.
problem() {
  say "$1: $2"
  [ -n "$problems_file" ] && jq -cn --arg s "$1" --arg m "$2" '{source: $s, message: $m}' >>"$problems_file"
  return 0
}

# forge_get PATH — GET a forge API path with the collector's token; prints the body or fails.
forge_get() {
  [ -n "${forge_cfg:-}" ] || return 1
  curl --silent --fail --max-time 20 --config "$forge_cfg" -H 'Accept: application/json' "$base$1"
}

# mirror OWNER/REPO — a bare mirror of a forge repo under $state/mirrors, fetched at most once per
# run. Prints its path. A fetch that fails keeps the last copy (reported as a problem).
declare -A fetched=()
mirror() {
  local repo="$1" dir="$state/mirrors/$1.git"
  if [ -z "${fetched[$repo]:-}" ]; then
    fetched[$repo]=1
    if [ ! -d "$dir" ]; then
      mkdir -p "$(dirname "$dir")"
      timeout 300 git clone --quiet --bare "$base/git/$repo.git" "$dir" 2>/dev/null \
        || { problem "git $repo" "could not clone $base/git/$repo.git"; rm -rf "$dir"; return 1; }
    fi
    timeout 180 git -C "$dir" fetch --quiet --prune --tags origin '+refs/heads/*:refs/heads/*' 2>/dev/null \
      || problem "git $repo" "fetch failed; using the copy from the last run"
  fi
  [ -d "$dir" ] && printf '%s\n' "$dir"
}

short() { printf '%s' "${1:0:7}"; }

# behind MIRROR FROM TO — commits in TO that FROM lacks, or empty when either is unknown.
behind() {
  [ -n "$2" ] && [ -n "$3" ] || return 0
  git -C "$1" rev-list --count "$2..$3" 2>/dev/null || true
}

# contains MIRROR ANCESTOR TIP — true when TIP has ANCESTOR (by history).
contains() { git -C "$1" merge-base --is-ancestor "$2" "$3" 2>/dev/null; }

# ---- JSON builders --------------------------------------------------------------------------

# target NAME RUNNING STATE [RUNNERS] [SLOTS] — RUNNERS is an optional JSON array of runner ids
# (as `runnerId` on /api/v1/control-plane/runners); SLOTS an optional JSON array of
# {family, host} worker pools (as they report to /api/v1/shift/heartbeat), for a target run by
# shift slots rather than forge runners. Empty or absent leaves the key out.
target() {
  jq -cn --arg n "$1" --arg r "$2" --arg s "$3" --argjson runners "${4:-[]}" \
    --argjson slots "${5:-[]}" \
    '{name: $n, running: (if $r == "" then null else $r end), state: $s}
     + (if ($runners | length) > 0 then {runners: $runners} else {} end)
     + (if ($slots | length) > 0 then {slots: $slots} else {} end)'
}

# stage KEY=VALUE... — one stage. Keys: id name version state status known parallel never_deployed
# column (one of the board's columns) targets (a JSON array) promote_cmd human_only automatic ships
# (JSON array) missing (missing_json) arrived (arrived_json) rollback forge_repo forge_env.
stage() {
  local -A f=([parallel]=false [never_deployed]=false [targets]='[]' [human_only]=false [automatic]=false [ships]='' [missing]='' [arrived]='[]')
  local kv
  for kv in "$@"; do f["${kv%%=*}"]="${kv#*=}"; done
  jq -cn \
    --arg id "${f[id]}" --arg name "${f[name]}" --arg version "${f[version]:-}" --arg state "${f[state]:-none}" \
    --arg status "${f[status]:-}" --arg known "${f[known]:-unverified}" --arg column "${f[column]:-}" \
    --argjson parallel "${f[parallel]}" --argjson never "${f[never_deployed]}" --argjson targets "${f[targets]}" \
    --arg cmd "${f[promote_cmd]:-}" --argjson human "${f[human_only]}" --argjson auto "${f[automatic]}" \
    --arg ships "${f[ships]}" --arg missing "${f[missing]}" --argjson arrived "${f[arrived]:-[]}" \
    --arg rollback "${f[rollback]:-}" \
    --arg frepo "${f[forge_repo]:-}" --arg fenv "${f[forge_env]:-}" '
    {id: $id, name: $name, version: (if $version == "" then null else $version end), state: $state,
     status: $status, known_by: $known, targets: $targets}
    + (if $parallel then {parallel: true} else {} end)
    + (if $never then {never_deployed: true} else {} end)
    + (if $column != "" then {column: $column} else {} end)
    + (if $cmd != "" then {promote: {command: $cmd, human_only: $human, automatic: $auto}} else {} end)
    + (if $ships != "" then {ships: ($ships | fromjson)} else {} end)
    + (if $missing != "" then {missing: ($missing | fromjson)} else {} end)
    + (if ($arrived | length) > 0 then {arrived: $arrived} else {} end)
    + (if $rollback != "" then {rollback: $rollback} else {} end)
    + (if $frepo != "" then {forge: {repo: $frepo, environment: $fenv}} else {} end)'
}

# lane ID NAME SOURCE OWNER READ_ONLY [group=NAME] STAGE_JSON... — one lane; appended to
# $lanes_file. Neighbouring lanes with the same group are drawn together under its name.
lane() {
  local id="$1" name="$2" source="$3" owner="$4" ro="$5" group=""
  shift 5
  case "${1:-}" in group=*) group="${1#group=}"; shift ;; esac
  printf '%s\n' "$@" | jq -cs --arg id "$id" --arg name "$name" --arg src "$source" --arg owner "$owner" \
    --argjson ro "$ro" --arg group "$group" '{id: $id, name: $name, source: $src, owner_family: $owner, stages: .}
      + (if $group != "" then {group: $group} else {} end)
      + (if $ro then {read_only: true} else {} end)' >>"$lanes_file"
}

# board_columns ID=NAME... — the board's fixed stage columns, left to right; sets $columns_json.
# Every lane is then drawn on one grid, a stage sits in the column its `column=` names, and a
# column a lane skips shows as "not used". Without it each lane is a free track of stages.
board_columns() {
  # shellcheck disable=SC2034  # read by collect.sh
  columns_json="$(printf '%s\n' "$@" | jq -Rcs 'split("\n") | map(select(length > 0)
    | {id: (split("=")[0]), name: (split("=")[1:] | join("="))})')"
}

# worst STATE... — the most serious of several states (bad > warn > ok > none).
worst() {
  local s out=none
  for s in "$@"; do
    case "$s" in
      bad) out=bad ;;
      warn) [ "$out" = bad ] || out=warn ;;
      ok) [ "$out" = none ] && out=ok ;;
    esac
  done
  printf '%s' "$out"
}

# worst_of TARGETS_JSON — the most serious state among a JSON array of targets.
worst_of() {
  local -a states=()
  mapfile -t states < <(jq -r '.[].state' <<<"$1")
  worst "${states[@]}"
}

# lines_json — stdin lines to a JSON array of strings (at most 50).
lines_json() { head -n 50 | jq -Rcs 'split("\n") | map(select(length > 0))'; }

# ---- todo join --------------------------------------------------------------------------------

# todo_trailers MIRROR... — "todo-id<TAB>sha" for every commit on any branch carrying a Todo: trailer.
todo_trailers() {
  local m
  for m in "$@"; do
    git -C "$m" log --all --format='%H%x09%(trailers:key=Todo,valueonly,separator=%x2C)' 2>/dev/null \
      | awk -F'\t' -v m="$m" '$2 != "" { n = split($2, ids, ","); for (i = 1; i <= n; i++) { gsub(/^ +| +$/, "", ids[i]); print ids[i] "\t" $1 "\t" m } }'
  done
}

# has_content MIRROR COMMIT TIP — true when TIP already holds COMMIT's change: by history, or by
# content (the commit's diff reverse-applies to TIP's tree). The content test is what finds work
# on a main that was reset by tree rather than merged.
has_content() {
  local m="$1" c="$2" tip="$3" idx rc
  contains "$m" "$c" "$tip" && return 0
  idx="$(mktemp)"
  GIT_INDEX_FILE="$idx" git -C "$m" read-tree "$tip" 2>/dev/null || { rm -f "$idx"; return 1; }
  git -C "$m" diff --binary "$c^" "$c" 2>/dev/null | GIT_INDEX_FILE="$idx" git -C "$m" apply --cached --check -R 2>/dev/null
  rc=$?
  rm -f "$idx"
  return "$rc"
}

# work_summary FAMILY METHOD UNLINKED PROD_SPEC... — the board's `work` object.
# PROD_SPEC is "MIRROR=TIP": a todo is live when every trailer commit found in that mirror is
# held by that tip. MAIN tips come from $main_specs ("MIRROR=TIP" lines) for `merged`.
work_summary() {
  local family="$1" method="$2" unlinked="$3"
  shift 3
  local -A live_tip=() main_tip=()
  local spec
  for spec in "$@"; do live_tip["${spec%%=*}"]="${spec#*=}"; done
  while IFS= read -r spec; do [ -n "$spec" ] && main_tip["${spec%%=*}"]="${spec#*=}"; done <<<"${main_specs:-}"
  local todos trailers
  todos="$(${JERYU_BOARD_QUEUE_CMD:-todoq} list "$family" --all 2>/dev/null)" || { problem "queue $family" "could not list the queue"; return 0; }
  trailers="$(todo_trailers "${!main_tip[@]}")"
  local live=0 merged=0 stranded=0 untraceable=0 blocked=0 open=0 id status
  while read -r id status _; do
    [ -n "$id" ] || continue
    case "$status" in
      blocked) blocked=$((blocked + 1)); continue ;;
      done) ;;
      *) open=$((open + 1)); continue ;;
    esac
    local commits all_live=1 all_main=1 any=0 c m
    commits="$(awk -F'\t' -v id="$id" '$1 == id { print $2 "\t" $3 }' <<<"$trailers" | sort -u)"
    while IFS=$'\t' read -r c m; do
      [ -n "$c" ] || continue
      any=1
      if [ -z "${live_tip[$m]:-}" ] || ! has_content "$m" "$c" "${live_tip[$m]}"; then all_live=0; fi
      if [ -z "${main_tip[$m]:-}" ] || ! has_content "$m" "$c" "${main_tip[$m]}"; then all_main=0; fi
    done <<<"$commits"
    if [ "$any" = 0 ]; then untraceable=$((untraceable + 1))
    elif [ "$all_live" = 1 ]; then live=$((live + 1))
    elif [ "$all_main" = 1 ]; then merged=$((merged + 1))
    else stranded=$((stranded + 1)); fi
  done <<<"$todos"
  local total=$((live + merged + stranded + untraceable + blocked + open))
  jq -cn --argjson total "$total" --arg method "$method" --arg unlinked "$unlinked" \
    --argjson live "$live" --argjson merged "$merged" --argjson stranded "$stranded" \
    --argjson untraceable "$untraceable" --argjson blocked "$blocked" --argjson open "$open" '
    {total: $total, method: $method, parts: [
      {key: "live", label: "Live", count: $live},
      {key: "merged", label: "Merged, not released", count: $merged},
      {key: "stranded", label: "Stranded on a branch that never reached main", count: $stranded},
      {key: "untraceable", label: "Done, but no commit carries its trailer", count: $untraceable},
      {key: "blocked", label: "Blocked", count: $blocked},
      {key: "open", label: "Open", count: $open}]}
    + (if $unlinked != "" then {unlinked: $unlinked} else {} end)'
}

# ---- what a stage is missing, and what just arrived -------------------------------------------

# todo_titles_json FAMILY — {"<todo id>": "<title>"} for the family's queue, read once per run.
todo_titles_json() {
  local family="$1"
  if [ -z "${todo_titles_cache[$family]:-}" ]; then
    todo_titles_cache[$family]="$(${JERYU_BOARD_QUEUE_CMD:-todoq} list "$family" --all 2>/dev/null \
      | awk '{ id = $1; $1 = $2 = $3 = $4 = $5 = ""; sub(/^ +/, ""); printf "%s\t%s\n", id, $0 }' \
      | jq -Rcs 'split("\n") | map(select(length > 0) | split("\t") | {(.[0]): .[1]}) | add // {}')"
    [ -n "${todo_titles_cache[$family]}" ] || todo_titles_cache[$family]='{}'
  fi
  printf '%s' "${todo_titles_cache[$family]}"
}
declare -gA todo_titles_cache=()

# changes_json FAMILY MIRROR FROM TO — {total, changes[]}: the commits TO holds that FROM does
# not, oldest first, at most 50, each {sha, subject, todo?, todo_title?} (todo from the Todo:
# trailer, its title from the family's queue). Prints nothing when either end is unknown.
changes_json() {
  local family="$1" m="$2" from="$3" to="$4"
  [ -n "$from" ] && [ -n "$to" ] || return 0
  git -C "$m" cat-file -e "$from^{commit}" 2>/dev/null && git -C "$m" cat-file -e "$to^{commit}" 2>/dev/null || return 0
  git -C "$m" log --reverse --topo-order --no-merges \
      --format='%h%x1f%s%x1f%(trailers:key=Todo,valueonly,separator=%x2C)%x1e' "$from..$to" 2>/dev/null \
    | jq -Rcs --argjson titles "$(todo_titles_json "$family")" '
      split("\u001e") | map(sub("^\n"; "") | select(length > 0) | split("\u001f")
        | {sha: .[0], subject: .[1]} + (
            (.[2] // "" | split(",") | map(gsub("^ +| +$"; "")) | map(select(length > 0)) | .[0]) as $todo
            | if $todo then {todo: $todo} + (if $titles[$todo] then {todo_title: $titles[$todo]} else {} end)
              else {} end))
      | {total: length, changes: .[:50]}'
}

# missing_json FAMILY MIRROR REPO FROM_NAME FROM_SHA STAGE_SHA — a stage's `missing` object:
# what FROM_NAME (e.g. main) holds that the stage does not. Empty when either end is unknown.
missing_json() {
  local family="$1" m="$2" repo="$3" from_name="$4" from_sha="$5" stage_sha="$6" c
  c="$(changes_json "$family" "$m" "$stage_sha" "$from_sha")"
  [ -n "$c" ] || return 0
  jq -c --arg from "$from_name" --arg repo "$repo" '{from: $from, repo: $repo} + .' <<<"$c"
}

# arrived_json FAMILY LANE STAGE MIRROR REPO SHA [AT] [PREVIOUS] — a stage's `arrived` array: its
# moves in the last 7 days, newest first (at most 5), each {at, from, to, repo, total, changes}. Each
# run records the stage's commit in $state/history/FAMILY/LANE.STAGE when it moved, at AT (an RFC
# 3339 time the source reports, e.g. a deployment's) or else when the collector first saw it there.
# PREVIOUS, the commit the stage ran before (a deployment's previous release), seeds a stage with no
# history yet, so its last move shows at once instead of after the next one.
arrived_json() {
  local family="$1" lane="$2" stage="$3" m="$4" repo="$5" sha="$6" at_text="${7:-}" previous="${8:-}"
  [ -n "$sha" ] && [ -n "${state:-}" ] || { printf '[]'; return 0; }
  local dir="$state/history/$family" file last now seen
  file="$dir/$lane.$stage"
  mkdir -p "$dir"
  now="$(date +%s)"
  seen="$now"; [ -n "$at_text" ] && seen="$(date -u -d "$at_text" +%s 2>/dev/null || echo "$now")"
  if [ ! -s "$file" ] && [ -n "$previous" ] && [ "$previous" != "$sha" ]; then
    printf '0 %s\n' "$previous" >"$file"
  fi
  last="$(tail -n 1 "$file" 2>/dev/null | cut -d' ' -f2)"
  if [ "$last" != "$sha" ]; then
    printf '%s %s\n' "$seen" "$sha" >>"$file"
    tail -n 50 "$file" >"$file.tmp" && mv "$file.tmp" "$file"
  fi
  local -a lines=()
  mapfile -t lines <"$file"
  local i at from to c out=() since=$((now - 7 * 24 * 3600))
  for ((i = ${#lines[@]} - 1; i >= 1 && ${#out[@]} < 5; i--)); do
    at="${lines[$i]%% *}" to="${lines[$i]#* }" from="${lines[$((i - 1))]#* }"
    [ "$at" -ge "$since" ] || break
    c="$(changes_json "$family" "$m" "$from" "$to")"
    [ -n "$c" ] || continue
    out+=("$(jq -c --arg at "$(date -u -d "@$at" +%FT%TZ)" --arg from "$(short "$from")" \
      --arg to "$(short "$to")" --arg repo "$repo" '{at: $at, from: $from, to: $to, repo: $repo} + .' <<<"$c")")
  done
  if [ ${#out[@]} -eq 0 ]; then printf '[]'; else printf '%s\n' "${out[@]}" | jq -cs .; fi
}
