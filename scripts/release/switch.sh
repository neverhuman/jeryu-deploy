#!/usr/bin/env bash
# switch.sh — run ON the forge host from a staged release directory
# (~/.jeryu/incoming/<release>/). Moves the live forge to REL: verify, stop,
# snapshot every database with SQLite's backup API, install, repoint the bin and
# web-dist symlinks, start, and prove the running binary is the staged one. REL
# comes from RELEASE.env beside this script, which stage-release.sh writes;
# nothing is edited per release.
#
# The release it replaces is whatever is live when it runs, not the PREV that
# RELEASE.env recorded at staging: a release deployed while this one was being
# built (or staged after it) changes what is live, and refusing then only meant
# re-staging the same commit. That live release is written to ROLLBACK.env in
# ~/.jeryu/releases/<REL>/, which rollback.sh restores. Refusing to put an older
# build over a newer one is deploy-release.sh's job, which knows the commits.
#
# Refuses unless the live symlink names an installed binary, the staged
# checksums hold, and no snapshot for REL exists yet. rollback.sh (staged beside
# it) undoes it.
#
# Running it again for the release that is already live is a no-op: it says
# "already live" and exits 0, without stopping the service or touching a thing.
#
# After starting REL it polls JERYU_HEALTH_URL (default
# http://172.19.0.1:8787/health, the forge's local health endpoint) once a second,
# JERYU_HEALTH_TRIES times (default 30). If it never answers, the switch fails
# (exit 1) with REL installed and live, so run rollback.sh.
#
# Before it stops anything it renders and installs the forge's systemd user unit
# from the staged jeryu.service.in with install-forge-unit.sh, so every release
# also refreshes the forge's memory, task and OOM limits. A site that has not set
# those numbers is refused there, with nothing stopped and nothing changed.
#
# Overridable for tests: JERYU_HOME, JERYU_DATA, JERYU_SYSTEMCTL, JERYU_HEALTH_URL,
# JERYU_HEALTH_TRIES, JERYU_FORGE_LIMITS_ENV, JERYU_SYSTEMD_USER_DIR.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
source "$here/RELEASE.env"
: "${REL:?RELEASE.env must set REL}"

J="${JERYU_HOME:-$HOME/.jeryu}"
DATA="${JERYU_DATA:-$HOME/.local/share/jeryu}"
SYSTEMCTL="${JERYU_SYSTEMCTL:-systemctl}"
HEALTH="${JERYU_HEALTH_URL:-http://172.19.0.1:8787/health}"
TRIES="${JERYU_HEALTH_TRIES:-30}"
[[ "$TRIES" =~ ^[1-9][0-9]*$ ]] || { echo "JERYU_HEALTH_TRIES must be a positive integer, got '$TRIES'" >&2; exit 1; }
IN="$J/incoming/$REL" OUT="$J/releases/$REL" SNAP="$J/backups/pre-$REL"

if [[ "$(readlink "$J/bin/jeryu")" == "jeryu-$REL" ]]; then
  echo "[switch] $REL is already live; nothing to do"
  exit 0
fi
live="$(readlink "$J/bin/jeryu" || true)"
[[ "$live" == jeryu-?* && "$live" != */* && -x "$J/bin/$live" ]] \
  || { echo "live binary '$live' is not an installed jeryu-<release>; refusing" >&2; exit 1; }
LIVE="${live#jeryu-}"
[[ "$LIVE" == "${PREV:-}" ]] || echo "[switch] replacing $LIVE (staged against ${PREV:-nothing}); rollback returns to $LIVE"
(cd "$IN" && sha256sum --quiet -c SHA256SUMS) || { echo "staged checksums fail; refusing" >&2; exit 1; }
[[ ! -e "$SNAP" ]] || { echo "snapshot $SNAP already exists; refusing" >&2; exit 1; }

# The unit, and with it the host's limits, before anything is stopped: an
# unconfigured site is refused while production is still up and untouched.
bash "$here/install-forge-unit.sh" || { echo "installing jeryu.service failed; refusing" >&2; exit 1; }

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
printf 'PREV=%s\n' "$LIVE" >"$OUT/ROLLBACK.env"
install -m 0755 "$OUT/bundle/jeryu" "$J/bin/jeryu-$REL"
cp -a "$OUT/web-dist" "$J/share/web-dist-$REL"
ln -sfn "jeryu-$REL" "$J/bin/jeryu"
ln -sfn "web-dist-$REL" "$J/share/web-dist"

echo "[switch] starting jeryu.service"
$SYSTEMCTL --user start jeryu.service
healthy=0
for ((i = 1; i <= TRIES; i++)); do
  curl -fsS -o /dev/null "$HEALTH" 2>/dev/null && { healthy=1; break; }
  ((i == TRIES)) || sleep 1
done
((healthy)) || { echo "health check timed out: $HEALTH never answered in $TRIES tries; REL is live, roll back with: bash $OUT/rollback.sh" >&2; exit 1; }
pid="$($SYSTEMCTL --user show jeryu.service -p MainPID --value)"
exe="$(readlink "/proc/$pid/exe")" want="$J/bin/jeryu-$REL"
[[ "$exe" == "$want" ]] || { echo "running exe $exe is not $want" >&2; exit 1; }
[[ "$(sha256sum "$exe" | cut -c1-64)" == "$(sha256sum "$OUT/bundle/jeryu" | cut -c1-64)" ]] \
  || { echo "running binary digest mismatch" >&2; exit 1; }
echo "[switch] live: pid $pid exe $exe; local health: $(curl -fsS "$HEALTH")"
echo "[switch] rollback: bash $OUT/rollback.sh"
