#!/usr/bin/env bash
# test-release-scripts.sh — exercise the real switch.sh and rollback.sh against a
# throwaway forge home: real SQLite databases, a "binary" that is a copy of sleep
# (so /proc/<pid>/exe resolves to the installed release), a stand-in systemctl
# that runs it, and a file:// health URL. Then auto-stage.sh against a local bare
# repo with stand-in ssh and curl. No service, network or credential.
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

# --- auto-stage.sh: stage once, tell the forge, never let a failed event POST fail staging ---
# Stand-ins: a local bare repo as the remote (its stage-release.sh is a stub that stages or fails
# on demand), ssh that names the live release, curl that answers the status API and records every
# event body. No network, no token beyond a throwaway file.
A="$T/auto"; mkdir -p "$A/bin" "$A/src/scripts/release"
cat >"$A/src/scripts/release/stage-release.sh" <<EOF
#!/usr/bin/env bash
echo "building \$1" >&2
[ -e "$A/fail" ] && { echo "error: linker exploded" >&2; exit 3; }
echo "prod-20260103T000000Z-\${1:0:7}-unsigned"
EOF
chmod +x "$A/src/scripts/release/stage-release.sh"
git -C "$A/src" init -q -b main
git -C "$A/src" -c user.name=t -c user.email=t@t add .
git -C "$A/src" -c user.name=t -c user.email=t@t commit -q -m stage
git clone -q --bare "$A/src" "$A/remote.git"
sha="$(git -C "$A/remote.git" rev-parse refs/heads/main)"
cat >"$A/bin/ssh" <<EOF
#!/usr/bin/env bash
echo "jeryu-$PREV"
EOF
cat >"$A/bin/curl" <<EOF
#!/usr/bin/env bash
# The status API answers success; an event POST is recorded, or refused when told to.
case "\$*" in
  *"/api/v1/events"*) [ -e "$A/refuse-events" ] && exit 22; cat >>"$A/events.jsonl"; echo >>"$A/events.jsonl" ;;
  *) echo '{"state":"success"}' ;;
esac
EOF
chmod +x "$A/bin/ssh" "$A/bin/curl"
echo "not-a-real-token" >"$A/token"
auto_stage() {
  PATH="$A/bin:$PATH" JERYU_DEPLOY_REMOTE="$A/remote.git" JERYU_AUTO_STAGE_STATE="$A/state" \
    JERYU_STATUS_TOKEN_FILE="$A/token" JERYU_BASE="https://forge.invalid" bash "$here/auto-stage.sh"
}

touch "$A/fail"
if auto_stage >"$A/run1.log" 2>&1; then fail "auto-stage reported success for a failed staging"; fi
[[ "$(jq -r '.kind' "$A/events.jsonl")" == release.stage_failed ]] || fail "no release.stage_failed event"
[[ "$(jq -r '.needs_human' "$A/events.jsonl")" == false ]] || fail "the first failure is retried, not a human's turn"
jq -e '.log_tail | contains("linker exploded")' "$A/events.jsonl" >/dev/null || fail "the failure event lacks the log tail"
grep -q "linker exploded" "$A/run1.log" || fail "staging output no longer reaches the journal"
if grep -rq "not-a-real-token" "$A/run1.log" "$A/events.jsonl"; then fail "the token leaked"; fi
: >"$A/events.jsonl"
if auto_stage >/dev/null 2>&1; then fail "second failed staging reported success"; fi
[[ "$(jq -r '.needs_human' "$A/events.jsonl")" == true ]] || fail "the final failed attempt must ask for a human"
[[ "$(jq -r '.event_id' "$A/events.jsonl")" == "auto-stage:failed:${sha:0:12}:2" ]] || fail "failure event_id is not stable"
ok "auto-stage reports a failed staging with its log tail, and asks for a human on the last attempt"

rm -f "$A/fail" "$A/state/failures/$sha"; : >"$A/events.jsonl"
touch "$A/refuse-events"
auto_stage >"$A/run3.log" 2>&1 || { cat "$A/run3.log" >&2; fail "a refused event POST failed the staging"; }
want="prod-20260103T000000Z-${sha:0:7}-unsigned"
[[ "$(cat "$A/state/latest")" == "$want" ]] || fail "latest is not the staged release"
grep -q "posting release.staged failed (ignored)" "$A/run3.log" || fail "a refused event POST was not logged"
ok "auto-stage stages even when the forge refuses the event"

rm -f "$A/refuse-events"; rm -rf "$A/state"
auto_stage >/dev/null 2>&1 || fail "auto-stage failed"
[[ "$(jq -r '.kind' "$A/events.jsonl")" == release.staged ]] || fail "no release.staged event"
[[ "$(jq -r '.detail.deploy_command' "$A/events.jsonl")" == "scripts/release/deploy-release.sh $want" ]] || fail "staged event lacks the deploy command"
[[ "$(jq -r '.detail.previous_release' "$A/events.jsonl")" == "$PREV" ]] || fail "staged event lacks the live release"
[[ "$(jq -r '.sha' "$A/events.jsonl")" == "$sha" ]] || fail "staged event names the wrong commit"
: >"$A/events.jsonl"
auto_stage >/dev/null 2>&1 || fail "an idle auto-stage tick failed"
[[ ! -s "$A/events.jsonl" ]] || fail "an already-staged commit emitted again"
ok "auto-stage reports a staged release once, with the deploy command and what is live"

echo "release scripts: $pass passed"
