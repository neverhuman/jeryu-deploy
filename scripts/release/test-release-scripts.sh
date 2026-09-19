#!/usr/bin/env bash
# test-release-scripts.sh — exercise the real switch.sh and rollback.sh against a
# throwaway forge home: real SQLite databases, a "binary" that is a copy of sleep
# (so /proc/<pid>/exe resolves to the installed release), a stand-in systemctl
# that runs it, and a file:// health URL. Then auto-stage.sh against a local bare
# repo with stand-in ssh and curl, and auto-pin.sh against two. No service, network or credential.
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
  "POST https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-deploy/pulls")
    [ -e "$P/refuse-pr" ] && exit 22; cat >>"$P/prs.jsonl"; echo >>"$P/prs.jsonl"; echo '{"number":7}' ;;
  "GET https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-deploy/pulls?"*) cat "$P/pulls.json" ;;
  "GET https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-web/commits/"*"/status") echo "{\"state\":\"\$(cat "$P/gate")\"}" ;;
  *) echo "unexpected curl: \$method \$url" >&2; exit 22 ;;
esac
EOF
chmod +x "$P/bin/curl"
echo "not-a-real-pin-token" >"$P/token"; chmod 600 "$P/token"
echo '[]' >"$P/pulls.json"; echo pending >"$P/gate"; : >"$P/curl-args"; : >"$P/events.jsonl"; : >"$P/prs.jsonl"; : >"$P/builds"
auto_pin() {
  PATH="$P/bin:$PATH" JERYU_DEPLOY_REMOTE="$P/deploy.git" JERYU_WEB_REMOTE="$P/web.git" \
    JERYU_DEPLOY_REPO=jeryu/jeryu-deploy JERYU_WEB_REPO=jeryu/jeryu-web JERYU_AUTO_PIN_STATE="$P/state" \
    JERYU_PIN_TOKEN_FILE="$P/token" JERYU_BASE="${PIN_BASE:-https://git.neverhuman.org}" bash "$here/auto-pin.sh"
}
branches() { git -C "$P/deploy.git" for-each-ref --format='%(refname:short)' 'refs/heads/auto/*'; }
lines() { wc -l <"$1" | tr -d ' '; }
posted() { jq -s length "$1"; }

rc=0; PIN_BASE=https://git.neverhuman.org.evil.invalid auto_pin >"$P/run.log" 2>&1 || rc=$?
[[ $rc == 2 && ! -s "$P/curl-args" ]] || fail "auto-pin sent a request to a noncanonical origin"
chmod 644 "$P/token"; rc=0; auto_pin >"$P/run.log" 2>&1 || rc=$?; chmod 600 "$P/token"
[[ $rc == 2 && ! -s "$P/curl-args" ]] || fail "auto-pin accepted a group-readable token file"
ok "auto-pin refuses a foreign origin and a loose token file before any request"

auto_pin >"$P/run.log" 2>&1 || fail "a not-green tick failed"
grep -q "gate is pending; waiting" "$P/run.log" || fail "auto-pin did not wait for the web gate"
[[ -z "$(branches)" && ! -s "$P/prs.jsonl" && ! -s "$P/builds" && ! -s "$P/events.jsonl" ]] || fail "auto-pin acted on a web head that is not green"
ok "auto-pin waits while jeryu-web main is not green"

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
if grep -rq "not-a-real-pin-token" "$P/curl-args" "$P/run.log" "$P/events.jsonl" "$P/prs.jsonl" "$P/state/logs"; then fail "the token leaked into argv or output"; fi
grep -q -- "--config" "$P/curl-args" || fail "curl did not get the token through a config file"
ok "auto-pin builds, changes exactly two lock lines, and opens one pull request with what it ships"

jq -n --arg b "$b1" '[{number: 7, state: "closed", title: "x", head: {ref: $b}}]' >"$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || fail "a tick with the pull request present failed"
[[ "$(posted "$P/prs.jsonl")" == 1 && "$(lines "$P/builds")" == 1 ]] || fail "auto-pin proposed a head that already has its pull request"
echo '[]' >"$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "opening the pull request for a pushed branch failed"; }
[[ "$(posted "$P/prs.jsonl")" == 2 && "$(lines "$P/builds")" == 1 ]] || fail "a pushed branch without a pull request must get one without a rebuild"
echo 4 >"$P/web/index.html"; g "$P/web" commit -q -am "web change 4"; g "$P/web" push -q "$P/web.git" main
h2="$(git -C "$P/web" rev-parse HEAD)"
jq -n '[{number: 53, state: "open", title: "release: pin jeryu-web 427bebe (by hand)", head: {ref: "alton/pin-web"}}]' >"$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || fail "a tick behind an open bump failed"
grep -q "bump #53 is open" "$P/run.log" && [[ "$(branches)" == "$b1" && "$(lines "$P/builds")" == 1 ]] || fail "auto-pin opened a second bump beside an open one"
ok "auto-pin never proposes twice: an existing pull request, a pushed branch, or any open bump"

echo '[]' >"$P/pulls.json"; touch "$P/fail"; : >"$P/events.jsonl"
if auto_pin >"$P/run.log" 2>&1; then fail "a failed build reported success"; fi
jq -es '.[0] | .kind == "pin.bump_failed" and .needs_human == false and (.log_tail | contains("vite exploded"))' "$P/events.jsonl" >/dev/null || fail "the first failure lacks its event or log tail"
: >"$P/events.jsonl"
if auto_pin >/dev/null 2>&1; then fail "the second failed build reported success"; fi
jq -es --arg id "auto-pin:failed:${h2:0:12}:2" '.[0] | .needs_human == true and .event_id == $id' "$P/events.jsonl" >/dev/null || fail "the last failure must ask for a human with a stable event_id"
: >"$P/events.jsonl"; builds="$(lines "$P/builds")"
auto_pin >"$P/run.log" 2>&1 || fail "a given-up head must be an idle tick"
grep -q "leaving it for a human" "$P/run.log" && [[ ! -s "$P/events.jsonl" && "$(lines "$P/builds")" == "$builds" ]] || fail "auto-pin kept retrying a head it gave up on"
ok "auto-pin counts a failed build, asks for a human on the second, then leaves that head alone"

rm -f "$P/fail" "$P/state/failures/$h2"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "auto-pin failed on the second head"; }
b2="auto/pin-web-${h2:0:12}"
git -C "$P/deploy.git" update-ref refs/heads/main "refs/heads/$b2" # the bump lands
: >"$P/curl-args"; builds="$(lines "$P/builds")"
auto_pin >"$P/run.log" 2>&1 || fail "an up-to-date tick failed"
[[ ! -s "$P/curl-args" && ! -s "$P/run.log" && "$(lines "$P/builds")" == "$builds" ]] || fail "an up-to-date pin is not a silent no-op"
ok "auto-pin is a silent no-op once the pin is the web head"

echo "release scripts: $pass passed"
