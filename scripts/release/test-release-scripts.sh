#!/usr/bin/env bash
# test-release-scripts.sh — exercise the real switch.sh and rollback.sh against a
# throwaway forge home: real SQLite databases, a "binary" that is a copy of sleep
# (so /proc/<pid>/exe resolves to the installed release), a stand-in systemctl
# that runs it, and a file:// health URL. No service, network or credential.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
T="$(mktemp -d)"
trap 'pkill -f "$T/home/.jeryu/bin/jeryu-" 2>/dev/null || true; rm -rf "$T"' EXIT
pass=0
ok() { pass=$((pass + 1)); echo "ok $pass - $1"; }
fail() { echo "not ok - $1" >&2; exit 1; }

export JERYU_HOME="$T/home/.jeryu" JERYU_DATA="$T/home/data" JERYU_HEALTH_URL="file://$T/health.json"
export JERYU_SYSTEMCTL="$T/systemctl"
PREV=prod-20260101T000000Z-aaaaaaa-unsigned REL=prod-20260102T000000Z-bbbbbbb-unsigned
mkdir -p "$JERYU_HOME"/{bin,share,incoming,releases,backups} "$JERYU_DATA"
printf '{"service":"jeryu-api","status":"ok"}\n' >"$T/health.json"
cat >"$T/systemctl" <<EOF
#!/usr/bin/env bash
# stand-in: --user {start|stop|show ... --value} jeryu.service
pidfile="$T/pid"
case "\$2" in
  start) "$JERYU_HOME/bin/jeryu" 300 & echo \$! >"\$pidfile" ;;
  stop) [[ -s "\$pidfile" ]] && kill "\$(cat "\$pidfile")" 2>/dev/null; rm -f "\$pidfile"; true ;;
  show) cat "\$pidfile" ;;
esac
EOF
chmod +x "$T/systemctl"

for n in forge work codegraph; do
  python3 -c "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute('create table t(v)'); c.execute(\"insert into t values('before')\"); c.commit()" "$JERYU_DATA/$n.sqlite"
done
db_value() { python3 -c "import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute('select v from t').fetchone()[0])" "$JERYU_DATA/forge.sqlite"; }

# The live release: a sleep copy under PREV's name.
cp "$(command -v sleep)" "$JERYU_HOME/bin/jeryu-$PREV"
mkdir -p "$JERYU_HOME/share/web-dist-$PREV"
ln -sfn "jeryu-$PREV" "$JERYU_HOME/bin/jeryu"
ln -sfn "web-dist-$PREV" "$JERYU_HOME/share/web-dist"

stage() { # stage REL into incoming exactly as stage-release.sh lays it out
  local d="$JERYU_HOME/incoming/$REL"
  rm -rf "$d"; mkdir -p "$d/bundle" "$d/web-dist"
  cp "$(command -v sleep)" "$d/bundle/jeryu"; echo "<html>$REL</html>" >"$d/web-dist/index.html"
  cp "$here/switch.sh" "$here/rollback.sh" "$d/"
  printf 'REL=%s\nPREV=%s\n' "$REL" "$PREV" >"$d/RELEASE.env"
  (cd "$d" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum >SHA256SUMS)
}

stage
echo tampered >>"$JERYU_HOME/incoming/$REL/web-dist/index.html"
if bash "$JERYU_HOME/incoming/$REL/switch.sh" >/dev/null 2>&1; then fail "switch accepted a tampered stage"; fi
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$PREV" ]] || fail "a refused switch moved the live symlink"
ok "switch refuses a stage whose checksums fail, and changes nothing"

stage
"$JERYU_SYSTEMCTL" --user start jeryu.service
bash "$JERYU_HOME/incoming/$REL/switch.sh" >"$T/switch.log" 2>&1 || { cat "$T/switch.log" >&2; fail "switch failed"; }
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$REL" ]] || fail "bin symlink not moved to REL"
[[ "$(readlink "$JERYU_HOME/share/web-dist")" == "web-dist-$REL" ]] || fail "web-dist symlink not moved to REL"
[[ -x "$JERYU_HOME/releases/$REL/rollback.sh" && -s "$JERYU_HOME/releases/$REL/RELEASE.env" ]] || fail "release dir lacks rollback.sh/RELEASE.env"
grep -q "integrity=ok" "$T/switch.log" || fail "no snapshot integrity line"
[[ "$(readlink "/proc/$(cat "$T/pid")/exe")" == "$JERYU_HOME/bin/jeryu-$REL" ]] || fail "running exe is not REL"
ok "switch installs REL, repoints both symlinks, snapshots every database and proves the running binary"

if bash "$JERYU_HOME/incoming/$REL/switch.sh" >/dev/null 2>&1; then fail "switch ran twice"; fi
ok "switch refuses when PREV is no longer live"

python3 -c "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute(\"update t set v='after'\"); c.commit()" "$JERYU_DATA/forge.sqlite"
bash "$JERYU_HOME/releases/$REL/rollback.sh" >"$T/rollback.log" 2>&1 || { cat "$T/rollback.log" >&2; fail "rollback failed"; }
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$PREV" ]] || fail "rollback did not repoint bin to PREV"
[[ "$(readlink "$JERYU_HOME/share/web-dist")" == "web-dist-$PREV" ]] || fail "rollback did not repoint web-dist to PREV"
[[ "$(db_value)" == before ]] || fail "rollback did not restore the pre-switch database"
kept="$(find "$JERYU_HOME/backups" -maxdepth 1 -name "post-$REL-*" | head -1)"
[[ "$(python3 -c "import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute('select v from t').fetchone()[0])" "$kept/forge.sqlite")" == after ]] \
  || fail "rollback did not keep the post-switch database aside"
ok "rollback restores PREV and the pre-switch database, keeping the post-switch one aside"

rm -rf "$JERYU_HOME/backups/pre-$REL"
if bash "$JERYU_HOME/releases/$REL/rollback.sh" >/dev/null 2>&1; then fail "rollback ran without a snapshot"; fi
ok "rollback refuses without a pre-switch snapshot"

echo "release scripts: $pass passed"
