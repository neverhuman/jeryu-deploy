#!/usr/bin/env bash
# test-release-scripts.sh — exercise the real switch.sh and rollback.sh against a
# throwaway forge home: real SQLite databases, a "binary" that is a copy of sleep
# (so /proc/<pid>/exe resolves to the installed release), a stand-in systemctl
# that runs it, and a file:// health URL. Then auto-stage.sh against a local bare
# repo with stand-in ssh and curl, and auto-pin.sh against two. No service, network or credential.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
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

# A live symlink that names no installed binary leaves nothing to roll back to, so refuse.
LIVE_REL="$REL" REL=prod-20260103T000000Z-ccccccc-unsigned
stage
ln -sfn jeryu-prod-20260101T000000Z-0000000-unsigned "$JERYU_HOME/bin/jeryu"
if bash "$JERYU_HOME/incoming/$REL/switch.sh" >/dev/null 2>&1; then fail "switch accepted a live symlink to no binary"; fi
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == jeryu-prod-20260101T000000Z-0000000-unsigned ]] || fail "a refused switch moved the live symlink"
ln -sfn "jeryu-$LIVE_REL" "$JERYU_HOME/bin/jeryu"
REL="$LIVE_REL"
ok "switch refuses when the live symlink names no installed binary"

# The same release again: nothing to do, and nothing done.
stamp="$(stat -c %Y "$JERYU_HOME/bin/jeryu-$REL")"
bash "$JERYU_HOME/incoming/$REL/switch.sh" >"$T/switch-again.log" 2>&1 \
  || { cat "$T/switch-again.log" >&2; fail "a redeploy of the live release did not exit 0"; }
grep -q "already live" "$T/switch-again.log" || fail "a redeploy of the live release did not say it is already live"
if grep -q "stopping jeryu.service" "$T/switch-again.log"; then fail "a redeploy of the live release stopped the service"; fi
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$REL" ]] || fail "a redeploy of the live release moved the live symlink"
[[ "$(stat -c %Y "$JERYU_HOME/bin/jeryu-$REL")" == "$stamp" ]] || fail "a redeploy of the live release reinstalled the binary"
ok "switch is a no-op for the release that is already live"

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

# A REL that never answers its health check must fail the switch, not report success.
REL_OK="$REL"; REL=prod-20260103T000000Z-ccccccc-unsigned
stage
rc=0; JERYU_HEALTH_URL="file://$T/no-such-health.json" JERYU_HEALTH_TRIES=2 \
  bash "$JERYU_HOME/incoming/$REL/switch.sh" >"$T/switch-unhealthy.log" 2>&1 || rc=$?
[[ $rc != 0 ]] || fail "switch reported success though its health check never passed"
grep -q "health check timed out" "$T/switch-unhealthy.log" || fail "an unhealthy switch did not say its health check timed out"
bash "$JERYU_HOME/releases/$REL/rollback.sh" >/dev/null 2>&1 || fail "rollback after an unhealthy switch failed"
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$PREV" ]] || fail "rollback after an unhealthy switch did not restore PREV"
REL="$REL_OK"
ok "switch fails when its health check never passes, and rollback recovers"

# A release staged while PREV was live, deployed after a newer one went live: it replaces what is
# live, not what it was staged against, and its rollback returns to what it replaced.
REL_OK="$REL"; REL=prod-20260104T000000Z-ddddddd-unsigned
stage
bash "$JERYU_HOME/incoming/$REL/switch.sh" >"$T/switch-newer.log" 2>&1 || { cat "$T/switch-newer.log" >&2; fail "switch to the newer release failed"; }
NEWER="$REL"; REL=prod-20260105T000000Z-eeeeeee-unsigned
stage
grep -qx "PREV=$PREV" "$JERYU_HOME/incoming/$REL/RELEASE.env" || fail "the late release is not staged against PREV"
bash "$JERYU_HOME/incoming/$REL/switch.sh" >"$T/switch-late.log" 2>&1 \
  || { cat "$T/switch-late.log" >&2; fail "switch refused a release staged before the live one went live"; }
grep -q "replacing $NEWER (staged against $PREV)" "$T/switch-late.log" || fail "switch did not say it replaces a release other than the staged PREV"
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$REL" ]] || fail "the late release is not live"
grep -qx "PREV=$NEWER" "$JERYU_HOME/releases/$REL/ROLLBACK.env" || fail "ROLLBACK.env does not name the release that was replaced"
bash "$JERYU_HOME/releases/$REL/rollback.sh" >/dev/null 2>&1 || fail "rollback of the late release failed"
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$NEWER" ]] || fail "rollback did not return to the release that was replaced"
[[ "$(readlink "$JERYU_HOME/share/web-dist")" == "web-dist-$NEWER" ]] || fail "rollback did not return web-dist to the release that was replaced"
REL="$REL_OK"
ok "switch replaces whatever is live, not the PREV it was staged against, and rollback returns to it"

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
# The status API answers success; an event POST or a heartbeat is recorded, or refused when told to.
printf '%s\n' "\$*" >>"$A/curl-args"
case "\$*" in
  *"/api/v1/runners/heartbeat"*) [ -e "$A/refuse-beats" ] && exit 22; cat >>"$A/beats.jsonl"; echo >>"$A/beats.jsonl" ;;
  *"/api/v1/events"*) [ -e "$A/refuse-events" ] && exit 22; cat >>"$A/events.jsonl"; echo >>"$A/events.jsonl" ;;
  *) echo '{"state":"success"}' ;;
