#!/usr/bin/env bash
# switch.sh — run ON the forge host from a staged release directory
# (~/.jeryu/incoming/<release>/). Moves the live forge from PREV to REL:
# verify, stop, snapshot every database with SQLite's backup API, install,
# repoint the bin and web-dist symlinks, start, and prove the running binary is
# the staged one. REL and PREV come from RELEASE.env beside this script, which
# stage-release.sh writes; nothing is edited per release.
#
# Refuses unless PREV is what is live, the staged checksums hold, and no
# snapshot for REL exists yet. rollback.sh (staged beside it) undoes it.
#
# Overridable for tests: JERYU_HOME, JERYU_DATA, JERYU_SYSTEMCTL, JERYU_HEALTH_URL.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
source "$here/RELEASE.env"
: "${REL:?RELEASE.env must set REL}" "${PREV:?RELEASE.env must set PREV}"

J="${JERYU_HOME:-$HOME/.jeryu}"
DATA="${JERYU_DATA:-$HOME/.local/share/jeryu}"
SYSTEMCTL="${JERYU_SYSTEMCTL:-systemctl}"
HEALTH="${JERYU_HEALTH_URL:-http://172.19.0.1:8787/health}"
IN="$J/incoming/$REL" OUT="$J/releases/$REL" SNAP="$J/backups/pre-$REL"

[[ "$(readlink "$J/bin/jeryu")" == "jeryu-$PREV" ]] || { echo "live binary is not $PREV; refusing" >&2; exit 1; }
(cd "$IN" && sha256sum --quiet -c SHA256SUMS) || { echo "staged checksums fail; refusing" >&2; exit 1; }
[[ ! -e "$SNAP" ]] || { echo "snapshot $SNAP already exists; refusing" >&2; exit 1; }

echo "[switch] stopping jeryu.service"
$SYSTEMCTL --user stop jeryu.service
mkdir -p "$SNAP"; chmod 700 "$SNAP"
python3 - "$DATA" "$SNAP" <<'P'
import hashlib, sqlite3, sys
src, dst = sys.argv[1], sys.argv[2]
for name in ("forge.sqlite", "work.sqlite", "codegraph.sqlite"):
    s = sqlite3.connect(f"file:{src}/{name}?mode=ro", uri=True)
    d = sqlite3.connect(f"{dst}/{name}")
    s.backup(d); d.close(); s.close()
    ok = sqlite3.connect(f"{dst}/{name}").execute("pragma integrity_check").fetchone()[0]
    digest = hashlib.sha256(open(f"{dst}/{name}", "rb").read()).hexdigest()[:16]
    print(f"[switch] snapshot {name}: integrity={ok} sha256={digest}")
    assert ok == "ok"
P

cp -a "$IN" "$OUT"
install -m 0755 "$OUT/bundle/jeryu" "$J/bin/jeryu-$REL"
cp -a "$OUT/web-dist" "$J/share/web-dist-$REL"
ln -sfn "jeryu-$REL" "$J/bin/jeryu"
ln -sfn "web-dist-$REL" "$J/share/web-dist"

echo "[switch] starting jeryu.service"
$SYSTEMCTL --user start jeryu.service
for _ in $(seq 30); do curl -fsS -o /dev/null "$HEALTH" && break; sleep 1; done
pid="$($SYSTEMCTL --user show jeryu.service -p MainPID --value)"
exe="$(readlink "/proc/$pid/exe")" want="$J/bin/jeryu-$REL"
[[ "$exe" == "$want" ]] || { echo "running exe $exe is not $want" >&2; exit 1; }
[[ "$(sha256sum "$exe" | cut -c1-64)" == "$(sha256sum "$OUT/bundle/jeryu" | cut -c1-64)" ]] \
  || { echo "running binary digest mismatch" >&2; exit 1; }
echo "[switch] live: pid $pid exe $exe; local health: $(curl -fsS "$HEALTH")"
echo "[switch] rollback: bash $OUT/rollback.sh"
