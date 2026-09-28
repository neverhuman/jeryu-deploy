#!/usr/bin/env bash
# collect.sh — build each family's release board (jeryu.release_board.v1, docs/release-board.md)
# and PUT it to the forge, where /releases renders it. Read-only everywhere else: it fetches
# its own git mirrors and asks each source what it runs, and changes none of them.
#
#   collect.sh [--push] [--trigger timer|release|manual] [--out DIR] FAMILY...
#   collect.sh --push --trigger release acme     # what a release script runs when it ends
#
# What a family's board reads is site configuration, not part of jeryu: one adapter per family,
# $JERYU_RELEASE_BOARD_FAMILIES/<family>.sh (default ~/.config/jeryu/release-board/families),
# written against lib.sh. scripts/release-board/examples/ has one to copy. `all` means every
# adapter in that directory. Without --push the boards are only written to --out (default
# $JERYU_RELEASE_BOARD_STATE/boards/<family>.json), which is how to look at one first.
#
# Settings come from the environment, and any that are unset from $JERYU_RELEASE_BOARD_ENV
# (default ~/.config/jeryu/release-board.env, KEY=VALUE lines), so a release script that starts a
# refresh needs no configuration of its own. A value already set always wins over the file.
# Env: JERYU_BASE (the forge's URL; required);
# JERYU_BOARD_TOKEN_FILE (a token of a forge admin or a JERYU_BOARD_REPORTERS login; default
# ~/.config/jeryu/release-board.token); JERYU_RELEASE_BOARD_FAMILIES; JERYU_RELEASE_BOARD_STATE
# (~/.local/state/jeryu-release-board: mirrors, boards, lock); JERYU_BOARD_QUEUE_CMD (the todo
# queue's command, default todoq).
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Fill unset settings from the env file: read as KEY=VALUE lines, never run as shell, and never
# over a value the caller already set.
env_file="${JERYU_RELEASE_BOARD_ENV:-$HOME/.config/jeryu/release-board.env}"
if [ -r "$env_file" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    [[ "$line" =~ ^[[:space:]]*([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] || continue
    key="${BASH_REMATCH[1]}" value="${BASH_REMATCH[2]}"
    value="${value%\"}"; value="${value#\"}"; value="${value%\'}"; value="${value#\'}"
    [ -n "${!key+set}" ] || export "$key=$value"
  done <"$env_file"
fi

# shellcheck source=scripts/release-board/lib.sh
. "$here/lib.sh"

push=0 trigger=manual out=""
families_dir="${JERYU_RELEASE_BOARD_FAMILIES:-$HOME/.config/jeryu/release-board/families}"
families=()
while [ $# -gt 0 ]; do
  case "$1" in
    --push) push=1 ;;
    --trigger) trigger="${2:?--trigger needs a value}"; shift ;;
    --out) out="${2:?--out needs a directory}"; shift ;;
    all) for f in "$families_dir"/*.sh; do [ -e "$f" ] && families+=("$(basename "$f" .sh)"); done ;;
    -*) say "unknown option $1"; exit 2 ;;
    *) families+=("$1") ;;
  esac
  shift
done
case "$trigger" in timer|release|manual) ;; *) say "--trigger must be timer, release or manual"; exit 2 ;; esac
[ ${#families[@]} -gt 0 ] || { say "no family to collect: name one, or put adapters in $families_dir (see scripts/release-board/examples/)"; exit 2; }
[ -n "$base" ] || { say "JERYU_BASE is not set: put the forge's URL in ~/.config/jeryu/release-board.env"; exit 2; }
out="${out:-$state/boards}"
mkdir -p "$state" "$out"

# One run at a time; a release trigger waits for a timer run instead of skipping, so the push
# that follows a release always sees it.
exec 9>"$state/lock"
if [ "$trigger" = release ]; then flock -w 600 9 || { say "timed out waiting for another run"; exit 1; }
else flock -n 9 || { say "another run holds the lock"; exit 0; }; fi

token_file="${JERYU_BOARD_TOKEN_FILE:-$HOME/.config/jeryu/release-board.token}"
forge_cfg="" work=""
trap 'rm -f "$forge_cfg"; rm -rf "$work"' EXIT
if [ -r "$token_file" ]; then
  forge_cfg="$(mktemp)"; chmod 600 "$forge_cfg"
  printf 'header = "Authorization: Bearer %s"\n' "$(cat "$token_file")" >"$forge_cfg"
else
  say "no readable token at $token_file: forge reads are skipped and nothing can be pushed"
fi
work="$(mktemp -d)"
version="$(git -C "$here" rev-parse --short=12 HEAD 2>/dev/null || cat "$here/VERSION" 2>/dev/null || echo unknown)"

status=0
for family in "${families[@]}"; do
  adapter="$families_dir/$family.sh"
  [ -r "$adapter" ] || { say "no adapter for family $family in $families_dir"; status=2; continue; }
  started="$(date +%s%3N)"
  lanes_file="$work/$family.lanes" problems_file="$work/$family.problems"
  summary="" work_json="null" pins_json="null" notes_json="null" columns_json="null" main_specs=""
  : >"$lanes_file"; : >"$problems_file"
  # shellcheck source=/dev/null
  . "$adapter"
  "collect_${family//-/_}" || problem "$family" "the adapter stopped early; the board is partial"
  if [ ! -s "$lanes_file" ]; then
    say "$family: no lanes collected; nothing written"
    status=1; continue
  fi
  board="$out/$family.json"
  jq -n --arg family "$family" --arg at "$(date -u +%FT%TZ)" --arg summary "$summary" \
    --arg host "$(hostname -s)" --arg version "$version" --arg trigger "$trigger" \
    --argjson ms "$(( $(date +%s%3N) - started ))" \
    --slurpfile lanes "$lanes_file" --slurpfile problems "$problems_file" \
    --argjson work "$work_json" --argjson pins "$pins_json" --argjson notes "$notes_json" \
    --argjson columns "$columns_json" '
    {schema: "jeryu.release_board.v1", family: $family, observed_at: $at, summary: $summary,
     collector: {host: $host, version: $version, trigger: $trigger, duration_ms: $ms},
     lanes: $lanes, problems: ($problems | .[:64])}
    + (if $columns != null and $columns != [] then {columns: $columns} else {} end)
    + (if $work != null then {work: $work} else {} end)
    + (if $pins != null then {pins: $pins} else {} end)
    + (if $notes != null then {notes: $notes} else {} end)' >"$board.tmp" && mv "$board.tmp" "$board" \
    || { say "$family: could not assemble the board"; status=1; continue; }
  say "$family: $(jq -r '[(.lanes | length), ([.lanes[].stages[]] | length), (.problems | length)] | "\(.[0]) lanes, \(.[1]) stages, \(.[2]) problems"' "$board") → $board"
  if [ "$push" = 1 ]; then
    if [ -z "$forge_cfg" ]; then say "$family: not pushed (no token)"; status=1; continue; fi
    if answer="$(curl --silent --show-error --max-time 20 --config "$forge_cfg" -X PUT \
        -H 'Content-Type: application/json' -H 'Accept: application/json' \
        --data-binary @"$board" -w '\n%{http_code}' "$base/api/v1/release-board/$family")"; then
      code="${answer##*$'\n'}"
      if [ "$code" = 200 ]; then say "$family: pushed (${answer%$'\n'*})"
      else say "$family: forge answered $code: ${answer%$'\n'*}"; status=1; fi
    else
      say "$family: push failed"; status=1
    fi
  fi
done
exit "$status"