esac
EOF
chmod +x "$A/bin/ssh" "$A/bin/curl"
echo "not-a-real-token" >"$A/token"
auto_stage() {
  PATH="$A/bin:$PATH" JERYU_DEPLOY_REMOTE="$A/remote.git" JERYU_DEPLOY_REPO=jeryu/jeryu-deploy JERYU_AUTO_STAGE_STATE="$A/state" \
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

# Every tick, idle or not, posts one heartbeat so /runners shows the timer alive and what it did.
newest() { jq -sc '.[-1]' "$1"; }
me="$(hostname -s)"
: >"$A/beats.jsonl"
auto_stage >"$A/run5.log" 2>&1 || fail "an idle auto-stage tick failed"
[[ "$(jq -s length "$A/beats.jsonl")" == 1 && ! -s "$A/run5.log" ]] || fail "an idle auto-stage tick must post exactly one heartbeat, silently"
newest "$A/beats.jsonl" | jq -e --arg id "$me/auto-stage" --arg host "$me" --arg sha "$sha" '
  .runnerId == $id and .host == $host and .slot == 0 and .labels == ["automation"] and .intervalSeconds == 300
  and (has("current") | not) and .last.repo == "jeryu/jeryu-deploy" and .last.sha == $sha and (.last | has("pr") | not)
  and .last.recipe == "auto-stage" and .last.conclusion == "staged" and .last.seconds == 0' >/dev/null || fail "the auto-stage heartbeat is not the contract's"
[[ "$(newest "$A/beats.jsonl" | jq -r .last.finishedAt)" == "$(tail -n 1 "$A/state/staged.tsv" | cut -f3)" ]] || fail "the heartbeat does not date the staging from staged.tsv"
rm -rf "$A/state"; touch "$A/fail" "$A/refuse-beats"; : >"$A/beats.jsonl"
rc=0; auto_stage >"$A/run6.log" 2>&1 || rc=$?
[[ $rc == 1 && "$(grep -c heartbeat "$A/run6.log")" == 1 ]] || fail "a refused heartbeat must cost one log line and leave the tick's result alone"
rm -f "$A/refuse-beats"
if auto_stage >/dev/null 2>&1; then fail "the second failed staging reported success"; fi
newest "$A/beats.jsonl" | jq -e --arg sha "$sha" '.last.conclusion == "failed" and .last.sha == $sha' >/dev/null || fail "a staging that gave up does not beat as failed"
rm -f "$A/fail"; rm -rf "$A/state"; : >"$A/beats.jsonl"
JERYU_AUTO_STAGE_BEAT=0 auto_stage >/dev/null 2>&1 || fail "auto-stage failed with the heartbeat off"
[[ ! -s "$A/beats.jsonl" ]] || fail "JERYU_AUTO_STAGE_BEAT=0 still posted a heartbeat"
if grep -rq "not-a-real-token" "$A/curl-args" "$A/beats.jsonl" "$A"/run*.log; then fail "the token leaked into argv or output"; fi
grep "runners/heartbeat" "$A/curl-args" | grep -qv -- "--config" && fail "a heartbeat did not get the token through a config file"
ok "auto-stage beats once per tick with what it last did; a refused beat never fails the tick"

# --- auto-pin.sh: propose the jeryu-web pin bump, exactly two lock lines, never merge ---
# Stand-ins: local bare repos as the jeryu-deploy and jeryu-web remotes (jeryu-deploy main carries
# a stub build-web-dist.sh that prints a hash or fails on demand) and a curl that answers the
# pull request list, the status API, the pull request POST and the events route, recording every
# argv. No network, no docker, no real token.
P="$T/pin"; mkdir -p "$P/bin" "$P/web" "$P/deploy/scripts/release"
g() { git -C "$1" -c user.name=t -c user.email=t@t "${@:2}"; }
g "$P/web" init -q -b main
for n in 1 2 3; do echo "$n" >"$P/web/index.html"; g "$P/web" add .; g "$P/web" commit -q -m "web change $n"; done
git clone -q --bare "$P/web" "$P/web.git"
w1="$(git -C "$P/web" rev-parse HEAD~2)"; h1="$(git -C "$P/web" rev-parse HEAD)"
dist_a="$(printf a%.0s {1..64})"; dist_b="$(printf b%.0s {1..64})"
cat >"$P/deploy/jeryu-split.lock.toml" <<EOF
web_artifact = "pinned"

[[repo]]
name = "jeryu-core"
commit = "$(printf c%.0s {1..40})"

[[repo]]
name = "jeryu-web"
commit = "$w1"
web_dist_sha256 = "$dist_a"

[[repo]]
name = "jeryu-deploy"
commit = "PENDING_SELF"
EOF
cat >"$P/deploy/scripts/release/build-web-dist.sh" <<EOF
#!/usr/bin/env bash
echo "[web] building \$2" >&2; echo build >>"$P/builds"
[ -e "$P/fail" ] && { echo "error: vite exploded" >&2; exit 4; }
echo "\$2 $dist_b"
EOF
chmod +x "$P/deploy/scripts/release/build-web-dist.sh"
g "$P/deploy" init -q -b main; g "$P/deploy" add .; g "$P/deploy" commit -q -m deploy
git clone -q --bare "$P/deploy" "$P/deploy.git"
cat >"$P/bin/curl" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$P/curl-args"
url="\${!#}"; method=GET; prev=""
for arg in "\$@"; do [ "\$prev" = -X ] && method="\$arg"; prev="\$arg"; done
case "\$method \$url" in
  "POST https://git.neverhuman.org/api/v1/events") cat >>"$P/events.jsonl"; echo >>"$P/events.jsonl"; echo '{"ok":true}' ;;
  "POST https://git.neverhuman.org/api/v1/runners/heartbeat")
    [ -e "$P/refuse-beats" ] && exit 22; cat >>"$P/beats.jsonl"; echo >>"$P/beats.jsonl"; echo '{"accepted":true}' ;;
  "POST https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-deploy/pulls")
    [ -e "$P/refuse-pr" ] && exit 22; cat >>"$P/prs.jsonl"; echo >>"$P/prs.jsonl"; echo '{"number":7}' ;;
  "GET https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-deploy/pulls?"*)
    page="\${url##*page=}"; page="\${page%%&*}"
    if [ -e "$P/pulls.json.\$page" ]; then cat "$P/pulls.json.\$page"; elif [ "\$page" = 1 ]; then cat "$P/pulls.json"; else echo '[]'; fi ;;
  "GET https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-web/commits/"*"/status") echo "{\"state\":\"\$(cat "$P/gate")\"}" ;;
  *) echo "unexpected curl: \$method \$url" >&2; exit 22 ;;
