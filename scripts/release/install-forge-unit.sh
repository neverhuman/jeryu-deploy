#!/usr/bin/env bash
# install-forge-unit.sh — render and install the forge's systemd user unit
# (jeryu.service) on the forge host from systemd/jeryu.service.in beside this
# script. switch.sh runs it before it stops anything, so every release installs
# or refreshes the unit; it can also be run on its own to apply a limits change.
#
# The unit's limits are the site's, never this repository's: they are read from
# the site limits file (JERYU_FORGE_LIMITS_ENV, default
# ~/.config/jeryu/forge-limits.env), which must set all of
#
#   JERYU_FORGE_MEMORY_HIGH    reclaim and throttle the forge from here (4G, 512M, bytes)
#   JERYU_FORGE_MEMORY_MAX     kill the forge past here; must exceed MemoryHigh
#   JERYU_FORGE_TASKS_MAX      processes and threads the forge may have at once
#   JERYU_FORGE_OOM_SCORE_ADJ  the forge's own oom_score_adj (-1000..1000)
#
# A site that has not decided these numbers is refused by name, and nothing is
# installed: an unbounded forge is what takes the host down, and that is never
# what a release should install. The limits file also reaches the forge itself
# through the unit's EnvironmentFile, so a setting the binary reads can live
# beside the ones systemd enforces.
#
# Budget the host for MemoryMax, not for what the forge uses when it is idle: a
# release, a clone and a migration all land inside the same cgroup.
#
# A site may also reshape the unit with its own drop-ins in jeryu.service.d/. A
# drop-in that rebuilds ExecStart out of ${VAR}s is checked against everything
# the unit and its drop-ins define through Environment= and EnvironmentFile=,
# and a variable no source defines is refused by name: a forge started with an
# empty --bind only crash-loops, and by then the previous release is gone.
#
# Overridable for tests: JERYU_FORGE_LIMITS_ENV, JERYU_SYSTEMD_USER_DIR, JERYU_SYSTEMCTL.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
template="$here/systemd/jeryu.service.in"
[[ -f "$template" ]] || template="$here/jeryu.service.in" # staged flat beside switch.sh
[[ -f "$template" ]] || { echo "no jeryu.service.in beside $0; refusing" >&2; exit 1; }
limits="${JERYU_FORGE_LIMITS_ENV:-$HOME/.config/jeryu/forge-limits.env}"
units="${JERYU_SYSTEMD_USER_DIR:-$HOME/.config/systemd/user}"
SYSTEMCTL="${JERYU_SYSTEMCTL:-systemctl}"

[[ -f "$limits" ]] || {
  echo "this site has no forge limits file at $limits; refusing to install an unbounded forge." >&2
  echo "Create it with JERYU_FORGE_MEMORY_HIGH, JERYU_FORGE_MEMORY_MAX, JERYU_FORGE_TASKS_MAX" >&2
  echo "and JERYU_FORGE_OOM_SCORE_ADJ (see scripts/release/README.md for what each one bounds)." >&2
  exit 1
}
# shellcheck source=/dev/null
set -a; source "$limits"; set +a

size_bytes() { # SIZE -> bytes, or nothing when it is not a size systemd takes
  local value="$1" unit="${1: -1}" digits="$1" scale=1
  case "$unit" in
    K) scale=1024 ;; M) scale=$((1024 ** 2)) ;; G) scale=$((1024 ** 3)) ;; T) scale=$((1024 ** 4)) ;;
    *) unit="" ;;
  esac
  [[ -z "$unit" ]] || digits="${value%?}"
  [[ "$digits" =~ ^[0-9]+$ ]] || return 0
  echo $((digits * scale))
}
need() { # NAME — the site must have set it
  [[ -n "${!1:-}" ]] || { echo "$limits does not set $1; refusing (see $0 --help)" >&2; exit 1; }
}
need_size() { # NAME — ... to a size systemd understands
  need "$1"
  [[ -n "$(size_bytes "${!1}")" ]] || { echo "$limits sets $1='${!1}', which is not a size (4G, 512M, bytes); refusing" >&2; exit 1; }
}

need_size JERYU_FORGE_MEMORY_HIGH
need_size JERYU_FORGE_MEMORY_MAX
need JERYU_FORGE_TASKS_MAX
[[ "$JERYU_FORGE_TASKS_MAX" =~ ^[1-9][0-9]*$ ]] \
  || { echo "$limits sets JERYU_FORGE_TASKS_MAX='$JERYU_FORGE_TASKS_MAX', which is not a positive count; refusing" >&2; exit 1; }
need JERYU_FORGE_OOM_SCORE_ADJ
[[ "$JERYU_FORGE_OOM_SCORE_ADJ" =~ ^-?[0-9]+$ ]] && ((JERYU_FORGE_OOM_SCORE_ADJ >= -1000 && JERYU_FORGE_OOM_SCORE_ADJ <= 1000)) \
  || { echo "$limits sets JERYU_FORGE_OOM_SCORE_ADJ='$JERYU_FORGE_OOM_SCORE_ADJ', which is not an oom_score_adj in -1000..1000; refusing" >&2; exit 1; }

(($(size_bytes "$JERYU_FORGE_MEMORY_HIGH") < $(size_bytes "$JERYU_FORGE_MEMORY_MAX"))) \
  || { echo "JERYU_FORGE_MEMORY_HIGH ($JERYU_FORGE_MEMORY_HIGH) must be below JERYU_FORGE_MEMORY_MAX ($JERYU_FORGE_MEMORY_MAX), or the forge is killed before it is ever throttled; refusing" >&2; exit 1; }

