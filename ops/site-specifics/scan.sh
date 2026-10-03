#!/usr/bin/env bash
# Scan a jeryu checkout for site-specific terms. The jeryu repositories are
# public and must not name one installation: hosts, user names, absolute home
# paths, private address ranges and credential file names belong in
# configuration outside the repository.
#
# The terms themselves are site data, so they are not in this repository
# either: JERYU_SITE_TERMS_FILE points at a file the site keeps outside it
# (for example ~/.config/jeryu/site-terms). Without that file the scanner
# refuses to run rather than scanning for nothing.
#
# Usage:
#   ops/site-specifics/scan.sh [--ci] [--quiet] [path ...]
#
#   --ci      exit non-zero when a match is not covered by the allowlist
#   --quiet   print only the per-path counts, not the matches
#   path ...  checkouts to scan (default: this repository)
#
# Environment:
#   JERYU_SITE_TERMS_FILE  required; one term per line, POSIX extended regex,
#                          matched case-insensitively. Blank lines and lines
#                          starting with # are ignored.
#   JERYU_SITE_ALLOW_FILE  optional; matches this file covers do not fail --ci.
#                          One entry per line, either `<path-glob>` or
#                          `<path-glob>:<term>`, where the term is the term as
#                          written in the terms file.
#
# Output is one line per match, `file:line:term`, followed by a count per path.
set -euo pipefail

self_name='site-specifics'

log() { printf '[%s] %s\n' "$self_name" "$*"; }
die() { printf '[%s] FATAL: %s\n' "$self_name" "$*" >&2; exit 2; }

ci_mode=0
quiet=0
roots=()

while [ $# -gt 0 ]; do
  case "$1" in
    --ci) ci_mode=1 ;;
    --quiet) quiet=1 ;;
    -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --) shift; break ;;
    -*) die "unknown option: $1" ;;
    *) roots+=("$1") ;;
  esac
  shift
done
while [ $# -gt 0 ]; do roots+=("$1"); shift; done

if [ "${#roots[@]}" -eq 0 ]; then
  roots=("$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)")
fi

terms_file="${JERYU_SITE_TERMS_FILE:-}"
if [ -z "$terms_file" ]; then
  die "JERYU_SITE_TERMS_FILE is unset. The site-specific terms are site data and
       are not kept in this public repository. Point JERYU_SITE_TERMS_FILE at a
       file outside it (for example ~/.config/jeryu/site-terms) holding one term
       per line, each a POSIX extended regex."
fi
[ -f "$terms_file" ] || die "JERYU_SITE_TERMS_FILE does not name a file: ${terms_file}"

allow_file="${JERYU_SITE_ALLOW_FILE:-}"
if [ -n "$allow_file" ] && [ ! -f "$allow_file" ]; then
  die "JERYU_SITE_ALLOW_FILE does not name a file: ${allow_file}"
fi

terms=()
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in ''|'#'*) continue ;; esac
  terms+=("$line")
done < "$terms_file"
[ "${#terms[@]}" -gt 0 ] || die "no terms in ${terms_file}"

allow_entries=()
if [ -n "$allow_file" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in ''|'#'*) continue ;; esac
    allow_entries+=("$line")
  done < "$allow_file"
fi

# A match is covered when an allowlist entry names its path, or names its path
# and the term that matched.
allowed() {
  local path="$1" term="$2" entry
  for entry in ${allow_entries+"${allow_entries[@]}"}; do
    case "$entry" in
      *:*)
        [ "${entry##*:}" = "$term" ] || continue
        # shellcheck disable=SC2254 # the entry is a glob on purpose
        case "$path" in ${entry%:*}) return 0 ;; esac
        ;;
      *)
        # shellcheck disable=SC2254
        case "$path" in $entry) return 0 ;; esac
        ;;
    esac
  done
  return 1
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

total_flagged=0
total_matches=0

for root in "${roots[@]}"; do
  [ -d "$root" ] || die "not a directory: ${root}"
  root_abs="$(cd "$root" && pwd)"
  label="$(basename "$root_abs")"

  # Scan what the repository publishes: its tracked files. A checkout that is
  # not a git repository falls back to every regular file under it.
  list="${work}/list"
  if git -C "$root_abs" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    git -C "$root_abs" ls-files -z > "$list"
  else
    (cd "$root_abs" && find . -type f -not -path './.git/*' -printf '%P\0') > "$list"
  fi
  scanned="$(tr -cd '\0' < "$list" | wc -c | tr -d ' ')"

  hits="${work}/hits"
  : > "$hits"
  for term in "${terms[@]}"; do
    # The term goes through the environment, not awk -v: awk would read a
    # regex escape such as \. as its own escape sequence and warn.
    # grep -I skips binary files; -n gives the line number. Only the path and
    # the line number are kept: the matched line itself can be site data.
    (cd "$root_abs" && xargs -0 -r grep -nHiIE -e "$term" -- < "$list" 2>/dev/null || true) \
      | term="$term" awk -F: 'NF >= 3 { print $1 ":" $2 ":" ENVIRON["term"] }' >> "$hits"
  done

  matches=0
  flagged=0
  while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    matches=$((matches + 1))
    path="${hit%%:*}"
    term="${hit#*:}"
    term="${term#*:}"
    if allowed "$path" "$term"; then
      continue
    fi
    flagged=$((flagged + 1))
    [ "$quiet" -eq 1 ] || printf '%s\n' "$hit"
  done < <(sort -t: -k1,1 -k2,2n "$hits")

  files="$(cut -d: -f1 "$hits" | sort -u | grep -c . || true)"
  log "${label}: ${matches} matches in ${files} files (${scanned} scanned), ${flagged} not allowlisted"
  total_matches=$((total_matches + matches))
  total_flagged=$((total_flagged + flagged))
done

if [ "${#roots[@]}" -gt 1 ]; then
  log "total: ${total_matches} matches, ${total_flagged} not allowlisted"
fi

if [ "$ci_mode" -eq 1 ] && [ "$total_flagged" -gt 0 ]; then
  printf '[%s] FAILED: %s site-specific match(es) are not allowlisted. Move the value into configuration outside the repository, use invented data in tests and docs, or add it to %s.\n' \
    "$self_name" "$total_flagged" "${allow_file:-the file named by JERYU_SITE_ALLOW_FILE}" >&2
  exit 1
fi