esac
EOF
chmod +x "$P/bin/curl"
echo "not-a-real-pin-token" >"$P/token"; chmod 600 "$P/token"
echo '[]' >"$P/pulls.json"; echo pending >"$P/gate"; : >"$P/curl-args"; : >"$P/events.jsonl"; : >"$P/prs.jsonl"; : >"$P/builds"; : >"$P/beats.jsonl"
auto_pin() {
  PATH="$P/bin:$PATH" JERYU_DEPLOY_REMOTE="$P/deploy.git" JERYU_WEB_REMOTE="$P/web.git" \
    JERYU_DEPLOY_REPO=jeryu/jeryu-deploy JERYU_WEB_REPO=jeryu/jeryu-web JERYU_AUTO_PIN_STATE="${PIN_STATE:-$P/state}" \
    JERYU_PIN_TOKEN_FILE="$P/token" JERYU_BASE="${PIN_BASE:-https://git.neverhuman.org}" bash "$here/auto-pin.sh"
}
branches() { git -C "$P/deploy.git" for-each-ref --format='%(refname:short)' 'refs/heads/auto/*'; }
lines() { wc -l <"$1" | tr -d ' '; }
beat_is() { newest "$P/beats.jsonl" | jq -e "${@:2}" "$1" >/dev/null; } # FILTER [jq args]: the newest heartbeat
posted() { jq -s length "$1"; }

rc=0; PIN_BASE=https://git.neverhuman.org.evil.invalid auto_pin >"$P/run.log" 2>&1 || rc=$?
[[ $rc == 2 && ! -s "$P/curl-args" ]] || fail "auto-pin sent a request to a noncanonical origin"
chmod 644 "$P/token"; rc=0; auto_pin >"$P/run.log" 2>&1 || rc=$?; chmod 600 "$P/token"
[[ $rc == 2 && ! -s "$P/curl-args" ]] || fail "auto-pin accepted a group-readable token file"
ok "auto-pin refuses a foreign origin and a loose token file before any request"

auto_pin >"$P/run.log" 2>&1 || fail "a not-green tick failed"
grep -q "gate is pending; waiting" "$P/run.log" || fail "auto-pin did not wait for the web gate"
[[ -z "$(branches)" && ! -s "$P/prs.jsonl" && ! -s "$P/builds" && ! -s "$P/events.jsonl" ]] || fail "auto-pin acted on a web head that is not green"
[[ "$(posted "$P/beats.jsonl")" == 1 && "$(grep -c runners/heartbeat "$P/curl-args")" == 1 ]] || fail "a tick must post exactly one heartbeat"
beat_is '.runnerId == $id and .host == $host and .slot == 0 and .labels == ["automation"] and .intervalSeconds == 300
  and (has("current") | not) and .last.conclusion == "waiting" and .last.sha == $sha and (.last | has("pr") | not)' \
  --arg id "$me/auto-pin" --arg host "$me" --arg sha "$h1" || fail "the auto-pin heartbeat is not the contract's"
ok "auto-pin waits while jeryu-web main is not green, and beats once to say so"

echo success >"$P/gate"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "auto-pin failed on the happy path"; }
b1="auto/pin-web-${h1:0:12}"
[[ "$(branches)" == "$b1" ]] || fail "auto-pin did not push $b1"
[[ "$(git -C "$P/deploy.git" diff --numstat main "$b1" | tr '\t' ' ')" == "2 2 jeryu-split.lock.toml" ]] || fail "the bump is not exactly two lock lines"
lock_now="$(git -C "$P/deploy.git" show "$b1:jeryu-split.lock.toml")"
grep -qx "commit = \"$h1\"" <<<"$lock_now" && grep -qx "web_dist_sha256 = \"$dist_b\"" <<<"$lock_now" || fail "the lock does not carry the new pin"
grep -q "commit = \"$(printf c%.0s {1..40})\"" <<<"$lock_now" || fail "another lock entry was edited"
[[ "$(git -C "$P/deploy.git" log -1 --format='%an <%ae>' "$b1")" == "alton2 <alton@veox.ai>" ]] || fail "the bump is not committed as alton2"
msg="$(git -C "$P/deploy.git" log -1 --format=%B "$b1")"
[[ "$msg" == "release: pin jeryu-web ${h1:0:7}"* ]] && grep -q "web change 3" <<<"$msg" && grep -q "web change 2" <<<"$msg" && ! grep -q "web change 1" <<<"$msg" || fail "the commit does not list what the bump ships"
[[ "$(posted "$P/prs.jsonl")" == 1 ]] || fail "auto-pin did not open exactly one pull request"
jq -es --arg b "$b1" --arg t "release: pin jeryu-web ${h1:0:7}" '.[0] | .head == $b and .base == "main" and .title == $t and (.body | contains("web change 3"))' "$P/prs.jsonl" >/dev/null || fail "the pull request is not the bump"
jq -es --arg to "$h1" --arg from "$w1" --arg hash "$dist_b" '.[0] | .kind == "pin.bump_opened" and .source == "auto-pin" and .pr == 7 and .repo == "jeryu/jeryu-deploy" and .needs_human == false and .detail.to == $to and .detail.from == $from and .detail.web_dist_sha256 == $hash and .detail.dependency == "jeryu/jeryu-web"' "$P/events.jsonl" >/dev/null || fail "no pin.bump_opened event"
beat_is '.last == {repo: "jeryu/jeryu-deploy", pr: 7, sha: $sha, recipe: "auto-pin", conclusion: "opened", seconds: 0, finishedAt: $at}' \
  --arg sha "$h1" --arg at "$(tail -n 1 "$P/state/opened.tsv" | cut -f3)" || fail "after opening a bump the heartbeat does not name its pull request"