mkdir -p "$units"
rendered="$units/.jeryu.service.$$"
trap 'rm -f "$rendered"' EXIT
sed -e "s|@LIMITS_ENV@|$limits|g" \
    -e "s|@JERYU_FORGE_MEMORY_HIGH@|$JERYU_FORGE_MEMORY_HIGH|g" \
    -e "s|@JERYU_FORGE_MEMORY_MAX@|$JERYU_FORGE_MEMORY_MAX|g" \
    -e "s|@JERYU_FORGE_TASKS_MAX@|$JERYU_FORGE_TASKS_MAX|g" \
    -e "s|@JERYU_FORGE_OOM_SCORE_ADJ@|$JERYU_FORGE_OOM_SCORE_ADJ|g" \
    "$template" >"$rendered"
! grep -q '@[A-Z_]*@' "$rendered" || { echo "the rendered unit still has a placeholder; refusing" >&2; exit 1; }

# A site drop-in in jeryu.service.d/ may rebuild ExecStart out of ${VAR}s. If no
# environment source defines one of them, systemd hands the forge an empty
# argument and it crash-loops — which is only visible once the previous release
# is already gone. So read the unit and its drop-ins the way systemd would and
# refuse here, by name: this runs before a release stops anything.
dropins=()
if [[ -d "$units/jeryu.service.d" ]]; then
  while IFS= read -r conf; do dropins+=("$conf"); done \
    < <(find "$units/jeryu.service.d" -maxdepth 1 -type f -name '*.conf' | LC_ALL=C sort)
fi

specifiers() { # the specifiers a site may write in an EnvironmentFile= path
  sed -e "s|%h|$HOME|g" -e "s|%u|$(id -un)|g" -e 's|%%|%|g'
}
contains() { # NAME HAYSTACK...
  local want="$1" have; shift
  for have in "$@"; do [[ "$have" == "$want" ]] && return 0; done
  return 1
}

used=()      # names ExecStart* lines reference, first use first
env_files=() # EnvironmentFile= paths, keeping any '-' that marks one as optional
env_names=() # names Environment= lines assign
for conf in "$rendered" ${dropins[@]+"${dropins[@]}"}; do
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%$'\r'}"
    line="${line//\$\$/}" # systemd's escape for a literal dollar
    case "$line" in
      ExecStart*=*)
        for ref in $(grep -oE '\$\{[A-Za-z_][A-Za-z0-9_]*\}|\$[A-Za-z_][A-Za-z0-9_]*' \
          <<<"${line#*=}"); do
          name="${ref#\$}"; name="${name#\{}"; name="${name%\}}"
          contains "$name" ${used[@]+"${used[@]}"} || used+=("$name")
        done ;;
      EnvironmentFile=)  env_files=() ;; # an empty value resets the list
      EnvironmentFile=*) env_files+=("$(specifiers <<<"${line#*=}")") ;;
      Environment=)      env_names=() ;;
      Environment=*)
        for assignment in ${line#*=}; do
          [[ "$assignment" == *=* ]] && env_names+=("${assignment%%=*}")
        done ;;
    esac
  done <"$conf"
done

defined=(${env_names[@]+"${env_names[@]}"})
for file in ${env_files[@]+"${env_files[@]}"}; do
  optional=""
  [[ "$file" == -* ]] && { optional=yes; file="${file#-}"; }
  [[ -f "$file" ]] || {
    [[ -n "$optional" ]] && continue
    echo "jeryu.service reads EnvironmentFile=$file, which does not exist; refusing" >&2
    exit 1
  }
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ "$line" =~ ^[A-Za-z_][A-Za-z0-9_]*= ]] && defined+=("${line%%=*}")
  done <"$file"
done
# What systemd itself puts in the forge's environment, so a drop-in may use it.
defined+=(HOME USER LOGNAME SHELL PATH LANG TERM INVOCATION_ID JOURNAL_STREAM XDG_RUNTIME_DIR)

undefined=()
for name in ${used[@]+"${used[@]}"}; do
  contains "$name" "${defined[@]}" || undefined+=("$name")
done
((${#undefined[@]} == 0)) || {
  echo "the forge unit's ExecStart uses ${undefined[*]}, which no Environment= or" >&2
  echo "EnvironmentFile= of jeryu.service or its drop-ins in $units/jeryu.service.d/ defines;" >&2
  echo "refusing: the forge would start with the argument empty and crash-loop." >&2
  echo "Define ${undefined[*]} in $limits, or in an environment file a drop-in of your own" >&2
  echo "names with EnvironmentFile= (see scripts/release/README.md)." >&2
  exit 1
}

if cmp -s "$rendered" "$units/jeryu.service"; then
  echo "[unit] jeryu.service is already the current one (MemoryMax=$JERYU_FORGE_MEMORY_MAX, TasksMax=$JERYU_FORGE_TASKS_MAX)"
else
  install -m 644 "$rendered" "$units/jeryu.service"
  echo "[unit] installed jeryu.service (MemoryHigh=$JERYU_FORGE_MEMORY_HIGH MemoryMax=$JERYU_FORGE_MEMORY_MAX TasksMax=$JERYU_FORGE_TASKS_MAX OOMScoreAdjust=$JERYU_FORGE_OOM_SCORE_ADJ)"
fi
$SYSTEMCTL --user daemon-reload
