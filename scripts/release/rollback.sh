#!/usr/bin/env bash
# rollback.sh — run ON the forge host (from ~/.jeryu/releases/<release>/) to undo
# switch.sh: stop, keep the post-switch databases aside, restore the pre-switch
# snapshot, repoint to PREV, start. Reads REL and PREV from RELEASE.env beside it,
# then PREV from ROLLBACK.env, where switch.sh records the release it actually
# replaced (which can be newer than the one live at staging).
#
# Overridable for tests: JERYU_HOME, JERYU_DATA, JERYU_SYSTEMCTL, JERYU_HEALTH_URL.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
source "$here/RELEASE.env"
# shellcheck source=/dev/null
[[ ! -e "$here/ROLLBACK.env" ]] || source "$here/ROLLBACK.env"
: "${REL:?RELEASE.env must set REL}" "${PREV:?RELEASE.env must set PREV}"

J="${JERYU_HOME:-$HOME/.jeryu}"
DATA="${JERYU_DATA:-$HOME/.local/share/jeryu}"
SYSTEMCTL="${JERYU_SYSTEMCTL:-systemctl}"
# No default: the address the forge answers its health endpoint on is site
# configuration, not source.
HEALTH="${JERYU_HEALTH_URL:-}"
[ -n "$HEALTH" ] || { echo "JERYU_HEALTH_URL is unset: set it to the forge's health endpoint, for example http://127.0.0.1:8787/health" >&2; exit 1; }
SNAP="$J/backups/pre-$REL"

[[ -s "$SNAP/forge.sqlite" ]] || { echo "no snapshot at $SNAP; refusing" >&2; exit 1; }
[[ -e "$J/bin/jeryu-$PREV" ]] || { echo "previous binary jeryu-$PREV is gone; refusing" >&2; exit 1; }

$SYSTEMCTL --user stop jeryu.service
KEEP="$J/backups/post-$REL-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$KEEP"; chmod 700 "$KEEP"
for n in forge.sqlite work.sqlite codegraph.sqlite; do
  cp -a "$DATA/$n" "$KEEP/"
  for s in -wal -shm; do [[ -e "$DATA/$n$s" ]] && mv "$DATA/$n$s" "$KEEP/"; done
  cp -a "$SNAP/$n" "$DATA/$n"
done
ln -sfn "jeryu-$PREV" "$J/bin/jeryu"
ln -sfn "web-dist-$PREV" "$J/share/web-dist"
$SYSTEMCTL --user start jeryu.service
for _ in $(seq 30); do curl -fsS -o /dev/null "$HEALTH" && break; sleep 1; done
pid="$($SYSTEMCTL --user show jeryu.service -p MainPID --value)"
echo "[rollback] live exe $(readlink "/proc/$pid/exe"); post-switch DBs kept in $KEEP; health: $(curl -fsS "$HEALTH")"