if grep -rq "not-a-real-pin-token" "$P/curl-args" "$P/run.log" "$P/events.jsonl" "$P/prs.jsonl" "$P/beats.jsonl" "$P/state/logs"; then fail "the token leaked into argv or output"; fi
if grep -v -- "--config" "$P/curl-args" | grep -q .; then fail "curl did not get the token through a config file"; fi
ok "auto-pin builds, changes exactly two lock lines, and opens one pull request with what it ships"

jq -n --arg b "$b1" '[{number: 7, state: "closed", title: "x", head: {ref: $b}}]' >"$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || fail "a tick with the pull request present failed"
[[ "$(posted "$P/prs.jsonl")" == 1 && "$(lines "$P/builds")" == 1 ]] || fail "auto-pin proposed a head that already has its pull request"
jq -n '[range(1; 101) | {number: ., state: "closed", title: "x", head: {ref: "old/\(.)"}}]' >"$P/pulls.json"
jq -n --arg b "$b1" '[{number: 101, state: "open", title: "x", head: {ref: $b}}]' >"$P/pulls.json.2"
auto_pin >"$P/run.log" 2>&1 || fail "a tick with the pull request on page 2 failed"
[[ "$(posted "$P/prs.jsonl")" == 1 ]] || fail "auto-pin missed its pull request past the first page and opened another"
rm "$P/pulls.json.2"
echo '[]' >"$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "opening the pull request for a pushed branch failed"; }
[[ "$(posted "$P/prs.jsonl")" == 2 && "$(lines "$P/builds")" == 1 ]] || fail "a pushed branch without a pull request must get one without a rebuild"
echo 4 >"$P/web/index.html"; g "$P/web" commit -q -am "web change 4"; g "$P/web" push -q "$P/web.git" main
h2="$(git -C "$P/web" rev-parse HEAD)"
jq -n '[{number: 53, state: "open", title: "release: pin jeryu-web 427bebe (by hand)", head: {ref: "alton/pin-web"}}]' >"$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || fail "a tick behind an open bump failed"
grep -q "bump #53 is open" "$P/run.log" && [[ "$(branches)" == "$b1" && "$(lines "$P/builds")" == 1 ]] || fail "auto-pin opened a second bump beside an open one"
beat_is '.last.conclusion == "waiting" and .last.pr == 53 and .last.sha == $sha' --arg sha "$h2" || fail "a head behind an open bump does not beat as waiting for it"
since="$(newest "$P/beats.jsonl" | jq -r .last.finishedAt)"; sleep 1
auto_pin >/dev/null 2>&1 || fail "a second tick behind an open bump failed"
beat_is '.last.finishedAt == $since' --arg since "$since" || fail "a wait must keep the time it began"
ok "auto-pin never proposes twice: an existing pull request (on any page), a pushed branch, or any open bump"

echo '[]' >"$P/pulls.json"; touch "$P/fail"; : >"$P/events.jsonl"
if auto_pin >"$P/run.log" 2>&1; then fail "a failed build reported success"; fi
jq -es '.[0] | .kind == "pin.bump_failed" and .needs_human == false and (.log_tail | contains("vite exploded"))' "$P/events.jsonl" >/dev/null || fail "the first failure lacks its event or log tail"
: >"$P/events.jsonl"
if auto_pin >/dev/null 2>&1; then fail "the second failed build reported success"; fi
jq -es --arg id "auto-pin:failed:${h2:0:12}:2" '.[0] | .needs_human == true and .event_id == $id' "$P/events.jsonl" >/dev/null || fail "the last failure must ask for a human with a stable event_id"
: >"$P/events.jsonl"; builds="$(lines "$P/builds")"
auto_pin >"$P/run.log" 2>&1 || fail "a given-up head must be an idle tick"
grep -q "leaving it for a human" "$P/run.log" && [[ ! -s "$P/events.jsonl" && "$(lines "$P/builds")" == "$builds" ]] || fail "auto-pin kept retrying a head it gave up on"
beat_is '.last.conclusion == "failed" and .last.sha == $sha and (.last | has("pr") | not)' --arg sha "$h2" || fail "a head it gave up on does not beat as failed"
ok "auto-pin counts a failed build, asks for a human on the second, then leaves that head alone"

rm -f "$P/fail" "$P/state/failures/$h2"; touch "$P/refuse-beats"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "auto-pin failed on the second head (the forge refusing the heartbeat must not matter)"; }
[[ "$(grep -c heartbeat "$P/run.log")" == 1 ]] || fail "a refused heartbeat must cost exactly one log line"
rm -f "$P/refuse-beats"
b2="auto/pin-web-${h2:0:12}"
git -C "$P/deploy.git" update-ref refs/heads/main "refs/heads/$b2" # the bump lands
: >"$P/curl-args"; : >"$P/beats.jsonl"
JERYU_AUTO_PIN_BEAT=0 auto_pin >"$P/run.log" 2>&1 || fail "an up-to-date tick failed with the heartbeat off"
[[ ! -s "$P/curl-args" && ! -s "$P/run.log" ]] || fail "JERYU_AUTO_PIN_BEAT=0 still sent a request"
PIN_STATE="$P/state-fresh" auto_pin >"$P/run.log" 2>&1 || fail "a first-ever tick failed"
beat_is '.runnerId == $id and (has("last") | not)' --arg id "$me/auto-pin" || fail "a timer with no history must beat without a last"
: >"$P/curl-args"; builds="$(lines "$P/builds")"
auto_pin >"$P/run.log" 2>&1 || fail "an up-to-date tick failed"
[[ "$(cat "$P/curl-args")" == *"/api/v1/runners/heartbeat" && "$(lines "$P/curl-args")" == 1 && ! -s "$P/run.log" && "$(lines "$P/builds")" == "$builds" ]] || fail "an up-to-date pin is not a silent no-op"
ok "auto-pin is a silent no-op once the pin is the web head"

# A web commit that changes no shipped file (tests, docs) builds the bundle that is already pinned.
echo "a test-only change" >"$P/web/NOTES.md"; g "$P/web" add NOTES.md; g "$P/web" commit -q -m "web tests only"; g "$P/web" push -q "$P/web.git" main
h3="$(git -C "$P/web" rev-parse HEAD)"; b3="auto/pin-web-${h3:0:12}"
echo '[]' >"$P/pulls.json"; : >"$P/events.jsonl"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "auto-pin refused a head whose bundle is the pinned one"; }
lock_now="$(git -C "$P/deploy.git" show "$b3:jeryu-split.lock.toml")"
grep -qx "commit = \"$h3\"" <<<"$lock_now" && grep -qx "web_dist_sha256 = \"$dist_b\"" <<<"$lock_now" || fail "the same-bundle bump does not carry the new commit and the unchanged hash"
[[ "$(git -C "$P/deploy.git" diff --numstat main "$b3" | tr '\t' ' ')" == "1 1 jeryu-split.lock.toml" ]] || fail "a same-bundle bump must change the commit line only"
git -C "$P/deploy.git" log -1 --format=%B "$b3" | grep -q "byte-identical" || fail "the same-bundle bump does not say the bundle is unchanged"
jq -es '.[0] | .kind == "pin.bump_opened"' "$P/events.jsonl" >/dev/null || fail "the same-bundle bump posted no event"
ok "auto-pin pins a head whose bundle is unchanged by moving the commit only"

# --- deploy-release.sh: keep switch.sh's output, and say in the failure status why it failed ---
# Stand-ins: ssh that answers the staged metadata and runs a switch that prints and exits 3 (or 0),
# curl that records every posted body. The operator's home is a throwaway directory.
D="$T/deploy"; mkdir -p "$D/bin" "$D/home"
D_REL=prod-20260920T135912Z-4f7d883-unsigned
cat >"$D/bin/ssh" <<EOF
#!/usr/bin/env bash
case "\$*" in
  *RELEASE.txt*) printf 'jeryu_deploy_commit=%s x\nrollback_target=%s\nbinary_sha256=%s\nlive=jeryu-%s\nlive_commit=%s\n' \
    "\$(cat "$D/staged_commit")" "$PREV" "$(printf e%.0s {1..64})" "\$(cat "$D/live")" "\$(cat "$D/live_commit")" ;;
  *)
    echo "[switch] stopping jeryu.service"
    [ -e "$D/succeed" ] && { echo "[switch] rollback: bash ~/.jeryu/releases/x/rollback.sh"; exit 0; }
    printf '\033[31merror: health check timed out after 30s\033[0m\n' >&2
    echo "[switch] rollback: bash ~/.jeryu/releases/x/rollback.sh"
    echo; exit 3 ;;
esac
EOF
cat >"$D/bin/curl" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$D/curl-args"
data=""; prev=""
for arg in "\$@"; do [ "\$prev" = --data ] && data="\${arg#@}"; prev="\$arg"; done
case "\${!#}" in
  */health) [ -e "$D/forge-up" ] || exit 7; echo '{"service":"jeryu-api","status":"ok"}' ;;
  */statuses) jq -c . "\$data" >>"$D/statuses.jsonl"; jq -c '{state}' "\$data" ;;
  */deployments) echo '{"id":5}' ;;
esac
EOF
chmod +x "$D/bin/ssh" "$D/bin/curl"
echo "not-a-real-deploy-token" >"$D/token"
echo "$PREV" >"$D/live"
# The commits the live and the staged release are built from: an older and a newer main.
git -C "$D" init -q -b main git
git -C "$D/git" -c user.name=t -c user.email=t@t commit -q --allow-empty -m older
older="$(git -C "$D/git" rev-parse HEAD)"
git -C "$D/git" -c user.name=t -c user.email=t@t commit -q --allow-empty -m newer
newer="$(git -C "$D/git" rev-parse HEAD)"
echo "$older" >"$D/live_commit"; echo "$newer" >"$D/staged_commit"
export JERYU_DEPLOY_GIT="$D/git" JERYU_DEPLOY_REMOTE="$D/git"
deploy_release() {
  PATH="$D/bin:$PATH" HOME="$D/home" JERYU_DEPLOY_TOKEN_FILE="$D/token" bash "$here/deploy-release.sh" "$D_REL"
}
logs="$D/home/.local/state/jeryu-release/logs"
rc=0; deploy_release >"$D/run.log" 2>&1 || rc=$?
[[ $rc == 3 ]] || { cat "$D/run.log" >&2; fail "a failed switch did not fail the deploy with its exit code"; }
grep -q "health check timed out" "$D/run.log" || fail "switch output is no longer shown to the operator"
failure="$(jq -sc '.[-1]' "$D/statuses.jsonl")"
jq -e '.state == "failure" and .description == "switch.sh exited 3: error: health check timed out after 30s"' <<<"$failure" >/dev/null \
  || fail "the failure status does not say why: $failure"
jq -e '(.log_tail | contains("health check timed out")) and (.log_tail | contains("\u001b") | not)' <<<"$failure" >/dev/null \
  || fail "the failure status lacks a clean log tail: $failure"
jq -e '.log_url | test("^https?://.*deploy\\.status")' <<<"$failure" >/dev/null \
  || fail "the failure status links nowhere the switch output can be read: $failure"
log="$(jq -r .log_path <<<"$failure")"
[[ "$log" == "$logs/$D_REL-"*Z.log && -s "$log" ]] || fail "the failure status does not name the kept log: $log"
[[ "$(stat -c %a "$log")" == 600 ]] || fail "the switch log is not 0600"
grep -q "health check timed out" "$log" || fail "the switch log lacks switch's stderr"
if grep -rq "not-a-real-deploy-token" "$logs" "$D/statuses.jsonl" "$D/run.log" "$D/curl-args"; then fail "the deploy token leaked"; fi
ok "deploy-release keeps switch's output in a 0600 log and puts its last error line in the failure status"

for n in $(seq 1 31); do : >"$logs/prod-20200101T000000Z-aaaaaaa-unsigned-$n.log"; touch -d "2020-01-01 +$n min" "$logs/prod-20200101T000000Z-aaaaaaa-unsigned-$n.log"; done
touch "$D/succeed"; : >"$D/statuses.jsonl"
deploy_release >"$D/run.log" 2>&1 || { cat "$D/run.log" >&2; fail "a successful switch failed the deploy"; }
jq -se '.[-1] | .state == "success" and (.log_path | endswith(".log")) and (has("log_tail") | not)' "$D/statuses.jsonl" >/dev/null \
  || fail "the success status does not name the log"
[[ -s "$(jq -sr '.[-1].log_path' "$D/statuses.jsonl")" ]] || fail "the success log is empty"
[[ "$(find "$logs" -name '*.log' | wc -l)" == 30 ]] || fail "the switch logs are not pruned to the newest 30"
ok "deploy-release records the log path on success and keeps the newest 30 logs"

# The switched forge holds no release board, so a successful deploy refreshes every family itself:
# it waits for the forge's health endpoint before pushing, keeps the run's output beside the
# boards, and says in a receipt whether the refresh landed. Stand-in collector; the stand-in curl
# answers /health only while $D/forge-up exists.
mkdir -p "$D/board"
cat >"$D/board/collect.sh" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$D/board/args"
[ -e "$D/board/hang" ] && sleep 30
[ -e "$D/board/fail" ] && { echo "acme: push failed" >&2; exit 1; }
echo "acme: pushed (ok)"
EOF
chmod +x "$D/board/collect.sh"
boards="$D/home/.local/state/jeryu-release-board/logs"
deploy_board() { # [ARG...] — a deploy whose board collector is the stand-in, with a short wait
  PATH="$D/bin:$PATH" HOME="$D/home" JERYU_DEPLOY_TOKEN_FILE="$D/token" \
    JERYU_RELEASE_BOARD="$D/board/collect.sh" JERYU_BOARD_HEALTH_TRIES=2 \
    JERYU_BOARD_REFRESH_TIMEOUT="${BOARD_TIMEOUT:-300}" bash "$here/deploy-release.sh" "$@" "$D_REL"
}

rm -f "$D/forge-up"; : >"$D/board/args"; : >"$D/curl-args"
deploy_board >"$D/run.log" 2>&1 || { cat "$D/run.log" >&2; fail "a forge that never answers failed the deploy"; }
grep -q "boards not refreshed" "$D/run.log" || fail "the deploy did not say the refresh never ran"
[[ ! -s "$D/board/args" ]] || fail "the collector pushed to a forge that never answered"
[[ "$(grep -c '/health' "$D/curl-args")" == 2 ]] || fail "the health wait is not bounded by JERYU_BOARD_HEALTH_TRIES"
refresh="$(find "$boards" -name 'refresh-*.log' | head -1)"
[[ -s "$refresh" && "$(stat -c %a "$refresh")" == 600 ]] || fail "the refresh log is missing or not 0600: $refresh"
grep -q "never answered in 2 tries" "$refresh" || fail "the refresh log does not say the forge never answered"
ok "deploy-release waits for the forge's health endpoint and reports a refresh that could not run"

touch "$D/forge-up"; : >"$D/board/args"; rm -f "$boards"/refresh-*.log
out="$(deploy_board --json 2>"$D/run.log")" || { cat "$D/run.log" >&2; fail "a deploy with a healthy forge failed"; }
[[ "$(cat "$D/board/args")" == "--push --trigger release all" ]] || fail "the collector was not run for every family: $(cat "$D/board/args")"
grep -q "boards refreshed: 1 pushed" "$D/run.log" || fail "the deploy did not report the landed refresh"
jq -e '.board_refresh.state == "pushed" and (.board_refresh.log_path | test("/jeryu-release-board/logs/refresh-"))' <<<"$out" >/dev/null \
  || fail "the success JSON does not say the boards were refreshed: $out"
grep -q "acme: pushed (ok)" "$(jq -r .board_refresh.log_path <<<"$out")" || fail "the refresh log lacks the collector's output"
ok "deploy-release pushes every family's board once the forge answers, and logs what the collector said"

touch "$D/board/fail"
out="$(deploy_board --json 2>"$D/run.log")" || { cat "$D/run.log" >&2; fail "a failed refresh failed the deploy"; }
jq -e '.board_refresh.state == "failed"' <<<"$out" >/dev/null || fail "a failed refresh is not reported as failed: $out"
grep -q "board refresh exited 1" "$D/run.log" || fail "the deploy did not report the failed refresh"
rm -f "$D/board/fail"; touch "$D/board/hang"
out="$(BOARD_TIMEOUT=1 deploy_board --json 2>"$D/run.log")" || { cat "$D/run.log" >&2; fail "a refresh that hung failed the deploy"; }
jq -e '.board_refresh.state == "timeout"' <<<"$out" >/dev/null || fail "a refresh that hung is not reported as a timeout: $out"
rm -f "$D/board/hang"
out="$(PATH="$D/bin:$PATH" HOME="$D/home" JERYU_DEPLOY_TOKEN_FILE="$D/token" \
  JERYU_RELEASE_BOARD="$D/board/missing.sh" bash "$here/deploy-release.sh" --json "$D_REL" 2>"$D/run.log")" \
  || { cat "$D/run.log" >&2; fail "a deploy without a collector failed"; }
jq -e '.board_refresh.state == "skipped" and .board_refresh.log_path == null' <<<"$out" >/dev/null \
  || fail "a deploy without a collector does not say the refresh was skipped: $out"
ok "a board refresh that fails, hangs or is not installed is reported and never fails the deploy"

# Deploying what is already live: nothing recorded, nothing switched, no failure to alert on.
echo "$D_REL" >"$D/live"; rm -f "$D/succeed"
: >"$D/statuses.jsonl"; : >"$D/curl-args"; logs_before="$(find "$logs" -name '*.log' | wc -l)"
deploy_release >"$D/run.log" 2>&1 || { cat "$D/run.log" >&2; fail "redeploying the live release did not exit 0"; }
grep -q "already live" "$D/run.log" || fail "redeploying the live release did not say it is already live"
[[ ! -s "$D/curl-args" && ! -s "$D/statuses.jsonl" ]] || fail "redeploying the live release recorded a deployment"
[[ "$(find "$logs" -name '*.log' | wc -l)" == "$logs_before" ]] || fail "redeploying the live release wrote a log"
out="$(PATH="$D/bin:$PATH" HOME="$D/home" JERYU_DEPLOY_TOKEN_FILE="$D/token" bash "$here/deploy-release.sh" --json "$D_REL" 2>/dev/null)" \
  || fail "an already live --json deploy failed"
jq -e --arg rel "$D_REL" '.already_live and .release == $rel and .deployment_id == null and (.dry_run | not)' <<<"$out" >/dev/null \
  || fail "an already live --json deploy does not say so: $out"
echo "$PREV" >"$D/live"; touch "$D/succeed"
ok "deploy-release treats the release production already runs as a no-op"

# --json and --dry-run on deploy-release.sh: one JSON line on stdout, a distinct exit code per refusal.
: >"$D/statuses.jsonl"; : >"$D/curl-args"; logs_before="$(find "$logs" -name '*.log' | wc -l)"
out="$(PATH="$D/bin:$PATH" HOME="$D/home" JERYU_DEPLOY_TOKEN_FILE="$D/token" bash "$here/deploy-release.sh" --json --dry-run "$D_REL" 2>/dev/null)" \
  || fail "a dry-run deploy failed"
jq -e --arg rel "$D_REL" --arg prev "$PREV" '.dry_run and .release == $rel and .deployment.payload.previous_release == $prev' <<<"$out" >/dev/null \
  || fail "the dry-run JSON does not describe the deployment: $out"
[[ ! -s "$D/curl-args" && ! -s "$D/statuses.jsonl" ]] || fail "a dry-run deploy recorded something"
[[ "$(find "$logs" -name '*.log' | wc -l)" == "$logs_before" ]] || fail "a dry-run deploy wrote a log"
rc=0; out="$(PATH="$D/bin:$PATH" HOME="$D/home" bash "$here/deploy-release.sh" --json not-a-release 2>/dev/null)" || rc=$?
[[ $rc == 64 ]] && jq -e '.code == "usage" and .exit_code == 64' <<<"$out" >/dev/null || fail "a bad release id is not a usage refusal ($rc): $out"
rc=0; out="$(PATH="$D/bin:$PATH" HOME="$D/home" JERYU_DEPLOY_TOKEN_FILE="$D/missing" bash "$here/deploy-release.sh" --json "$D_REL" 2>/dev/null)" || rc=$?
[[ $rc == 77 ]] && jq -e '.code == "credential" and .exit_code == 77' <<<"$out" >/dev/null || fail "a missing token is not a credential refusal ($rc): $out"
rm -f "$D/succeed"
rc=0; out="$(PATH="$D/bin:$PATH" HOME="$D/home" JERYU_DEPLOY_TOKEN_FILE="$D/token" bash "$here/deploy-release.sh" --json "$D_REL" 2>/dev/null)" || rc=$?
[[ $rc == 3 && "$(wc -l <<<"$out")" == 1 ]] || fail "a failed --json switch did not print one line and exit 3 ($rc): $out"
jq -e '.code == "switch_failed" and .exit_code == 3 and (.message | contains("health check timed out"))' <<<"$out" >/dev/null \
  || fail "a failed --json switch is not an error envelope: $out"
touch "$D/succeed"
out="$(PATH="$D/bin:$PATH" HOME="$D/home" JERYU_DEPLOY_TOKEN_FILE="$D/token" bash "$here/deploy-release.sh" --json "$D_REL" 2>/dev/null)" \
  || fail "a successful --json deploy failed"
jq -e --arg rel "$D_REL" '.release == $rel and .deployment_id == 5 and (.log_path | endswith(".log")) and (.dry_run | not)' <<<"$out" >/dev/null \
  || fail "a successful --json deploy is not one JSON line: $out"
ok "deploy-release --json prints one line, --dry-run changes nothing, and each refusal has its own exit code"

# An older build over a newer one is refused before anything is recorded, unless asked for.
deploy_json() { PATH="$D/bin:$PATH" HOME="$D/home" JERYU_DEPLOY_TOKEN_FILE="$D/token" bash "$here/deploy-release.sh" --json "$@" "$D_REL" 2>/dev/null; }
echo "$newer" >"$D/live_commit"; echo "$older" >"$D/staged_commit"; : >"$D/curl-args"
rc=0; out="$(deploy_json)" || rc=$?
[[ $rc == 65 ]] && jq -e '.code == "state" and (.message | contains("take production back"))' <<<"$out" >/dev/null \
  || fail "a downgrade is not a state refusal ($rc): $out"
[[ ! -s "$D/curl-args" ]] || fail "a refused downgrade recorded a deployment"
out="$(deploy_json --allow-downgrade)" || fail "--allow-downgrade did not deploy the older build: $out"
jq -e '.deployment_id == 5' <<<"$out" >/dev/null || fail "--allow-downgrade did not record the deployment: $out"
: >"$D/live_commit"
rc=0; out="$(deploy_json)" || rc=$?
[[ $rc == 65 ]] && jq -e '.message | contains("cannot tell which commit")' <<<"$out" >/dev/null \
  || fail "a live release of unknown commit is not refused ($rc): $out"
echo "$older" >"$D/live_commit"; printf 6%.0s {1..40} >"$D/staged_commit"
rc=0; out="$(deploy_json)" || rc=$?
[[ $rc == 69 ]] && jq -e '.code == "unreachable"' <<<"$out" >/dev/null || fail "a commit missing even after a fetch is not unreachable ($rc): $out"
echo "$newer" >"$D/staged_commit"
out="$(deploy_json)" || fail "a newer build over an older one was refused: $out"
ok "deploy-release refuses an older build over a newer one, or one it cannot place, unless --allow-downgrade"

# --json and --dry-run on stage-release.sh. Stand-ins: git that names main, ssh that names the live
# release, has the builder image, and runs the remote build with the exit code in $S/remote-rc.
S="$T/stage"; mkdir -p "$S/bin"
S_SHA="$(printf 5%.0s {1..40})"
printf '#!/usr/bin/env bash\nprintf "%%s\\trefs/heads/main\\n" %s\n' "$S_SHA" >"$S/bin/git"
cat >"$S/bin/ssh" <<EOF
#!/usr/bin/env bash
case "\$*" in
  *readlink*) cat "$S/live" ;;
  *"image inspect"*) exit 0 ;;
  *"bash -s"*) cat >"$S/remote-script"; exit "\$(cat "$S/remote-rc")" ;;
esac
EOF
chmod +x "$S/bin/git" "$S/bin/ssh"
echo "jeryu-$PREV" >"$S/live"; echo 0 >"$S/remote-rc"
stage_release() { PATH="$S/bin:$PATH" bash "$here/stage-release.sh" "$@" 2>/dev/null; }
out="$(stage_release --dry-run)" || fail "a dry-run stage failed"
[[ "$(tail -n 1 <<<"$out")" == prod-*-5555555-unsigned ]] || fail "a dry-run stage does not end with the release id: $out"
echo 70 >"$S/remote-rc"
out="$(stage_release --json --dry-run)" || fail "a --json dry-run stage failed (it must not build)"
jq -e --arg sha "$S_SHA" --arg prev "$PREV" '.dry_run and .commit == $sha and .previous_release == $prev and (.release | endswith("-5555555-unsigned"))' <<<"$out" >/dev/null \
  || fail "the dry-run stage JSON is wrong: $out"
rc=0; out="$(stage_release --json)" || rc=$?
[[ $rc == 70 ]] && jq -e '.code == "build" and .exit_code == 70' <<<"$out" >/dev/null || fail "a too-new glibc is not a build refusal ($rc): $out"
echo 0 >"$S/remote-rc"
out="$(stage_release --json)" || fail "a --json stage failed"
jq -e '(.dry_run | not) and (.release | startswith("prod-"))' <<<"$out" >/dev/null || fail "a --json stage is not one JSON line: $out"
rc=0; out="$(stage_release --json abc)" || rc=$?
[[ $rc == 64 ]] && jq -e '.code == "usage"' <<<"$out" >/dev/null || fail "a short sha is not a usage refusal ($rc): $out"
echo "jeryu-something-else" >"$S/live"
rc=0; out="$(stage_release --json "$S_SHA")" || rc=$?
[[ $rc == 65 ]] && jq -e '.code == "state" and (.message | contains("something-else"))' <<<"$out" >/dev/null \
  || fail "an unexpected live release is not a state refusal ($rc): $out"
out="$(stage_release --json --dry-run --prev something-else "$S_SHA")" || fail "--prev did not accept the named live release"
jq -e '.previous_release == "something-else"' <<<"$out" >/dev/null || fail "--prev did not become the rollback target: $out"
rc=0; out="$(stage_release --json --dry-run --prev=other "$S_SHA")" || rc=$?
[[ $rc == 65 ]] && jq -e '.code == "state"' <<<"$out" >/dev/null || fail "--prev naming a release that is not live is not a state refusal ($rc): $out"
echo "jeryu-$PREV" >"$S/live"
rc=0; out="$(JERYU_BUILD_ROOT=relative/dir stage_release --json --dry-run)" || rc=$?
[[ $rc == 64 ]] || fail "a relative JERYU_BUILD_ROOT is not a usage refusal ($rc): $out"
JERYU_BUILD_ROOT="~/rb" stage_release >/dev/null || fail "a stage with JERYU_BUILD_ROOT=~/rb failed"
grep -qxF 'root="$HOME/rb"; mkdir -p "$root"' "$S/remote-script" || fail "~/ in JERYU_BUILD_ROOT is not the build host's home"
stage_release >/dev/null || fail "a default stage failed"
grep -qxF 'root="$HOME/jeryu-release-build"; mkdir -p "$root"' "$S/remote-script" || fail "the default build root is not under the build host's home"
ok "stage-release --json prints one line, --dry-run builds nothing, --prev overrides the live-name check, and the build root is the build host's"

# -h and --help on every release script print its header and exit 0 without running anything:
# the network, git, docker, service and file tools on PATH are tripwires.
mkdir -p "$T/tripwire"
for c in ssh scp curl git mkdir install docker systemctl; do
  printf '#!/bin/sh\necho "tripwire: %s ran" >&2; exit 97\n' "$c" >"$T/tripwire/$c"; chmod +x "$T/tripwire/$c"
done
for s in "$here"/*.sh; do
  n="$(basename "$s")"
  for flag in -h --help; do
    out="$(PATH="$T/tripwire:$PATH" bash "$s" "$flag" 2>&1)" || fail "$n $flag exited non-zero: $out"
    [[ "$(head -1 <<<"$out")" == "$n"* ]] || fail "$n $flag did not print its header: $out"
    [[ "$out" != *tripwire* ]] || fail "$n $flag ran a command: $out"
  done
done
ok "every release script answers -h and --help with its header and runs nothing"

echo "release scripts: $pass passed"
