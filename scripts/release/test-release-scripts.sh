#!/usr/bin/env bash
# test-release-scripts.sh — exercise the real switch.sh and rollback.sh against a
# throwaway forge home: real SQLite databases, a "binary" that is a copy of sleep
# (so /proc/<pid>/exe resolves to the installed release), a stand-in systemctl
# that runs it, and a file:// health URL. Then auto-stage.sh against a local bare
# repo with stand-in ssh and curl, and auto-pin.sh against two, verify-pin-bump.sh against a
# stand-in forge, and auto-tag.sh against crate remotes and a stand-in forge. No service,
# network or credential.
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
export JERYU_FORGE_LIMITS_ENV="$T/forge-limits.env" JERYU_SYSTEMD_USER_DIR="$T/units"
# A site that has budgeted the forge. The numbers are this test's, not any host's.
cat >"$JERYU_FORGE_LIMITS_ENV" <<'EOF'
JERYU_FORGE_MEMORY_HIGH=3G
JERYU_FORGE_MEMORY_MAX=4G
JERYU_FORGE_TASKS_MAX=512
JERYU_FORGE_OOM_SCORE_ADJ=-500
JERYU_MIRROR_REMOTE_ALLOWLIST=github.com/acme
EOF
PREV=prod-20260101T000000Z-aaaaaaa-unsigned REL=prod-20260102T000000Z-bbbbbbb-unsigned
mkdir -p "$JERYU_HOME"/{bin,share,incoming,releases,backups} "$JERYU_DATA"
printf '{"service":"jeryu-api","status":"ok"}\n' >"$T/health.json"
cat >"$T/systemctl" <<EOF
#!/usr/bin/env bash
# stand-in: --user {start|stop|show ... --value} jeryu.service
pidfile="$T/pid"
case "\$2" in
  start)
    # Like Type=simple: return once forked, before the service execs the release,
    # so switch.sh's MainPID/exe check always meets the pre-exec window.
    bash -c 'sleep "\$1"; exec "\$2" 300' _ "\${JERYU_TEST_EXEC_DELAY:-0.2}" "$JERYU_HOME/bin/jeryu" &
    echo \$! >"\$pidfile"
    ;;
  stop) [[ -s "\$pidfile" ]] && kill "\$(cat "\$pidfile")" 2>/dev/null; rm -f "\$pidfile"; true ;;
  show) cat "\$pidfile" ;;
esac
EOF
chmod +x "$T/systemctl"

for n in forge work codegraph shift; do
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
  cp "$here/switch.sh" "$here/rollback.sh" "$here/install-forge-unit.sh" "$here/systemd/jeryu.service.in" "$d/"
  printf 'REL=%s\nPREV=%s\n' "$REL" "$PREV" >"$d/RELEASE.env"
  (cd "$d" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum >SHA256SUMS)
}

stage
echo tampered >>"$JERYU_HOME/incoming/$REL/web-dist/index.html"
if bash "$JERYU_HOME/incoming/$REL/switch.sh" >/dev/null 2>&1; then fail "switch accepted a tampered stage"; fi
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$PREV" ]] || fail "a refused switch moved the live symlink"
ok "switch refuses a stage whose checksums fail, and changes nothing"

# A site that has not budgeted the forge is refused while production is still up:
# an unbounded forge is what takes a host down, so it is never what gets installed.
stage
"$JERYU_SYSTEMCTL" --user start jeryu.service
mv "$JERYU_FORGE_LIMITS_ENV" "$T/limits.aside"
rc=0; bash "$JERYU_HOME/incoming/$REL/switch.sh" >"$T/switch-unbudgeted.log" 2>&1 || rc=$?
[[ $rc != 0 ]] || fail "switch installed a release for a site with no limits file"
grep -q "no forge limits file" "$T/switch-unbudgeted.log" || fail "switch did not name the missing limits file"
if grep -q "stopping jeryu.service" "$T/switch-unbudgeted.log"; then fail "a refused unit install still stopped production"; fi
[[ -s "$T/pid" ]] || fail "a refused unit install stopped the running forge"
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$PREV" ]] || fail "a refused unit install moved the live symlink"
[[ ! -e "$JERYU_SYSTEMD_USER_DIR/jeryu.service" ]] || fail "a refused unit install wrote a unit anyway"
mv "$T/limits.aside" "$JERYU_FORGE_LIMITS_ENV"
ok "switch refuses a site that has not budgeted the forge, before it stops anything"

# A site drop-in that rebuilds ExecStart from a variable nothing defines is what
# crash-looped the forge once the old release was gone: refuse it while that
# release is still serving, and accept it once an env file defines the variable.
unit="$JERYU_SYSTEMD_USER_DIR/jeryu.service"
mkdir -p "$JERYU_SYSTEMD_USER_DIR/jeryu.service.d"
cat >"$JERYU_SYSTEMD_USER_DIR/jeryu.service.d/10-site.conf" <<'EOF'
[Service]
ExecStart=
ExecStart=%h/.jeryu/bin/jeryu serve --bind ${JERYU_SERVE_BIND}
EOF
stage
"$JERYU_SYSTEMCTL" --user start jeryu.service
rc=0; bash "$JERYU_HOME/incoming/$REL/switch.sh" >"$T/switch-unset-var.log" 2>&1 || rc=$?
[[ $rc != 0 ]] || fail "switch installed a unit whose drop-in needs an undefined variable"
grep -q "JERYU_SERVE_BIND" "$T/switch-unset-var.log" || fail "the refusal does not name JERYU_SERVE_BIND"
if grep -q "stopping jeryu.service" "$T/switch-unset-var.log"; then fail "a refused unit install still stopped production"; fi
[[ -s "$T/pid" ]] || fail "a refused unit install stopped the running forge"
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$PREV" ]] || fail "a refused unit install moved the live symlink"
[[ ! -e "$unit" ]] || fail "a refused unit install wrote a unit anyway"
ok "install-forge-unit refuses a drop-in needing a variable no environment file defines, before any stop"

# The same drop-in, plus a site env file a second drop-in names, is accepted.
printf '# the site s own forge settings\n\nJERYU_SERVE_BIND=127.0.0.1:1\n' >"$T/site.env"
printf '[Service]\nEnvironmentFile=%s\n' "$T/site.env" >"$JERYU_SYSTEMD_USER_DIR/jeryu.service.d/20-site-env.conf"
bash "$here/install-forge-unit.sh" >"$T/unit-dropin.log" 2>&1 \
  || { cat "$T/unit-dropin.log" >&2; fail "a drop-in whose variable is defined was refused"; }
grep -q "installed jeryu.service" "$T/unit-dropin.log" || fail "the installer did not say what it installed"
rm -rf "$JERYU_SYSTEMD_USER_DIR/jeryu.service.d" "$unit"
ok "install-forge-unit accepts the same drop-in once an environment file defines the variable"

# Validate the service's config, even when the caller has an allowlist.
stage
"$JERYU_SYSTEMCTL" --user start jeryu.service
cp "$JERYU_FORGE_LIMITS_ENV" "$T/limits-with-mirror"
sed '/^JERYU_MIRROR_REMOTE_ALLOWLIST=/d' "$T/limits-with-mirror" >"$JERYU_FORGE_LIMITS_ENV"
for value in missing empty separators; do
  cp "$JERYU_FORGE_LIMITS_ENV" "$T/mirror-env-base"
  case "$value" in
    empty) echo 'JERYU_MIRROR_REMOTE_ALLOWLIST=""' >>"$JERYU_FORGE_LIMITS_ENV" ;;
    separators) echo 'JERYU_MIRROR_REMOTE_ALLOWLIST=" , "' >>"$JERYU_FORGE_LIMITS_ENV" ;;
  esac
  rc=0
  JERYU_MIRROR_REMOTE_ALLOWLIST=github.com/shell-only bash "$JERYU_HOME/incoming/$REL/switch.sh" >"$T/mirror-unset.log" 2>&1 || rc=$?
  [[ $rc != 0 ]] || fail "switch accepted $value mirror allowlist"
  grep -q 'JERYU_MIRROR_REMOTE_ALLOWLIST' "$T/mirror-unset.log" || fail "refusal did not name mirror allowlist"
  if grep -q 'stopping jeryu.service' "$T/mirror-unset.log"; then fail "missing allowlist stopped production"; fi
  kill -0 "$(cat "$T/pid")" || fail "missing allowlist stopped running forge"
  [[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$PREV" ]] || fail "missing allowlist moved live release"
  [[ ! -e "$JERYU_SYSTEMD_USER_DIR/jeryu.service" ]] || fail "missing allowlist installed unit"
  cp "$T/mirror-env-base" "$JERYU_FORGE_LIMITS_ENV"
done
echo 'JERYU_GITHUB_PUSH=0' >>"$JERYU_FORGE_LIMITS_ENV"
bash "$here/install-forge-unit.sh" >"$T/mirror-disabled.log" 2>&1 || fail "explicitly disabled mirrors refused"
rm "$JERYU_SYSTEMD_USER_DIR/jeryu.service"
cp "$T/limits-with-mirror" "$JERYU_FORGE_LIMITS_ENV"
mkdir -p "$JERYU_SYSTEMD_USER_DIR/jeryu.service.d"
printf 'JERYU_MIRROR_REMOTE_ALLOWLIST=\n' >"$T/mirror.env"
printf '[Service]\nEnvironmentFile=%s\n' "$T/mirror.env" >"$JERYU_SYSTEMD_USER_DIR/jeryu.service.d/30-mirror.conf"
if bash "$here/install-forge-unit.sh" >"$T/mirror-overridden.log" 2>&1; then fail "empty override accepted"; fi
printf 'JERYU_MIRROR_REMOTE_ALLOWLIST="github.com/acme github.com/example"\n' >"$T/mirror.env"
bash "$here/install-forge-unit.sh" >"$T/mirror-configured.log" 2>&1 || fail "configured mirror refused"
rm "$JERYU_SYSTEMD_USER_DIR/jeryu.service.d/30-mirror.conf" "$JERYU_SYSTEMD_USER_DIR/jeryu.service"
rmdir "$JERYU_SYSTEMD_USER_DIR/jeryu.service.d"
ok "mirror allowlist preflight refuses missing or empty config before stopping, and accepts configured or disabled mirrors"

# Unit syntax the preflight does not expect must not read as a crash: a word
# that assigns nothing is ignored like systemd ignores it, and a file it cannot
# read or parse is refused by name instead of as a traceback.
mkdir -p "$JERYU_SYSTEMD_USER_DIR/jeryu.service.d"
dropin="$JERYU_SYSTEMD_USER_DIR/jeryu.service.d/30-mirror.conf"
printf '[Service]\nEnvironment=JERYU_MIRROR_REMOTE_ALLOWLIST=github.com/acme NOT_AN_ASSIGNMENT\n' >"$dropin"
bash "$here/install-forge-unit.sh" >"$T/mirror-odd-word.log" 2>&1 \
  || { cat "$T/mirror-odd-word.log" >&2; fail "a word assigning nothing was read as a failure"; }
rm "$JERYU_SYSTEMD_USER_DIR/jeryu.service"
printf 'JERYU_MIRROR_REMOTE_ALLOWLIST="github.com/acme\n' >"$T/mirror-unparsable.env"
printf '[Service]\nEnvironmentFile=%s\n' "$T/mirror-unparsable.env" >"$dropin"
rc=0; bash "$here/install-forge-unit.sh" >"$T/mirror-unparsable.log" 2>&1 || rc=$?
[[ $rc != 0 ]] || fail "an unparsable environment file was accepted"
grep -q "cannot parse" "$T/mirror-unparsable.log" || fail "the refusal does not say the line cannot be parsed"
grep -q "mirror-unparsable.env" "$T/mirror-unparsable.log" || fail "the refusal does not name the file"
if grep -q "Traceback" "$T/mirror-unparsable.log"; then fail "the preflight answered with a traceback"; fi
[[ ! -e "$JERYU_SYSTEMD_USER_DIR/jeryu.service" ]] || fail "an unparsable environment file installed a unit"
printf 'JERYU_MIRROR_REMOTE_ALLOWLIST=github.com/acme\n' >"$T/mirror-unreadable.env"
chmod 000 "$T/mirror-unreadable.env"
printf '[Service]\nEnvironmentFile=%s\n' "$T/mirror-unreadable.env" >"$dropin"
rc=0; bash "$here/install-forge-unit.sh" >"$T/mirror-unreadable.log" 2>&1 || rc=$?
[[ $rc != 0 ]] || fail "an unreadable environment file was accepted"
grep -q "cannot be read" "$T/mirror-unreadable.log" || fail "the refusal does not say the file cannot be read"
if grep -q "Traceback" "$T/mirror-unreadable.log"; then fail "the preflight answered with a traceback"; fi
[[ ! -e "$JERYU_SYSTEMD_USER_DIR/jeryu.service" ]] || fail "an unreadable environment file installed a unit"
chmod 600 "$T/mirror-unreadable.env"
rm "$dropin"
rmdir "$JERYU_SYSTEMD_USER_DIR/jeryu.service.d"
ok "the mirror allowlist preflight reports unreadable unit syntax by name instead of crashing"

stage
"$JERYU_SYSTEMCTL" --user start jeryu.service
for n in forge work codegraph shift; do
  pid_before="$(cat "$T/pid")"
  mv "$JERYU_DATA/$n.sqlite" "$T/$n.aside"
  if bash "$JERYU_HOME/incoming/$REL/switch.sh" >"$T/missing-store.log" 2>&1; then fail "switch accepted missing $n"; fi
  grep -q "$n.sqlite" "$T/missing-store.log" || fail "switch did not name missing $n"
  [[ "$(cat "$T/pid")" == "$pid_before" ]] && kill -0 "$pid_before" || fail "missing store stopped production"
  mv "$T/$n.aside" "$JERYU_DATA/$n.sqlite"
done
ok "switch validates every store before stopping production"

bash "$JERYU_HOME/incoming/$REL/switch.sh" >"$T/switch.log" 2>&1 || { cat "$T/switch.log" >&2; fail "switch failed"; }
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$REL" ]] || fail "bin symlink not moved to REL"
[[ "$(readlink "$JERYU_HOME/share/web-dist")" == "web-dist-$REL" ]] || fail "web-dist symlink not moved to REL"
[[ -x "$JERYU_HOME/releases/$REL/rollback.sh" && -s "$JERYU_HOME/releases/$REL/RELEASE.env" ]] || fail "release dir lacks rollback.sh/RELEASE.env"
grep -q "integrity=ok" "$T/switch.log" || fail "no snapshot integrity line"
[[ "$(readlink -f "/proc/$(cat "$T/pid")/exe")" == "$(readlink -f "$JERYU_HOME/bin/jeryu-$REL")" ]] || fail "running exe is not REL"
ok "switch installs REL, repoints both symlinks, snapshots every database and proves the running binary"

# The unit the release just installed carries the site's limits, not the repository's.
unit="$JERYU_SYSTEMD_USER_DIR/jeryu.service"
[[ -f "$unit" ]] || fail "the switch did not install jeryu.service"
if grep -q '@[A-Z_]*@' "$unit"; then fail "the installed unit still has a template placeholder"; fi
for want in "MemoryHigh=3G" "MemoryMax=4G" "TasksMax=512" "OOMScoreAdjust=-500" \
  "ExecStart=%h/.jeryu/bin/jeryu serve" "Restart=on-failure" "NoNewPrivileges=true" "PrivateTmp=true" "Type=simple"; do
  grep -qxF "$want" "$unit" || fail "the installed unit lacks $want"
done
grep -qxF "EnvironmentFile=$JERYU_FORGE_LIMITS_ENV" "$unit" \
  || fail "the unit does not pass the site limits file to the forge"
ok "switch installs a jeryu.service carrying the site's memory, task and OOM limits"

# Every refusal, and never a half-installed unit. Each bad site is refused by name.
rm -f "$unit"
install_unit() { JERYU_FORGE_LIMITS_ENV="$T/bad.env" bash "$here/install-forge-unit.sh"; }
bad_site() { # MESSAGE-FRAGMENT LINES... — a limits file that must be refused, saying why
  local want="$1"; shift
  printf '%s\n' "$@" >"$T/bad.env"
  local out rc=0
  out="$(install_unit 2>&1)" || rc=$?
  [[ $rc != 0 ]] || fail "install-forge-unit accepted a site whose limits are '$want'"
  grep -q -- "$want" <<<"$out" || fail "the refusal does not mention $want: $out"
  [[ ! -e "$unit" ]] || fail "a refused install wrote a unit"
}
full=(JERYU_FORGE_MEMORY_HIGH=3G JERYU_FORGE_MEMORY_MAX=4G JERYU_FORGE_TASKS_MAX=512
  JERYU_FORGE_OOM_SCORE_ADJ=-500 JERYU_MIRROR_REMOTE_ALLOWLIST=github.com/acme)
for name in JERYU_FORGE_MEMORY_HIGH JERYU_FORGE_MEMORY_MAX JERYU_FORGE_TASKS_MAX \
  JERYU_FORGE_OOM_SCORE_ADJ; do
  bad_site "does not set $name" "${full[@]/#$name=*/}"
done
bad_site "not a size" "${full[@]/#JERYU_FORGE_MEMORY_MAX=*/JERYU_FORGE_MEMORY_MAX=plenty}"
bad_site "not a size" "${full[@]/#JERYU_FORGE_MEMORY_HIGH=*/JERYU_FORGE_MEMORY_HIGH=2GB}"
bad_site "not a positive count" "${full[@]/#JERYU_FORGE_TASKS_MAX=*/JERYU_FORGE_TASKS_MAX=0}"
bad_site "not an oom_score_adj" "${full[@]/#JERYU_FORGE_OOM_SCORE_ADJ=*/JERYU_FORGE_OOM_SCORE_ADJ=-2000}"
bad_site "must be below JERYU_FORGE_MEMORY_MAX" "${full[@]/#JERYU_FORGE_MEMORY_HIGH=*/JERYU_FORGE_MEMORY_HIGH=8G}"
printf '%s\n' "${full[@]}" >"$T/bad.env"
install_unit >"$T/unit-install.log" 2>&1 || { cat "$T/unit-install.log" >&2; fail "a budgeted site was refused"; }
grep -q "installed jeryu.service" "$T/unit-install.log" || fail "the installer did not say what it installed"
install_unit >"$T/unit-again.log" 2>&1 || fail "re-running the installer failed"
grep -q "already the current one" "$T/unit-again.log" || fail "an unchanged unit was rewritten"
ok "install-forge-unit refuses every unbudgeted or contradictory site by name, and is idempotent"

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

for n in forge work codegraph shift; do
  pid_before="$(cat "$T/pid")"
  mv "$JERYU_HOME/backups/pre-$REL/$n.sqlite" "$T/$n.snapshot"
  if bash "$JERYU_HOME/releases/$REL/rollback.sh" >"$T/missing-snapshot.log" 2>&1; then fail "rollback accepted missing $n snapshot"; fi
  grep -q "$n.sqlite" "$T/missing-snapshot.log" || fail "rollback did not name missing $n snapshot"
  [[ "$(cat "$T/pid")" == "$pid_before" ]] && kill -0 "$pid_before" || fail "missing snapshot stopped production"
  mv "$T/$n.snapshot" "$JERYU_HOME/backups/pre-$REL/$n.sqlite"
done
ok "rollback validates every snapshot before stopping production"

python3 -c "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute(\"update t set v='after'\"); c.commit()" "$JERYU_DATA/forge.sqlite"
python3 -c "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute(\"update t set v='after'\"); c.commit()" "$JERYU_DATA/shift.sqlite"
bash "$JERYU_HOME/releases/$REL/rollback.sh" >"$T/rollback.log" 2>&1 || { cat "$T/rollback.log" >&2; fail "rollback failed"; }
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$PREV" ]] || fail "rollback did not repoint bin to PREV"
[[ "$(readlink "$JERYU_HOME/share/web-dist")" == "web-dist-$PREV" ]] || fail "rollback did not repoint web-dist to PREV"
[[ "$(db_value)" == before ]] || fail "rollback did not restore the pre-switch database"
kept="$(find "$JERYU_HOME/backups" -maxdepth 1 -name "post-$REL-*" | head -1)"
[[ "$(python3 -c "import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute('select v from t').fetchone()[0])" "$kept/forge.sqlite")" == after ]] \
  || fail "rollback did not keep the post-switch database aside"
[[ "$(python3 -c "import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute('select v from t').fetchone()[0])" "$JERYU_DATA/shift.sqlite")" == before ]] || fail "rollback did not restore the Flow store"
[[ "$(python3 -c "import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute('select v from t').fetchone()[0])" "$kept/shift.sqlite")" == after ]] || fail "rollback did not preserve the post-switch Flow store"
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
mv "$JERYU_DATA/shift.sqlite" "$T/damaged-live-shift.sqlite"
bash "$JERYU_HOME/releases/$REL/rollback.sh" >"$T/rollback-missing-live.log" 2>&1 || fail "rollback after an unhealthy switch failed"
grep -q "live store shift.sqlite is missing" "$T/rollback-missing-live.log" || fail "rollback did not record missing live state"
[[ "$(python3 -c "import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute('select v from t').fetchone()[0])" "$JERYU_DATA/shift.sqlite")" == before ]] || fail "rollback did not restore missing live state"
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$PREV" ]] || fail "rollback after an unhealthy switch did not restore PREV"
REL="$REL_OK"
ok "switch fails when its health check never passes, and rollback recovers"

# A service that never execs REL within the bounded re-reads fails the switch: the
# exe check waits for the fork/exec window, it does not skip it.
REL_OK="$REL"; REL=prod-20260103T000000Z-fffffff-unsigned
stage
rc=0; JERYU_TEST_EXEC_DELAY=5 JERYU_EXE_TRIES=3 \
  bash "$JERYU_HOME/incoming/$REL/switch.sh" >"$T/switch-no-exec.log" 2>&1 || rc=$?
[[ $rc != 0 ]] || fail "switch reported success though the service never ran REL"
grep -q "is not $JERYU_HOME/bin/jeryu-$REL after 3 reads" "$T/switch-no-exec.log" \
  || { cat "$T/switch-no-exec.log" >&2; fail "a switch whose service never ran REL did not say so"; }
bash "$JERYU_HOME/releases/$REL/rollback.sh" >/dev/null 2>&1 || fail "rollback after a never-exec'd switch failed"
[[ "$(readlink "$JERYU_HOME/bin/jeryu")" == "jeryu-$PREV" ]] || fail "rollback after a never-exec'd switch did not restore PREV"
REL="$REL_OK"
ok "switch waits a bounded time for the service to exec REL, then fails and rollback recovers"

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
  *"/commits/"*"/status"*)
    jq -n --arg state "\$(cat "$A/health" 2>/dev/null || echo success)" '{state: \$state, statuses: [
      {context: "jeryu-deploy/main-health", state: \$state, updated_at: "2026-10-15T16:49:48Z"},
      {context: "jeryu-deploy/main-health", state: "failure", updated_at: "2026-10-15T16:40:22Z"}]}' ;;
  *) echo '{"state":"success"}' ;;
esac
EOF
chmod +x "$A/bin/ssh" "$A/bin/curl"
echo "not-a-real-token" >"$A/token"
# The hosts and the credential path are site settings with no default, so every
# invocation names them; the stand-in ssh ignores which host it was given.
# It runs from a copy placed in a fixture home, so the checkout and settings file the deploy
# command names are the fixture's and the same wherever this test runs.
H="$A/home"; mkdir -p "$H/deploy-checkout/scripts/release" "$H/.config/jeryu"
cp "$here/auto-stage.sh" "$here/deploy-release.sh" "$here/latest-statuses.jq" "$H/deploy-checkout/scripts/release/"
cat >"$H/.config/jeryu/release.env" <<EOF
JERYU_BUILD_HOST=build-1
JERYU_FORGE_HOST=edge-1
JERYU_DEPLOY_TOKEN_FILE=$A/token
JERYU_HEALTH_URL=http://127.0.0.1:8787/health
EOF
auto_stage() {
  env -u JERYU_RELEASE_ENV -u JERYU_RELEASE_ACTION -u XDG_CONFIG_HOME HOME="$H" PATH="$A/bin:$PATH" \
    JERYU_DEPLOY_REMOTE="$A/remote.git" JERYU_DEPLOY_REPO=jeryu/jeryu-deploy JERYU_AUTO_STAGE_STATE="$A/state" \
    JERYU_BUILD_HOST="${STAGE_BUILD_HOST-build.invalid}" JERYU_FORGE_HOST="${STAGE_FORGE_HOST-forge.invalid}" \
    JERYU_STATUS_TOKEN_FILE="${STAGE_TOKEN_FILE-$A/token}" JERYU_BASE="https://forge.invalid" \
    bash "$H/deploy-checkout/scripts/release/auto-stage.sh"
}

# Each site setting is refused by name, before anything is staged or posted.
for unset_setting in JERYU_BUILD_HOST JERYU_FORGE_HOST JERYU_STATUS_TOKEN_FILE; do
  rc=0
  case "$unset_setting" in
    JERYU_BUILD_HOST) STAGE_BUILD_HOST= auto_stage >"$A/unset.log" 2>&1 || rc=$? ;;
    JERYU_FORGE_HOST) STAGE_FORGE_HOST= auto_stage >"$A/unset.log" 2>&1 || rc=$? ;;
    *) STAGE_TOKEN_FILE= auto_stage >"$A/unset.log" 2>&1 || rc=$? ;;
  esac
  [[ $rc == 64 ]] || fail "auto-stage ran without $unset_setting"
  grep -q "$unset_setting is unset" "$A/unset.log" || fail "the refusal does not name $unset_setting"
done
ok "auto-stage refuses each unset site setting by name"

# Nothing of main is proved green yet: there is no commit to stage, and no status is read to guess
# one. staged.tsv is created by every tick, so "nothing staged" is an empty file.
# A branch whose name spells the green ref (refs/heads/refs/green/main) matches a bare ls-remote
# pattern; it must not stand in for the ref. It stays for the rest of the suite.
git -C "$A/remote.git" update-ref refs/heads/refs/green/main "$sha"
auto_stage >"$A/no-green.log" 2>&1 || fail "auto-stage failed while there was no green ref"
grep -q "has no refs/green/main yet" "$A/no-green.log" || { cat "$A/no-green.log" >&2; fail "auto-stage did not say it awaits the green ref (or took the spoofing branch)"; }
[[ ! -s "$A/state/staged.tsv" && ! -s "$A/events.jsonl" ]] || fail "auto-stage staged without a green ref"
jq -sc '.[-1]' "$A/beats.jsonl" | jq -e --arg sha "$sha" '.last.conclusion == "waiting" and .last.sha == $sha' >/dev/null \
  || fail "a tick waiting for the green ref does not beat waiting for main's head"
rm -rf "$A/state"
ok "auto-stage stages nothing until the green ref names a commit, and a branch spelling it is not it"

# The gate proves the first commit; main then moves on. Everything below asks for $sha, the commit
# the green ref names, so each assertion is also proof that staging follows the ref and not main's
# head -- which is the head a rebased landing leaves with no status at all.
git -C "$A/remote.git" update-ref refs/green/main "$sha"
echo later >"$A/src/later.txt"
git -C "$A/src" -c user.name=t -c user.email=t@t add later.txt
git -C "$A/src" -c user.name=t -c user.email=t@t commit -q -m later
git -C "$A/src" push -q "$A/remote.git" main:main
newer_main="$(git -C "$A/remote.git" rev-parse refs/heads/main)"
[[ "$newer_main" != "$sha" ]] || fail "the fixture did not move main past the green commit"

# The ref is not trusted alone: an off-main commit is refused, a commit whose newest main-health
# is not success waits, and neither is staged.
off_main="$(git -C "$A/remote.git" -c user.name=t -c user.email=t@t commit-tree -m side "$sha^{tree}" -p "$sha")"
git -C "$A/remote.git" update-ref refs/heads/side "$off_main"
git -C "$A/remote.git" update-ref refs/green/main "$off_main"
rc=0; auto_stage >"$A/off-main.log" 2>&1 || rc=$?
[[ $rc == 1 ]] && grep -q "is not a commit of main; refusing" "$A/off-main.log" \
  || { cat "$A/off-main.log" >&2; fail "auto-stage did not refuse a green ref off main"; }
[[ ! -s "$A/state/staged.tsv" && ! -s "$A/events.jsonl" ]] || fail "auto-stage staged an off-main commit"
git -C "$A/remote.git" update-ref refs/green/main "$sha"; git -C "$A/remote.git" update-ref -d refs/heads/side
rm -rf "$A/state"
echo failure >"$A/health"; : >"$A/beats.jsonl"
auto_stage >"$A/red-health.log" 2>&1 || fail "a tick with a red main-health failed"
grep -q "has jeryu-deploy/main-health failure; waiting" "$A/red-health.log" || { cat "$A/red-health.log" >&2; fail "auto-stage did not wait on a red main-health"; }
[[ ! -s "$A/state/staged.tsv" && ! -s "$A/events.jsonl" ]] || fail "auto-stage staged a commit whose main-health is red"
jq -sc '.[-1]' "$A/beats.jsonl" | jq -e --arg sha "$sha" '.last.conclusion == "waiting" and .last.sha == $sha' >/dev/null \
  || fail "a tick waiting on main-health does not beat waiting"
rm -f "$A/health"; rm -rf "$A/state"; : >"$A/beats.jsonl"
ok "auto-stage refuses a green ref off main and waits for a green main-health on the green commit"

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
auto_stage >"$A/run-staged.log" 2>&1 || fail "auto-stage failed"
[[ "$(jq -r '.kind' "$A/events.jsonl")" == release.staged ]] || fail "no release.staged event"
want_line="cd ~/deploy-checkout && set -a && . ~/.config/jeryu/release.env && set +a && scripts/release/deploy-release.sh $want"
[[ "$(jq -r '.detail.deploy_command' "$A/events.jsonl")" == "$want_line" ]] || fail "staged event lacks the self-contained deploy command"
[[ "$(jq -r '.detail.previous_release' "$A/events.jsonl")" == "$PREV" ]] || fail "staged event lacks the live release"
[[ "$(jq -r '.sha' "$A/events.jsonl")" == "$sha" ]] || fail "staged event names the wrong commit"
: >"$A/events.jsonl"
auto_stage >/dev/null 2>&1 || fail "an idle auto-stage tick failed"
[[ ! -s "$A/events.jsonl" ]] || fail "an already-staged commit emitted again"
ok "auto-stage reports a staged release once, with the deploy command and what is live"

# The line the row shows runs as pasted: a fresh login shell in ~ with no site settings in its
# environment, only what the line loads. --dry-run stops before anything is recorded or switched.
mkdir -p "$A/dbin"
cat >"$A/dbin/ssh" <<EOF
#!/usr/bin/env bash
printf 'jeryu_deploy_commit=%s\nbinary_sha256=%s\nlive=jeryu-%s\nlive_commit=%s\n' "$sha" "$(printf '0%.0s' {1..64})" "$PREV" "$sha"
EOF
chmod +x "$A/dbin/ssh"
dry="$(env -i HOME="$H" PATH="$A/dbin:/usr/bin:/bin" bash -lc "cd ~ && $want_line --dry-run --allow-downgrade" 2>&1)" \
  || { echo "$dry" >&2; fail "the staged deploy command does not run as pasted"; }
[[ "$dry" == *"[dry-run] would switch edge-1 to $want"* ]] || fail "the pasted deploy command did not load the site settings: $dry"
grep -qF "STAGED $want; deploy with: $want_line" "$A/run-staged.log" || fail "the STAGED log line is not the event's deploy command"
ok "the staged deploy command runs as pasted from ~ with the site settings loaded, and the STAGED line prints it"

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
for n in 1 2 3; do
  echo "$n" >"$P/web/index.html"
  if [ "$n" = 3 ]; then
    # Both producer outputs exceed a pipe buffer. Commit text and names
    # are untrusted review data and must not be copied into the PR body.
    for i in {1..1500}; do
      printf -v name '%04d-%0100d-review-data.txt' "$i" "$i"
      printf 'fixture\n' >"$P/web/$name"
    done
    { printf 'web change 3\n\nNote: preserve this complete message on the forge.\n';
      for i in {1..2000}; do printf 'REVIEW_DATA_DO_NOT_COPY %0100d\n' "$i"; done;
    } >"$P/large-message"
    g "$P/web" add .
    g "$P/web" commit -q -F "$P/large-message"
  else
    g "$P/web" add .
    g "$P/web" commit -q -m "web change $n"
  fi
done
git clone -q --bare "$P/web" "$P/web.git"
w1="$(git -C "$P/web" rev-parse HEAD~2)"; h1="$(git -C "$P/web" rev-parse HEAD)"
# The gate has proved jeryu-web main's head, so the forge has moved its green ref there. auto-pin
# reads that ref and never jeryu-web's own head, so a commit the gate has not reached is not pinned.
git -C "$P/web.git" update-ref refs/green/main "$h1"
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
[ ! -e "$P/post-build-pulls.json" ] || cp "$P/post-build-pulls.json" "$P/pulls.json"
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
    [ -e "$P/refuse-pr" ] && exit 22; cat >>"$P/prs.jsonl"; echo >>"$P/prs.jsonl"
    [ ! -e "$P/post-create-pulls.json" ] || cp "$P/post-create-pulls.json" "$P/pulls.json"
    [ ! -e "$P/post-create-detail.json" ] || cp "$P/post-create-detail.json" "$P/detail.json"
    [ ! -e "$P/post-create-checks.json" ] || cp "$P/post-create-checks.json" "$P/checks.json"
    echo '{"number":7}' ;;
  "GET https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-deploy/pulls?"*)
    page="\${url##*page=}"; page="\${page%%&*}"
    if [ -e "$P/pulls.json.\$page" ]; then cat "$P/pulls.json.\$page"; elif [ "\$page" = 1 ]; then cat "$P/pulls.json"; else echo '[]'; fi ;;
  "GET https://git.neverhuman.org/api/v1/repos/jeryu/jeryu-deploy/merge-queue?"*) cat "$P/queue.json" ;;
  "GET https://git.neverhuman.org/api/v1/repos/jeryu/jeryu-deploy/pulls?"*)
    if [ -e "$P/readback-failure" ]; then
      jq '{items: [.summary + {number:60, required_contexts:[{name:"required-context/check-readback", state:"failing"}]}], page:{has_more:false}}' "$P/detail.json"
      exit 0
    fi
    if [ -e "$P/posture-page-two" ] && [[ "\$url" == *page=1 ]]; then
      echo '{"items":[],"page":{"has_more":true}}'; exit 0
    fi
    jq -n --slurpfile d "$P/detail.json" --slurpfile c "$P/checks.json" '
      {items: [\$d[0].summary + {number: 60, required_contexts: [\$c[0].checks[] | select(.required == true)
        | {name: "required", state: (if .status == "success" then "passing"
          elif (.status | IN("failure", "error", "cancelled")) then "failing" else .status end)}]}], page: {has_more: false}}' || exit 22
    [ ! -e "$P/after-checks-detail.json" ] || cp "$P/after-checks-detail.json" "$P/detail.json"
    ;;
  "GET https://git.neverhuman.org/api/v1/repos/jeryu/jeryu-deploy/pulls/"*"/checks") cat "$P/checks.json" ;;
  "GET https://git.neverhuman.org/api/v1/repos/jeryu/jeryu-deploy/pulls/"*) cat "$P/detail.json" ;;
  "GET https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-deploy/pulls/"*)
    [ -e "$P/refuse-close-detail" ] && exit 22
    jq --argjson n "\${url##*/}" '[.[] | select(.number == \$n)][0] // {}' "$P/pulls.json" ;;
  "POST https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-deploy/issues/"*"/comments")
    [ -e "$P/refuse-comment" ] && exit 22
    body="\$(cat)"
    echo "\${url%/comments}" >>"$P/comments.log"; printf '%s\n' "\$body" >>"$P/comments.log"
    printf '%s' "\$body" | jq -c --arg url "\$url" '. + {url: \$url}' >>"$P/comments.jsonl"
    if [ -e "$P/retarget-on-comment" ]; then
      jq 'map(if .number == 193 then .base.ref = "release/stable" else . end)' "$P/pulls.json" >"$P/pulls.next"
      mv "$P/pulls.next" "$P/pulls.json"
    fi
    if [ -e "$P/rewind-main-on-comment" ]; then
      git -C "$P/deploy.git" update-ref refs/heads/main refs/heads/main^
    fi
    echo '{}' ;;
  "PATCH https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-deploy/pulls/"*)
    [ -e "$P/refuse-close" ] && exit 22
    body="\$(cat)"; echo "\${url##*/} \$body" >>"$P/closed.log"
    printf '%s' "\$body" | jq -c --arg url "\$url" '. + {url: \$url}' >>"$P/closes.jsonl"; echo '{}' ;;
  "GET https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-web/commits/"*"/status")
    jq -n --arg state "\$(cat "$P/gate")" '{state: \$state, statuses: [
      {context: "jeryu-web/required", state: \$state, updated_at: "2026-10-15T16:49:48.808799285Z"},
      {context: "jeryu-web/required", state: "pending", updated_at: "2026-10-15T16:40:22Z"},
      {context: "jeryu-web/main-health", state: \$state, updated_at: "2026-10-15T16:49:48Z"}]}' ;;
  *) echo "unexpected curl: \$method \$url" >&2; exit 22 ;;
esac
EOF
chmod +x "$P/bin/curl"
pin_real_git="$(command -v git)"
cat >"$P/bin/git" <<EOF
#!/usr/bin/env bash
case " \$* " in
  *" push "*) [ ! -e "$P/refuse-refresh-push" ] || exit 42 ;;
  *" diff-tree "*) [ ! -e "$P/fail-review-detail" ] || exit 42 ;;
esac
exec "$pin_real_git" "\$@"
EOF
chmod +x "$P/bin/git"
echo "not-a-real-pin-token" >"$P/token"; chmod 600 "$P/token"
echo '{"entries":[],"page":{"has_more":false}}' >"$P/queue.json"; echo '{"reviews":[]}' >"$P/detail.json"; : >"$P/comments.log"; : >"$P/closed.log"
: >"$P/comments.jsonl"; : >"$P/closes.jsonl"
echo '[]' >"$P/pulls.json"; echo pending >"$P/gate";: >"$P/curl-args"; : >"$P/events.jsonl"; : >"$P/prs.jsonl"; : >"$P/builds"; : >"$P/beats.jsonl"
# The bump identity and the credential path are site settings with no default,
# so every invocation names them. The identity below is this test's, not a site's.
# It starts in / as the timer starts outside any repository, so a bare `git push` fails here too.
auto_pin() ( # a subshell, so the cd stays in it
  cd / && PATH="$P/bin:$PATH" JERYU_DEPLOY_REMOTE="$P/deploy.git" JERYU_WEB_REMOTE="$P/web.git" \
    JERYU_DEPLOY_REPO=jeryu/jeryu-deploy JERYU_WEB_REPO=jeryu/jeryu-web JERYU_AUTO_PIN_STATE="${PIN_STATE:-$P/state}" \
    JERYU_PIN_GIT_NAME="${PIN_GIT_NAME-pin-bot}" JERYU_PIN_GIT_EMAIL="${PIN_GIT_EMAIL-pin-bot@forge.invalid}" \
    JERYU_PIN_TOKEN_FILE="${PIN_TOKEN_FILE-$P/token}" JERYU_BASE="${PIN_BASE:-https://git.neverhuman.org}" bash "$here/auto-pin.sh"
)
branches() { git -C "$P/deploy.git" for-each-ref --format='%(refname:short)' 'refs/heads/auto/*'; }
lines() { wc -l <"$1" | tr -d ' '; }
beat_is() { newest "$P/beats.jsonl" | jq -e "${@:2}" "$1" >/dev/null; } # FILTER [jq args]: the newest heartbeat
posted() { jq -s length "$1"; }

rc=0; PIN_BASE=https://git.neverhuman.org.evil.invalid auto_pin >"$P/run.log" 2>&1 || rc=$?
[[ $rc == 2 && ! -s "$P/curl-args" ]] || fail "auto-pin sent a request to a noncanonical origin"
chmod 644 "$P/token"; rc=0; auto_pin >"$P/run.log" 2>&1 || rc=$?; chmod 600 "$P/token"
[[ $rc == 2 && ! -s "$P/curl-args" ]] || fail "auto-pin accepted a group-readable token file"
ok "auto-pin refuses a foreign origin and a loose token file before any request"

rc=0; PIN_TOKEN_FILE= auto_pin >"$P/run.log" 2>&1 || rc=$?
[[ $rc == 2 ]] || fail "auto-pin ran without JERYU_PIN_TOKEN_FILE"
grep -q "JERYU_PIN_TOKEN_FILE is unset" "$P/run.log" || fail "the refusal does not name JERYU_PIN_TOKEN_FILE"
rc=0; PIN_GIT_NAME= auto_pin >"$P/run.log" 2>&1 || rc=$?
[[ $rc == 2 ]] || fail "auto-pin ran without a bump identity"
grep -q "JERYU_PIN_GIT_NAME and JERYU_PIN_GIT_EMAIL are unset" "$P/run.log" \
  || fail "the refusal does not name the bump identity settings"
[[ -z "$(branches)" && ! -s "$P/prs.jsonl" ]] || fail "a refused tick acted on the repository"
ok "auto-pin refuses an unset credential path or bump identity by name"

git -C "$P/web.git" update-ref -d refs/green/main # the gate has proved nothing of jeryu-web main
# A branch whose name spells the green ref matches a bare ls-remote pattern; it is not the ref.
git -C "$P/web.git" update-ref refs/heads/refs/green/main "$h1"
auto_pin >"$P/run.log" 2>&1 || fail "a tick without a green ref failed"
grep -q "has no refs/green/main yet" "$P/run.log" || { cat "$P/run.log" >&2; fail "auto-pin did not wait for the web green ref (or took the spoofing branch)"; }
git -C "$P/web.git" update-ref -d refs/heads/refs/green/main
[[ -z "$(branches)" && ! -s "$P/prs.jsonl" && ! -s "$P/builds" && ! -s "$P/events.jsonl" ]] || fail "auto-pin acted with no proved web commit"
[[ "$(posted "$P/beats.jsonl")" == 1 && "$(grep -c runners/heartbeat "$P/curl-args")" == 1 ]] || fail "a tick must post exactly one heartbeat"
beat_is '.runnerId == $id and .host == $host and .slot == 0 and .labels == ["automation"]
  and .intervalSeconds == 300 and (has("current") | not) and .last.conclusion == "waiting"
  and .last.sha == $sha and (.last | has("pr") | not)' \
  --arg id "$me/auto-pin" --arg host "$me" --arg sha "$h1" || fail "the auto-pin heartbeat is not the contract's"
ok "auto-pin waits while jeryu-web has no green ref, a branch spelling it is not it, and beats once to say so"

# The ref is not trusted alone: off jeryu-web main it is refused, and a green commit whose newest
# main-health is not success is a wait.
web_side="$(git -C "$P/web.git" -c user.name=t -c user.email=t@t commit-tree -m side "$h1^{tree}" -p "$h1")"
git -C "$P/web.git" update-ref refs/heads/side "$web_side"; git -C "$P/web.git" update-ref refs/green/main "$web_side"
rc=0; auto_pin >"$P/run.log" 2>&1 || rc=$?
[[ $rc == 1 ]] && grep -q "is not a commit of jeryu-web main; refusing" "$P/run.log" \
  || { cat "$P/run.log" >&2; fail "auto-pin did not refuse a green ref off jeryu-web main"; }
git -C "$P/web.git" update-ref -d refs/heads/side
git -C "$P/web.git" update-ref refs/green/main "$h1"
auto_pin >"$P/run.log" 2>&1 || fail "a tick with a pending main-health failed"
grep -q "has jeryu-web/main-health pending; waiting" "$P/run.log" || { cat "$P/run.log" >&2; fail "auto-pin did not wait on main-health"; }
beat_is '.last.conclusion == "waiting" and .last.sha == $sha' --arg sha "$h1" || fail "a main-health wait does not beat waiting"
[[ -z "$(branches)" && ! -s "$P/prs.jsonl" && ! -s "$P/builds" ]] || fail "auto-pin built a commit whose main-health is not green"
ok "auto-pin refuses a green ref off jeryu-web main and waits for its main-health"

# The status API now only feeds the snapshot the review body quotes; it decides nothing.
echo success >"$P/gate"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "auto-pin failed on the happy path"; }
b1="auto/pin-web-${h1:0:12}"
[[ "$(branches)" == "$b1" ]] || fail "auto-pin did not push $b1"
[[ "$(git -C "$P/deploy.git" diff --numstat main "$b1" | tr '\t' ' ')" == "2 2 jeryu-split.lock.toml" ]] || fail "the bump is not exactly two lock lines"
lock_now="$(git -C "$P/deploy.git" show "$b1:jeryu-split.lock.toml")"
grep -qx "commit = \"$h1\"" <<<"$lock_now" && grep -qx "web_dist_sha256 = \"$dist_b\"" <<<"$lock_now" || fail "the lock does not carry the new pin"
grep -q "commit = \"$(printf c%.0s {1..40})\"" <<<"$lock_now" || fail "another lock entry was edited"
[[ "$(git -C "$P/deploy.git" log -1 --format='%an <%ae>' "$b1")" == "pin-bot <pin-bot@forge.invalid>" ]] \
  || fail "the bump is not committed as the configured JERYU_PIN_GIT_NAME/EMAIL identity"
msg="$(git -C "$P/deploy.git" log -1 --format=%B "$b1")"
[[ "$msg" == "release: pin jeryu-web ${h1:0:7}"* ]] && grep -q "web change 3" <<<"$msg" && grep -q "web change 2" <<<"$msg" && ! grep -q "web change 1" <<<"$msg" || fail "the commit does not list what the bump ships"
[[ "$(posted "$P/prs.jsonl")" == 1 ]] || fail "auto-pin did not open exactly one pull request"
jq -es --arg b "$b1" --arg t "release: pin jeryu-web ${h1:0:7}" '.[0] | .head == $b and .base == "main" and .title == $t' "$P/prs.jsonl" >/dev/null || fail "the pull request is not the bump"
jq -es --arg h "$h1" '.[0].body | contains("**Evidence for review.**") and contains("Verify `\($h)` against jeryu-web main")
  and contains("ops/ci/web-pin-on-main.sh") and contains("jeryu-web status snapshot at that exact commit:")
  and contains("/jeryu-web/commit/\($h)") and contains("1501 changed paths.")
  and (contains("REVIEW_DATA_DO_NOT_COPY") | not) and (contains("web change 3") | not)
  and contains("pointer, not a gate receipt")' "$P/prs.jsonl" >/dev/null \
  || fail "the pull request body does not carry the review evidence and each shipped commit"
jq -es '.[0].body | contains("combined: success; latest contexts: jeryu-web/main-health: success, jeryu-web/required: success")
  and (contains("jeryu-web/required: pending") | not)
  and contains("does not perform this real bundle rebuild")' "$P/prs.jsonl" >/dev/null \
  || fail "the pin summary mixes historical states or overclaims the required proof"
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
git -C "$P/web.git" update-ref refs/green/main "$h2" # the gate proved the new web head
jq -n '[{number: 53, state: "open", title: "release: pin jeryu-web 427bebe (by hand)", head: {ref: "operator/pin-web"}}]' >"$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || fail "a tick behind an open bump failed"
grep -q "bump #53 is open" "$P/run.log" && [[ "$(branches)" == "$b1" && "$(lines "$P/builds")" == 1 ]] || fail "auto-pin opened a second bump beside an open one"
beat_is '.last.conclusion == "waiting" and .last.pr == 53 and .last.sha == $sha' --arg sha "$h2" || fail "a head behind an open bump does not beat as waiting for it"
since="$(newest "$P/beats.jsonl" | jq -r .last.finishedAt)"; sleep 1
auto_pin >/dev/null 2>&1 || fail "a second tick behind an open bump failed"
beat_is '.last.finishedAt == $since' --arg since "$since" || fail "a wait must keep the time it began"
ok "auto-pin never proposes twice: an existing pull request (on any page), a pushed branch, or any open bump"

# A stale bump no longer blocks a newer head. #60 is auto-pin's own bump of h1 (an ancestor of the
# head h2). It is closed with a comment, and the bump for h2 opens in the same tick, unless it is
# approved without a finished failing required gate, queued, pins a commit that is not an
# ancestor, or is the head's own bump.
cp "$P/state/opened.tsv" "$P/opened.tsv.keep"; builds_keep="$(lines "$P/builds")"
: >"$P/prs.jsonl"; : >"$P/events.jsonl"
side_sha="$(g "$P/web" commit-tree -m side "$(git -C "$P/web" rev-parse 'HEAD^{tree}')" -p "$w1")" # off main
git -C "$P/state/jeryu-web.git" fetch -q "$P/web" "$side_sha" # the cache has seen it, as it would an orphaned pin
stale_pr() { # NUMBER REF CREATED_AT
  printf "%s\t%s\t%s\n" "$(git -C "$P/state/jeryu-web.git" rev-parse "${2#auto/pin-web-}^{commit}")" "$1" "$3" >>"$P/state/opened.tsv"
  jq -n --argjson n "$1" --arg ref "$2" --arg at "$3" --arg sha "$(git -C "$P/deploy.git" rev-parse "$2" 2>/dev/null || true)" \
    '[{number: $n, state: "open", title: "release: pin jeryu-web abc1234", created_at: $at,
    head: {ref: $ref, sha: $sha, repo: {full_name: "jeryu/jeryu-deploy"}}}]' >"$P/pulls.json"
}
undo_bump() { # forget what a test tick opened for h2
  git -C "$P/deploy.git" update-ref -d "refs/heads/$b2" 2>/dev/null || true
  cp "$P/opened.tsv.keep" "$P/state/opened.tsv"; : >"$P/prs.jsonl"; : >"$P/events.jsonl"; : >"$P/comments.log"; : >"$P/closed.log"
  : >"$P/builds"; for _ in $(seq 1 "$builds_keep"); do echo build >>"$P/builds"; done
  echo '{"reviews":[]}' >"$P/detail.json"; echo '{"entries":[],"page":{"has_more":false}}' >"$P/queue.json"
  echo '{"checks":[]}' >"$P/checks.json"
}
now_iso="$(date -u +%FT%TZ)"; b2="auto/pin-web-${h2:0:12}"
approval='{"reviews":[{"state":"APPROVED","effective":true}]}'

stale_pr 60 "$b1" "$now_iso"; echo "$approval" >"$P/detail.json"
auto_pin >"$P/run.log" 2>&1 || fail "a tick behind an approved bump failed"
grep -q "bump #60 has a current approval" "$P/run.log" && grep -q "bump #60 is open" "$P/run.log" \
  && [[ ! -s "$P/closed.log" && ! -s "$P/prs.jsonl" && -z "$(git -C "$P/deploy.git" for-each-ref "refs/heads/$b2")" ]] \
  || { cat "$P/run.log" >&2; fail "auto-pin closed or replaced an approved bump"; }
undo_bump

approved_stale_pr() {
  stale_pr 60 "$b1" "$now_iso"
  jq --argjson approval "$approval" '.[0] | $approval + {summary: {
    state: .state, base_ref: "main", head_ref: .head.ref, head_sha: .head.sha}}' "$P/pulls.json" >"$P/detail.json"
}
for required_state in success pending missing running cancelled skipped unknown; do
  approved_stale_pr
  jq -n --arg status "$required_state" '{checks: [
    {required: true, status: $status}, {required: false, status: "failure"}]}' >"$P/checks.json"
  auto_pin >"$P/run.log" 2>&1 || fail "a tick behind an approved $required_state bump failed"
  [[ ! -s "$P/closed.log" && ! -s "$P/prs.jsonl" ]] || fail "approved $required_state bump was replaced"
  undo_bump
done
for required_results in '{"checks":[]}' 'unreadable' \
  '{"checks":[{"required":false,"status":"failure"}]}' \
  '{"checks":[{"required":true,"status":"failure"},{"required":true,"status":"missing"}]}' \
  '{"checks":[{"required":true,"status":"failure"},{"required":true,"status":"pending"}]}'; do
  approved_stale_pr; echo "$required_results" >"$P/checks.json"
  auto_pin >"$P/run.log" 2>&1 || fail "a tick with incomplete required evidence failed"
  [[ ! -s "$P/closed.log" && ! -s "$P/prs.jsonl" ]] || fail "incomplete required evidence discarded approval"
  undo_bump
done
failed_required='{"checks":[{"required":true,"status":"failure"},{"required":true,"status":"success"}]}'
approved_stale_pr; echo "$failed_required" >"$P/checks.json"; touch "$P/posture-page-two"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "approved failed bump blocked replacement"; }
[[ "$(posted "$P/prs.jsonl")" == 1 ]] && grep -q '^60 {"state":"closed"}$' "$P/closed.log" \
  || fail "approved failed bump was not replaced and closed"
grep -q 'requires its own review and checks' "$P/comments.log" || fail "replacement does not explain fresh review"
jq -es 'any(.[]; .kind == "pin.bump_superseded")' "$P/events.jsonl" >/dev/null || fail "replacement has no supersede event"
mv "$P/posture-page-two" "$P/posture-page-two.saved"; undo_bump

approved_stale_pr; echo "$failed_required" >"$P/checks.json"
jq '.summary.head_sha = "0000000000000000000000000000000000000000"' "$P/detail.json" >"$P/after-checks-detail.json"
auto_pin >"$P/run.log" 2>&1 || fail "a moving head failed the tick"
[[ ! -s "$P/closed.log" && ! -s "$P/prs.jsonl" ]] || fail "a different head's failure discarded approval"
mv "$P/after-checks-detail.json" "$P/after-checks-detail.saved"; undo_bump

approved_stale_pr; echo "$failed_required" >"$P/checks.json"
echo '{"checks":[{"required":true,"status":"success"}]}' >"$P/post-create-checks.json"
auto_pin >"$P/run.log" 2>&1 || fail "a recovered gate failed the tick"
[[ "$(posted "$P/prs.jsonl")" == 1 && ! -s "$P/closed.log" ]] || fail "a recovered approved bump was closed"
jq -es 'any(.[]; .kind == "pin.bump_failed" and .needs_human == true)
  and all(.[]; .kind != "pin.bump_superseded")' "$P/events.jsonl" >/dev/null || fail "recovered bump has no cleanup signal"
mv "$P/post-create-checks.json" "$P/post-create-checks.saved"; undo_bump
ok "approved stale bumps are replaced only for finished exact-head required failures, rechecked before close"
source "$here/test-pin-refresh.inc"

stale_pr 60 "$b1" "$now_iso"; echo '{"entries":[{"number":60,"state":"building"}],"page":{"has_more":false}}' >"$P/queue.json"
auto_pin >"$P/run.log" 2>&1 || fail "a tick behind a queued bump failed"
grep -q "bump #60 is in the merge queue" "$P/run.log" && [[ ! -s "$P/closed.log" && ! -s "$P/prs.jsonl" ]] \
  || { cat "$P/run.log" >&2; fail "auto-pin closed or replaced a queued bump"; }
undo_bump

stale_pr 61 "auto/pin-web-${side_sha:0:12}" "$now_iso"
auto_pin >"$P/run.log" 2>&1 || fail "a tick behind a diverged bump failed"
grep -q "does not lead to jeryu-web main" "$P/run.log" && [[ ! -s "$P/closed.log" && ! -s "$P/prs.jsonl" ]] \
  || { cat "$P/run.log" >&2; fail "auto-pin closed or replaced a diverged bump"; }
jq -es '.[0] | .kind == "pin.bump_diverged" and .pr == 61 and .needs_human == true' "$P/events.jsonl" >/dev/null || fail "a diverged bump posted no event"
undo_bump

stale_pr 62 "$b2" "$now_iso"; git -C "$P/deploy.git" update-ref "refs/heads/$b2" "$(git -C "$P/deploy.git" rev-parse main)"
auto_pin >"$P/run.log" 2>&1 || fail "a tick with the head's own bump open failed"
[[ ! -s "$P/closed.log" && ! -s "$P/prs.jsonl" && ! -s "$P/events.jsonl" && ! -s "$P/comments.log" ]] || fail "the head's own open bump was touched"
undo_bump

stale_pr 60 "$b1" "$now_iso" # no approval, not queued: superseded
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "superseding a stale bump failed the tick"; }
grep -q '^60 {"state":"closed"}$' "$P/closed.log" && grep -q "issues/60" "$P/comments.log" && grep -q "Superseded by ${h2:0:7}" "$P/comments.log" \
  || fail "the stale bump was not closed with a comment naming the new head"
[[ "$(posted "$P/prs.jsonl")" == 1 && -n "$(git -C "$P/deploy.git" for-each-ref "refs/heads/$b2")" ]] || fail "the new bump was not opened in the same tick"
jq -es --arg from "$h1" --arg to "$h2" '.[0] | .kind == "pin.bump_superseded" and .needs_human == false and .pr == 7
  and .detail.superseded_pr == 60 and .detail.superseding_pr == 7 and .detail.from == $from and .detail.to == $to' "$P/events.jsonl" >/dev/null \
  || fail "no pin.bump_superseded event naming both pull requests"
jq -es '.[1].kind == "pin.bump_opened"' "$P/events.jsonl" >/dev/null || fail "the superseding tick did not also post pin.bump_opened"
undo_bump

stale_pr 60 "$b1" "$(date -u -d '7 hours ago' +%FT%TZ)"; echo "$approval" >"$P/detail.json" # approved, but older than 6h
auto_pin >"$P/run.log" 2>&1 || fail "a tick behind an aged approved bump failed"
[[ ! -s "$P/closed.log" && ! -s "$P/prs.jsonl" ]] || fail "an aged effective approval was discarded"
undo_bump
stale_pr 60 "$b1" "$now_iso"; echo '{"reviews":[{"state":"APPROVED","effective":true},{"state":"CHANGES_REQUESTED","effective":true}]}' >"$P/detail.json"
auto_pin >"$P/run.log" 2>&1 || fail "superseding a bump with requested changes failed the tick"
[[ ! -s "$P/closed.log" && ! -s "$P/prs.jsonl" ]] || fail "a recent changes-requested review was superseded"
undo_bump
green_keep="$(git -C "$P/web.git" rev-parse refs/green/main)"
git -C "$P/web.git" update-ref -d refs/green/main; stale_pr 60 "$b1" "$now_iso"
auto_pin >"$P/run.log" 2>&1 || fail "a tick with no green ref failed"
[[ ! -s "$P/closed.log" && ! -s "$P/prs.jsonl" ]] || fail "a bump was closed with no proved commit to replace it"
git -C "$P/web.git" update-ref refs/green/main "$green_keep"; undo_bump
ok "auto-pin supersedes a stale unqueued bump in one tick, and leaves approved, queued, diverged and same-head bumps alone"

# Building or opening a replacement must never retire the existing review.
stale_pr 60 "$b1" "$now_iso"; touch "$P/fail"
if auto_pin >"$P/run.log" 2>&1; then fail "a failed replacement build reported success"; fi
[[ ! -s "$P/closed.log" && ! -s "$P/comments.log" ]] || fail "a failed build closed or commented on the old bump"
mv "$P/fail" "$P/fail.saved"; mv "$P/state/failures/$h2" "$P/state/failures/$h2-build.saved"; undo_bump
stale_pr 60 "$b1" "$now_iso"; touch "$P/refuse-pr"
if auto_pin >"$P/run.log" 2>&1; then fail "a refused replacement PR reported success"; fi
[[ ! -s "$P/closed.log" && ! -s "$P/comments.log" ]] || fail "a refused create closed or commented on the old bump"
mv "$P/refuse-pr" "$P/refuse-pr.saved"; mv "$P/state/failures/$h2" "$P/state/failures/$h2-create.saved"; undo_bump

stale_pr 60 "$b1" "$now_iso"; touch "$P/refuse-close"
auto_pin >"$P/run.log" 2>&1 || fail "a refused old-bump close lost its replacement"
[[ "$(posted "$P/prs.jsonl")" == 1 && ! -s "$P/comments.log" ]] || fail "a refused close posted a superseding comment"
jq -es 'any(.[]; .kind == "pin.bump_failed" and .needs_human == true)' "$P/events.jsonl" >/dev/null || fail "a refused close did not ask for cleanup"
mv "$P/refuse-close" "$P/refuse-close.saved"; undo_bump

stale_pr 60 "$b1" "$now_iso"
jq '. + [.[0] | .number = 61 | .head.ref = "operator/pin-web"]' "$P/pulls.json" >"$P/pulls.multiple.json"
cp "$P/pulls.multiple.json" "$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || fail "multiple existing bumps failed the tick"
[[ ! -s "$P/prs.jsonl" && ! -s "$P/closed.log" ]] || fail "multiple existing bumps were replaced"
undo_bump
stale_pr 60 "$b1" "$now_iso"; cp "$P/opened.tsv.keep" "$P/state/opened.tsv"
auto_pin >"$P/run.log" 2>&1 || fail "an unrecorded bump failed the tick"
[[ ! -s "$P/prs.jsonl" && ! -s "$P/closed.log" ]] || fail "an unrecorded bump was replaced"
undo_bump
ok "replacement failures preserve the old review; refused closes, multiple bumps and unknown provenance fail safely"

stale_pr 60 "$b1" "$now_iso"
jq '. + [.[0] | .number = 61 | .head.ref = "operator/pin-web"]' "$P/pulls.json" >"$P/post-build-pulls.json"
auto_pin >"$P/run.log" 2>&1 || fail "another bump arriving during the build failed the tick"
[[ ! -s "$P/prs.jsonl" && ! -s "$P/closed.log" ]] || fail "the pre-create list used cached bump state"
mv "$P/post-build-pulls.json" "$P/post-build-pulls.saved"; undo_bump
stale_pr 60 "$b1" "$now_iso"
jq '.[0].state = "closed"' "$P/pulls.json" >"$P/post-create-pulls.json"
auto_pin >"$P/run.log" 2>&1 || fail "a manual close during replacement failed the tick"
[[ "$(posted "$P/prs.jsonl")" == 1 && ! -s "$P/closed.log" && ! -s "$P/comments.log" ]] \
  || { cat "$P/run.log" "$P/closed.log" "$P/comments.log" >&2; fail "a manual close did not preserve the old review and replacement"; }
jq -es 'any(.[]; .kind == "pin.bump_failed" and .needs_human == true)
  and all(.[]; .kind != "pin.bump_superseded")' "$P/events.jsonl" >/dev/null \
  || fail "a changed close-state was reported as superseded"
mv "$P/post-create-pulls.json" "$P/post-create-pulls.saved"; undo_bump
stale_pr 60 "$b1" "$now_iso"; echo "$approval" >"$P/post-create-detail.json"
auto_pin >"$P/run.log" 2>&1 || fail "an approval arriving during replacement failed the tick"
[[ "$(posted "$P/prs.jsonl")" == 1 && ! -s "$P/closed.log" && ! -s "$P/comments.log" ]] \
  || fail "an approval arriving during the build was discarded"
jq -es 'any(.[]; .kind == "pin.bump_failed" and .needs_human == true)
  and all(.[]; .kind != "pin.bump_superseded")' "$P/events.jsonl" >/dev/null \
  || fail "a preserved approval did not produce the cleanup signal"
mv "$P/post-create-detail.json" "$P/post-create-detail.saved"; undo_bump
ok "fresh lists detect a new bump, preserve manual closes and approvals, and signal replacement cleanup"

echo '[]' >"$P/pulls.json"; touch "$P/fail-review-detail"; : >"$P/events.jsonl"
if PIN_STATE="$P/state-evidence-failure" auto_pin >"$P/run.log" 2>&1; then
  fail "failed evidence generation reported success"
fi
[[ "$(cat "$P/state-evidence-failure/failures/$h2")" == 1 ]] || fail "evidence failure was not counted"
grep -q "could not prepare shipped commit review pointers" "$P/run.log" || fail "evidence failure lost its reason"
jq -es '.[0] | .kind == "pin.bump_failed" and .needs_human == false' "$P/events.jsonl" >/dev/null || fail "evidence failure lost its event"
: >"$P/events.jsonl"
if PIN_STATE="$P/state-evidence-failure" auto_pin >"$P/run.log" 2>&1; then fail "repeated evidence failure passed"; fi
jq -es '.[0] | .kind == "pin.bump_failed" and .needs_human == true' "$P/events.jsonl" >/dev/null || fail "evidence failures never ask for a human"
rm -f "$P/fail-review-detail"
ok "auto-pin counts evidence-generation failures and reaches its human backstop"

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
[[ ! -s "$P/curl-args" ]] || fail "JERYU_AUTO_PIN_BEAT=0 still sent a request"
grep -q "already pinned on main; nothing to do" "$P/run.log" || fail "up-to-date skip was not logged"
PIN_STATE="$P/state-fresh" auto_pin >"$P/run.log" 2>&1 || fail "a first-ever tick failed"
beat_is '.runnerId == $id and (has("last") | not)' --arg id "$me/auto-pin" || fail "a timer with no history must beat without a last"
: >"$P/curl-args"; builds="$(lines "$P/builds")"
auto_pin >"$P/run.log" 2>&1 || fail "an up-to-date tick failed"
[[ "$(cat "$P/curl-args")" == *"/api/v1/runners/heartbeat" && "$(lines "$P/curl-args")" == 1 && "$(lines "$P/builds")" == "$builds" ]] || fail "an up-to-date pin made extra requests or built"
grep -q "already pinned on main; nothing to do" "$P/run.log" || fail "up-to-date skip was silent"
ok "auto-pin logs why it skips once the pin is the web head"

# Closed bumps leave their branches behind; a tick deletes them. What stays: a branch that moved
# past its closed pull request's head, one an open pull request uses (also when an older closed
# pull request names it at the same commit), one only a fork's closed pull request names, and the
# current head's.
s1="$(git -C "$P/deploy.git" rev-parse "$b1")"; s2="$(git -C "$P/deploy.git" rev-parse "$b2")"
moved="auto/pin-web-$(printf 1%.0s {1..12})"; open_b="auto/pin-web-$(printf 2%.0s {1..12})"
reopened="auto/pin-web-$(printf 3%.0s {1..12})"; forked="auto/pin-web-$(printf 4%.0s {1..12})"
for b in "$moved" "$open_b" "$reopened" "$forked"; do git -C "$P/deploy.git" update-ref "refs/heads/$b" "$s2"; done
jq -n --arg b1 "$b1" --arg s1 "$s1" --arg b2 "$b2" --arg s2 "$s2" --arg mv "$moved" --arg ob "$open_b" \
  --arg ro "$reopened" --arg fk "$forked" '
  def row($n; $state; $ref; $sha; $repo): {number: $n, state: $state, title: "x", head: {ref: $ref, sha: $sha, repo: {full_name: $repo}}};
  [row(7; "closed"; $b1; $s1; "jeryu/jeryu-deploy"),
   row(8; "closed"; $mv; $s1; "jeryu/jeryu-deploy"),
   row(9; "open"; $ob; $s2; "jeryu/jeryu-deploy"),
   row(10; "closed"; $b2; $s2; "jeryu/jeryu-deploy"),
   row(11; "closed"; $ro; $s2; "jeryu/jeryu-deploy"),
   row(12; "open"; $ro; $s2; "jeryu/jeryu-deploy"),
   row(13; "closed"; $fk; $s2; "someone/jeryu-deploy")]' >"$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "a pruning tick failed"; }
grep -qx "\[auto-pin [0-9:]*\] deleted $b1 (#7 is closed)" "$P/run.log" || { cat "$P/run.log" >&2; fail "auto-pin did not delete the closed bump's branch"; }
[[ "$(branches | sort)" == "$(printf '%s\n' "$moved" "$open_b" "$reopened" "$forked" "$b2" | sort)" ]] \
  || fail "auto-pin deleted a branch that must stay: $(branches | tr '\n' ' ')"
: >"$P/curl-args"
auto_pin >"$P/run.log" 2>&1 || fail "an idle tick after pruning failed"
[[ "$(lines "$P/curl-args")" == 1 ]] || fail "an idle tick listed the pull requests again with no new bump branch"
grep -q "already pinned on main; nothing to do" "$P/run.log" || fail "idle skip after pruning was silent"
# A refused delete logs git's reason, never fails the tick, and is tried (and logged) again next tick.
# The stand-in git refuses only the delete, as a forge that protects the branch would.
git -C "$P/deploy.git" update-ref "refs/heads/$b1" "$s1"
real_git="$(command -v git)"
printf '#!/usr/bin/env bash\n[[ " $* " == *" push "*" --delete "* ]] && { echo "remote: deleting is not allowed" >&2; exit 1; }\nexec "%s" "$@"\n' "$real_git" >"$P/bin/git"
chmod +x "$P/bin/git"
for tick in 1 2; do
  auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "a refused delete failed the tick"; }
  grep -q "could not delete $b1 (#7 is closed; ignored): remote: deleting is not allowed" "$P/run.log" \
    && git -C "$P/deploy.git" rev-parse -q --verify "refs/heads/$b1" >/dev/null || fail "a refused delete was not logged with its reason on tick $tick"
done
rm "$P/bin/git"
auto_pin >"$P/run.log" 2>&1 || fail "the tick after a refusal failed"
grep -q "deleted $b1 (#7 is closed)" "$P/run.log" || fail "a delete the forge refused once was not retried"
ok "auto-pin deletes the branches of closed bumps, keeps moved, open, fork-named and current ones, and logs and retries a refused delete"

# A web commit that changes no shipped file (tests, docs) builds the bundle that is already pinned.
echo "a test-only change" >"$P/web/NOTES.md"; g "$P/web" add NOTES.md; g "$P/web" commit -q -m "web tests only"; g "$P/web" push -q "$P/web.git" main
h3="$(git -C "$P/web" rev-parse HEAD)"; b3="auto/pin-web-${h3:0:12}"
git -C "$P/web.git" update-ref refs/green/main "$h3" # the gate proved it too
echo '[]' >"$P/pulls.json"; : >"$P/events.jsonl"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "auto-pin refused a head whose bundle is the pinned one"; }
lock_now="$(git -C "$P/deploy.git" show "$b3:jeryu-split.lock.toml")"
grep -qx "commit = \"$h3\"" <<<"$lock_now" && grep -qx "web_dist_sha256 = \"$dist_b\"" <<<"$lock_now" || fail "the same-bundle bump does not carry the new commit and the unchanged hash"
[[ "$(git -C "$P/deploy.git" diff --numstat main "$b3" | tr '\t' ' ')" == "1 1 jeryu-split.lock.toml" ]] || fail "a same-bundle bump must change the commit line only"
git -C "$P/deploy.git" log -1 --format=%B "$b3" | grep -q "byte-identical" || fail "the same-bundle bump does not say the bundle is unchanged"
jq -es '.[0] | .kind == "pin.bump_opened"' "$P/events.jsonl" >/dev/null || fail "the same-bundle bump posted no event"
ok "auto-pin pins a head whose bundle is unchanged by moving the commit only"

# A bump whose change reached main by another route has nothing left to land: #184 and #193 stayed
# open in that state, and #193 was gated anyway, hit a flaky test and asked for a human about a
# change that had already shipped. Such a pull request is closed, with a comment naming the commit
# its work landed as; the branch goes on a later tick's prune.
echo '[]' >"$P/pulls.json"; : >"$P/comments.jsonl"; : >"$P/closes.jsonl"
landed_sha="$(git -C "$P/deploy.git" rev-parse "refs/heads/$b3")"
# main takes the same change as another commit: a commit of its own first, then the bump's patch.
g "$P/deploy" fetch -q "$P/deploy.git" "+refs/heads/main:refs/remotes/d/main" "+refs/heads/$b3:refs/remotes/d/b3"
g "$P/deploy" checkout -q -B landing refs/remotes/d/main
echo "a change of main's own" >"$P/deploy/NOTES.md"; g "$P/deploy" add NOTES.md
g "$P/deploy" commit -q -m "main moves first"
g "$P/deploy" cherry-pick refs/remotes/d/b3 >/dev/null
g "$P/deploy" push -q "$P/deploy.git" HEAD:refs/heads/main
landing="$(git -C "$P/deploy.git" rev-parse refs/heads/main)"
[[ "$landing" != "$landed_sha" ]] || fail "the fixture did not land the bump as another commit"
printf '%s\t%s\t%s\n' "$h3" 193 "$now_iso" >>"$P/state/opened.tsv"
jq -n --arg b "$b3" --arg sha "$landed_sha" '
  [{number: 193, state: "open", base: {ref: "main", repo: {full_name: "jeryu/jeryu-deploy"}}, title: "release: pin jeryu-web x", head: {ref: $b, sha: $sha, repo: {full_name: "jeryu/jeryu-deploy"}}},
   {number: 194, state: "open", base: {ref: "main", repo: {full_name: "jeryu/jeryu-deploy"}}, title: "something else", head: {ref: "work/other", sha: $sha, repo: {full_name: "jeryu/jeryu-deploy"}}}]' >"$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "a tick that closes a landed bump failed"; }
grep -q "closed #193 ($b3): already on main as ${landing:0:12}" "$P/run.log" \
  || { cat "$P/run.log" >&2; fail "auto-pin did not close the bump whose change is on main"; }
jq -es --arg landing "$landing" '.[0].url == "https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-deploy/issues/193/comments"
  and (.[0].body | contains($landing)) and length == 1' "$P/comments.jsonl" >/dev/null \
  || { cat "$P/comments.jsonl" >&2; fail "the close did not comment once, naming the landing commit"; }
jq -es '.[0].url == "https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-deploy/pulls/193"
  and .[0].state == "closed" and length == 1' "$P/closes.jsonl" >/dev/null \
  || { cat "$P/closes.jsonl" >&2; fail "exactly the landed bump must be closed"; }
ok "auto-pin closes its own bump whose change is already on main, naming the landing commit"

# Main's copy of the change says nothing about a PR retargeted to a release
# branch. It remains owned, and its source is unchanged, but it must stay open.
: >"$P/comments.jsonl"; : >"$P/closes.jsonl"
jq -n --arg b "$b3" --arg sha "$landed_sha" '
  [{number: 193, state: "open", title: "release: pin jeryu-web x",
    base: {ref: "release/stable", repo: {full_name: "jeryu/jeryu-deploy"}},
    head: {ref: $b, sha: $sha, repo: {full_name: "jeryu/jeryu-deploy"}}}]' >"$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || fail "a tick with a retargeted landed bump failed"
[[ ! -s "$P/closes.jsonl" && ! -s "$P/comments.jsonl" ]] \
  || fail "main's landed change closed a bump targeting a release branch"
ok "auto-pin leaves a bump targeting a release branch open"

# The remote source remains fixed while the comment changes the PR's target.
# The fresh PR read must invalidate the earlier main-targeted eligibility.
: >"$P/comments.jsonl"; : >"$P/closes.jsonl"
jq -n --arg b "$b3" --arg sha "$landed_sha" '
  [{number: 193, state: "open", title: "release: pin jeryu-web x",
    base: {ref: "main", repo: {full_name: "jeryu/jeryu-deploy"}},
    head: {ref: $b, sha: $sha, repo: {full_name: "jeryu/jeryu-deploy"}}}]' >"$P/pulls.json"
touch "$P/retarget-on-comment"
auto_pin >"$P/run.log" 2>&1 || fail "a tick with a late retarget failed"
rm "$P/retarget-on-comment"
[[ -s "$P/comments.jsonl" && ! -s "$P/closes.jsonl" ]] \
  || fail "a late retarget did not prevent the close"
ok "auto-pin rechecks the PR target after the comment before closing"

# An unreadable current PR is not permission to close it from an older list.
: >"$P/comments.jsonl"; : >"$P/closes.jsonl"
jq 'map(.base.ref = "main")' "$P/pulls.json" >"$P/pulls.next"
mv "$P/pulls.next" "$P/pulls.json"
touch "$P/refuse-close-detail"
auto_pin >"$P/run.log" 2>&1 || fail "a refused current PR read failed the tick"
rm "$P/refuse-close-detail"
[[ ! -s "$P/closes.jsonl" ]] || fail "an unreadable current PR was closed"
ok "auto-pin keeps the PR open when its current identity cannot be read"

# A main rewind can remove the supposedly landed patch while the source and
# PR identity stay unchanged. Do not apply the result from the earlier main.
: >"$P/comments.jsonl"; : >"$P/closes.jsonl"
touch "$P/rewind-main-on-comment"
auto_pin >"$P/run.log" 2>&1 || fail "a main movement failed the tick"
rm "$P/rewind-main-on-comment"
[[ -s "$P/comments.jsonl" && ! -s "$P/closes.jsonl" ]] \
  || fail "a main movement did not invalidate the earlier landing result"
git -C "$P/deploy.git" update-ref refs/heads/main "$landing"
ok "auto-pin leaves the bump open when main changes after the landing proof"

# A manually opened pull request on the same branch has no timer ownership.
: >"$P/comments.jsonl"; : >"$P/closes.jsonl"
jq -n --arg b "$b3" --arg sha "$landed_sha" '
  [{number: 195, state: "open", base: {ref: "main", repo: {full_name: "jeryu/jeryu-deploy"}}, title: "release: pin jeryu-web x", head: {ref: $b, sha: $sha, repo: {full_name: "jeryu/jeryu-deploy"}}}]' >"$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || fail "a tick with an unowned landed bump failed"
[[ ! -s "$P/closes.jsonl" && ! -s "$P/comments.jsonl" ]] || fail "an unowned pull request was touched"
grep -q "bump #195 has no auto-pin ownership record" "$P/run.log" || fail "missing ownership was not explained"
ok "auto-pin never closes an unowned pull request even on its own branch"

# A bump with something to land is left alone, and so is one whose comment the forge refuses: a
# close nobody can read the reason for is worse than an open pull request.
: >"$P/comments.jsonl"; : >"$P/closes.jsonl"
# $b1 is an older pin the prune pass above deleted: put it back, so it is a candidate git has to
# answer about rather than a branch the close pass never sees.
git -C "$P/deploy.git" update-ref "refs/heads/$b1" "$s1"
jq -n --arg b "$b1" --arg sha "$s1" '
  [{number: 7, state: "open", base: {ref: "main", repo: {full_name: "jeryu/jeryu-deploy"}}, title: "release: pin jeryu-web old", head: {ref: $b, sha: $sha, repo: {full_name: "jeryu/jeryu-deploy"}}}]' >"$P/pulls.json"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "a tick with an unlanded bump open failed"; }
[[ ! -s "$P/closes.jsonl" && ! -s "$P/comments.jsonl" ]] || fail "a bump with something to land was closed"
jq -n --arg b "$b3" --arg sha "$landed_sha" '
  [{number: 193, state: "open", base: {ref: "main", repo: {full_name: "jeryu/jeryu-deploy"}}, title: "release: pin jeryu-web x", head: {ref: $b, sha: $sha, repo: {full_name: "jeryu/jeryu-deploy"}}}]' >"$P/pulls.json"
git -C "$P/deploy.git" update-ref "refs/heads/$b3" "$landed_sha"
touch "$P/refuse-comment"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "a refused comment failed the tick"; }
grep -q "could not comment on #193 before closing it (ignored); leaving it open" "$P/run.log" \
  || { cat "$P/run.log" >&2; fail "a refused comment was not logged with its consequence"; }
[[ ! -s "$P/closes.jsonl" ]] || fail "a pull request was closed without its reason"
rm "$P/refuse-comment"
# A refused close is logged and tried again on the next tick, never fatal.
touch "$P/refuse-close"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "a refused close failed the tick"; }
grep -q "could not close #193 ($b3; ignored)" "$P/run.log" \
  || { cat "$P/run.log" >&2; fail "a refused close was not logged with its reason"; }
rm "$P/refuse-close"
# A tick with no landed bump asks the forge nothing extra: which branches are candidates is git's
# answer, so the pull requests are listed only when there is one.
git -C "$P/deploy.git" update-ref -d "refs/heads/$b3"
jq -n '[]' >"$P/pulls.json"
: >"$P/curl-args"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "an idle tick after the close pass failed"; }
[[ "$(lines "$P/curl-args")" == 1 ]] \
  || { cat "$P/curl-args" >&2; fail "a tick with no landed bump listed the pull requests"; }
ok "auto-pin leaves an unlanded bump alone, keeps one it cannot explain open, and asks nothing extra when there is none"

# --- deploy-release.sh: keep switch.sh's output, and say in the failure status why it failed ---
# Stand-ins: ssh that answers the staged metadata and runs a switch that prints and exits 3 (or 0),
# curl that records every posted body. The operator's home is a throwaway directory.
D="$T/deploy"; mkdir -p "$D/bin" "$D/home"
D_REL=prod-20260920T135912Z-4f7d883-unsigned
# The two hosts are site settings with no default; the stand-in ssh ignores which
# host it was given, so these names are this test's.
export JERYU_BUILD_HOST=build.invalid JERYU_FORGE_HOST=forge.invalid
export JERYU_HEALTH_URL=http://forge.invalid:8787/health
cat >"$D/bin/ssh" <<EOF
#!/usr/bin/env bash
case "\$*" in
  *RELEASE.txt*) printf 'jeryu_deploy_commit=%s x\nrollback_target=%s\nbinary_sha256=%s\nlive=jeryu-%s\nlive_commit=%s\n' \
    "\$(cat "$D/staged_commit")" "$PREV" "$(printf e%.0s {1..64})" "\$(cat "$D/live")" "\$(cat "$D/live_commit")" ;;
  *)
    printf '%s\n' "\$*" >>"$D/switch-args"
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
grep -qF "'JERYU_HEALTH_URL=http://forge.invalid:8787/health bash ~/.jeryu/incoming/$D_REL/switch.sh'" "$D/switch-args" \
  || fail "deploy-release did not hand JERYU_HEALTH_URL to switch.sh on the forge host: $(cat "$D/switch-args")"
rc=0; out="$(JERYU_HEALTH_URL='' deploy_release 2>&1)" || rc=$?
[[ $rc -ne 0 && "$out" == *"JERYU_HEALTH_URL is unset"* ]] || fail "deploy-release ran without JERYU_HEALTH_URL (rc $rc): $out"
rc=0; out="$(JERYU_HEALTH_URL="http://x/'; touch /tmp/pwn" deploy_release 2>&1)" || rc=$?
[[ $rc -ne 0 && "$out" == *"must be a plain http(s) URL"* ]] || fail "deploy-release took a health URL that could break the remote command (rc $rc)"
ok "deploy-release hands JERYU_HEALTH_URL to switch.sh and refuses a missing or unsafe one"

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

# --- verify-pin-bump.sh: the default-off fast lane admits exactly a verified mechanical pin bump ---
# Stand-ins: a local bare jeryu-web, a jeryu-deploy whose main carries a stub builder that prints
# the hash in $V/dist, and a curl answering the status API from $V/status.
V="$T/fast"; mkdir -p "$V/bin" "$V/web" "$V/deploy/scripts/release"
g "$V/web" init -q -b main
for n in 1 2 3; do echo "$n" >"$V/web/index.html"; g "$V/web" add .; g "$V/web" commit -q -m "web $n"; done
git clone -q --bare "$V/web" "$V/web.git"
f_old="$(git -C "$V/web" rev-parse HEAD~1)"; f_new="$(git -C "$V/web" rev-parse HEAD)"
f_hash_a="$(printf a%.0s {1..64})"; f_hash_b="$(printf b%.0s {1..64})"
write_lock() { # COMMIT HASH [CORE_COMMIT]
  cat <<LOCK
web_artifact = "pinned"

[[repo]]
name = "jeryu-core"
commit = "${3:-$(printf c%.0s {1..40})}"

[[repo]]
name = "jeryu-web"
commit = "$1"
web_dist_sha256 = "$2"
required_check = "jeryu-web/required"
LOCK
}
write_lock "$f_old" "$f_hash_a" >"$V/deploy/jeryu-split.lock.toml"
cat >"$V/deploy/scripts/release/build-web-dist.sh" <<STUB
#!/usr/bin/env bash
echo "\$2 \$(cat "$V/dist")"
STUB
chmod +x "$V/deploy/scripts/release/build-web-dist.sh"
echo base >"$V/deploy/other.txt"
g "$V/deploy" init -q -b main; g "$V/deploy" add .; g "$V/deploy" commit -q -m base
f_base="$(git -C "$V/deploy" rev-parse HEAD)"
g "$V/deploy" update-ref refs/remotes/origin/main "$f_base"
cat >"$V/bin/curl" <<STUB
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$V/curl-args"
case "\${!#}" in
  https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-web/commits/*/status) cat "$V/status" ;;
  *) echo "unexpected curl: \${!#}" >&2; exit 22 ;;
esac
STUB
chmod +x "$V/bin/curl"
echo "not-a-real-token" >"$V/token"; chmod 600 "$V/token"
fast_ok='{"state":"success","statuses":[{"context":"jeryu-web/required","state":"success"}]}'
bump() { # COMMIT HASH [CORE_COMMIT]: HEAD = base plus a lock edit, as a commit on a throwaway branch
  g "$V/deploy" checkout -q -B bump "$f_base"
  write_lock "$1" "$2" "${3:-}" >"$V/deploy/jeryu-split.lock.toml"
  g "$V/deploy" commit -q -am bump
}
verify() { # [--json]: sets rc and out
  rc=0; out="$(cd "$V/deploy" && env PATH="$V/bin:$PATH" JERYU_PIN_FAST_LANE="${FAST-1}" JERYU_WEB_REMOTE="$V/web.git" \
    JERYU_WEB_REPO=jeryu/jeryu-web JERYU_PIN_TOKEN_FILE="$V/token" ${VERIFY_ENV:-} bash "$here/verify-pin-bump.sh" "$@" "$f_base" HEAD 2>/dev/null)" || rc=$?
}
refused() { # WHAT substring
  [[ $rc == 10 && "$out" == "normal-path: "*"$2"* ]] || fail "$1 (rc $rc): $out"
}
echo "$f_hash_b" >"$V/dist"; echo "$fast_ok" >"$V/status"
bump "$f_new" "$f_hash_b"
FAST=0 verify; refused "the lane is not off by default" "is off"
FAST="" verify; refused "an empty switch turned the lane on" "is off"
: >"$V/curl-args"; FAST=0 verify; [[ ! -s "$V/curl-args" ]] || fail "an off lane reached the forge"
verify
[[ $rc == 0 && "$out" == "eligible $f_new $f_hash_b" ]] || fail "a verified bump is not eligible (rc $rc): $out"
verify --json
jq -e --arg c "$f_new" --arg h "$f_hash_b" '.eligible == true and .commit == $c and .web_dist_sha256 == $h' <<<"$out" >/dev/null \
  || fail "--json verdict is not the contract's: $out"
ok "verify-pin-bump is off by default and passes a verified bump"

protected_base="$f_base"; f_base="$(git -C "$V/deploy" rev-parse HEAD)"
verify; refused "an unprotected BASE was accepted" "not an ancestor of protected"
f_base="$protected_base"
ok "verify-pin-bump refuses a caller-chosen BASE outside protected origin/main"

echo "$f_hash_a" >"$V/dist"; bump "$f_new" "$f_hash_a"
verify; [[ $rc == 0 ]] || fail "a bump whose bundle is the pinned one (commit moves only) is not eligible: $out"
echo "$f_hash_b" >"$V/dist"
bump "$f_new" "$f_hash_b"; echo changed >"$V/deploy/other.txt"; g "$V/deploy" commit -q -am more
verify; refused "a second file was accepted" "more than one file"
bump "$f_new" "$f_hash_b" "$(printf d%.0s {1..40})"
verify; refused "another lock line was accepted" "more than the jeryu-web pin"
bump "$f_new" "$f_hash_b"; echo '# note' >>"$V/deploy/jeryu-split.lock.toml"; g "$V/deploy" commit -q -am note
verify; refused "an appended line was accepted" "more than the jeryu-web pin"
bump "$f_old" "$f_hash_b"; verify; refused "an unmoved commit was accepted" "does not move"
bump "$(printf e%.0s {1..40})" "$f_hash_b"; verify; refused "a commit that is not on jeryu-web was accepted" "not a jeryu-web commit"
bump "$f_new" "not-a-hash"; verify; refused "a malformed hash was accepted" "not 64-hex"
ok "verify-pin-bump sends anything but the two pin lines, or an unknown or unmoved commit, to the normal path"

bump "$f_new" "$f_hash_b"
echo "$f_hash_a" >"$V/dist"; verify; refused "a hash the build does not reproduce was accepted" "hashes to"
echo "$f_hash_b" >"$V/dist"
echo '{"state":"pending","statuses":[]}' >"$V/status"; verify; refused "a pending web gate was accepted" "is not success"
echo '{"state":"success","statuses":[{"context":"other","state":"success"}]}' >"$V/status"
verify; refused "a green status without the required check was accepted" "is not success"
echo 'not json' >"$V/status"; verify; refused "an unreadable status was accepted" "is not success"
echo "$fast_ok" >"$V/status"
VERIFY_ENV="JERYU_BASE=https://example.invalid" verify; refused "a foreign credential origin was accepted" "noncanonical"
chmod 644 "$V/token"; verify; refused "a loose token file was accepted" "0600"; chmod 600 "$V/token"
verify; [[ $rc == 0 ]] || fail "the verified bump stopped being eligible: $out"
ok "verify-pin-bump sends an unreproduced hash, a web gate that is not green, a foreign origin or a loose token to the normal path"

# -h and --help on every release script print its header and exit 0 without running anything:
# the network, git, docker, service and file tools on PATH are tripwires.
# --- release-action.sh: the steps a deployer asks for, by hand or through the forge's queue ---
# Stand-ins: a local bare repo whose deploy-release.sh records how it was called, a release.env the
# script must read by itself, and curl that plays the forge's release-actions routes. The forge
# never sends a command: each ask is checked again and mapped to a fixed step.
R="$T/ra"; mkdir -p "$R/bin" "$R/src/scripts/release" "$R/stage"
cat >"$R/src/scripts/release/deploy-release.sh" <<EOF
#!/usr/bin/env bash
echo "DEPLOYED \$* build=\${JERYU_BUILD_HOST:-unset} git=\${JERYU_DEPLOY_GIT:+set}"
[ -e "$R/deploy-fails" ] && { echo "switch.sh exited 3" >&2; exit 3; }
exit 0
EOF
chmod +x "$R/src/scripts/release/deploy-release.sh"
git -C "$R/src" init -q -b main
git -C "$R/src" -c user.name=t -c user.email=t@t add .
git -C "$R/src" -c user.name=t -c user.email=t@t commit -q -m deploy
git clone -q --bare "$R/src" "$R/remote.git"
printf 'JERYU_BUILD_HOST=build-1\nJERYU_DEPLOY_TOKEN_FILE=%s\n' "$R/token" >"$R/release.env"
echo "not-a-real-token" >"$R/token"
cat >"$R/bin/curl" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$R/curl-args"
case "\$*" in
  *"/api/v1/release-actions/poller"*) echo '{}' ;;
  *"release-actions?state=pending"*) cat "$R/pending.json" ;;
  *"/claim"*) [ -e "$R/claimed" ] && { echo '{"code":"already_claimed"}'; exit 22; }; touch "$R/claimed"; echo '{}' ;;
  *"/result"*) prev=""; for a in "\$@"; do [ "\$prev" = --data-binary ] && printf '%s\n' "\$a" >>"$R/results.jsonl"; prev="\$a"; done; echo 200 ;;
  *) echo '{}' ;;
esac
EOF
chmod +x "$R/bin/curl"
action() {
  # Settings already in the environment win over release.env, so clear the ones the file must supply.
  env -u JERYU_BUILD_HOST -u JERYU_DEPLOY_TOKEN_FILE \
    PATH="$R/bin:$PATH" JERYU_RELEASE_ENV="$R/release.env" JERYU_DEPLOY_REMOTE="$R/remote.git" \
    JERYU_DEPLOY_REPO=acme/forge-deploy JERYU_RELEASE_ACTIONS_STATE="$R/state" \
    JERYU_AUTO_STAGE_STATE="$R/stage" JERYU_FORGE_URL="https://forge.invalid" \
    bash "$here/release-action.sh" "$@"
}
rel="prod-20261012T171717Z-$(git -C "$R/src" rev-parse --short=12 HEAD)-unsigned"

for bad in "prod-1; reboot" "../../etc/passwd" "prod-20261012T171717Z-F1B1FB4-unsigned"; do
  rc=0; action deploy "$bad" >/dev/null 2>&1 || rc=$?
  [[ $rc == 64 ]] || fail "release-action deployed a malformed release id '$bad' (exit $rc)"
done
rc=0; action reboot >/dev/null 2>&1 || rc=$?
[[ $rc == 64 ]] || fail "release-action ran an unknown step (exit $rc)"
ok "release-action refuses a malformed release id and an unknown step"

out="$(action deploy "$rel" --dry-run 2>&1)" || fail "a dry-run deploy failed: $out"
grep -q "DEPLOYED --dry-run $rel build=build-1 git=set" <<<"$out" \
  || fail "deploy did not run main's deploy-release.sh with release.env and the history cache: $out"
ok "release-action deploy runs the staged commit's script from any directory with release.env loaded"

# Advancing main must not change the script the already staged release executes.
printf '\necho UNEXPECTED_MAIN_SCRIPT\n' >>"$R/src/scripts/release/deploy-release.sh"
git -C "$R/src" add .
git -C "$R/src" -c user.name=t -c user.email=t@t commit -q -m newer
# The local bare stand-in is the transport source, not an external publication.
git -C "$R/src" push -q "$R/remote.git" main
out="$(action deploy "$rel" --dry-run 2>&1)" || fail "pinned script failed after main advanced: $out"
[[ "$out" != *UNEXPECTED_MAIN_SCRIPT* ]] || fail "staged release executed an unrelated newer script"
ok "release-action pins execution to the staged commit after main advances"


# The forge asks for another repository's release: not this host's to run, never claimed.
jq -n --arg rel "$rel" '{actions: [{id: "ra-20261012T180000Z-1", action: "deploy", repo: "acme/other",
  release: $rel, requested_by: "operator", requested_at: "2026-10-12T18:00:00Z"}]}' >"$R/pending.json"
action poll >/dev/null 2>&1 || fail "poll failed on another repository's ask"
! grep -q "/claim" "$R/curl-args" || fail "poll claimed an ask for another repository"
grep -q "/api/v1/release-actions/poller" "$R/curl-args" || fail "poll sent no heartbeat"
ok "release-action poll beats, and leaves an ask for another repository alone"

jq -n --arg rel "$rel" '{actions: [{id: "ra-20261012T180500Z-2", action: "deploy", repo: "acme/forge-deploy",
  release: $rel, requested_by: "operator", requested_at: "2026-10-12T18:05:00Z"}]}' >"$R/pending.json"
action poll >/dev/null 2>&1 || fail "poll failed on a deploy ask"
grep -q "release-actions/ra-20261012T180500Z-2/claim" "$R/curl-args" || fail "poll did not claim the ask"
[[ "$(jq -r '.ok' "$R/results.jsonl" | tail -1)" == true ]] || fail "poll did not report the deploy as done"
tail -n 1 "$R/results.jsonl" | jq -r '.log_tail' | grep -q "DEPLOYED $rel build=build-1" \
  || fail "the reported log tail is not the deploy's own output"
ok "release-action poll claims a deploy ask, runs it, and reports it with its log"

# A foreign oldest ask must not starve the local ask after it.
rm -f "$R/claimed"
jq --arg rel "$rel" '.actions += [{id: "ra-20261012T180000Z-1", action: "deploy", repo: "acme/other", release: $rel, requested_by: "operator", requested_at: "2026-10-12T18:00:00Z"}]' "$R/pending.json" >"$R/mixed.json"
mv "$R/mixed.json" "$R/pending.json"
: >"$R/results.jsonl"
action poll >/dev/null 2>&1 || fail "poll failed on a mixed-repository queue"
[[ "$(jq -s length "$R/results.jsonl")" == 1 ]] || fail "a foreign oldest ask starved the local ask"
ok "release-action skips foreign asks before selecting the oldest local ask"

redaction_input="jpat_$(printf a%.0s {1..64})"
redacted="$(jq -cn --argjson ok false --arg message "Bearer $redaction_input" --arg log "https://user:pass@example.invalid/path?credential=hidden token=hidden $redaction_input" -f "$here/redact-release-result.jq")"
[[ "$redacted" != *"$redaction_input"* && "$redacted" != *"hidden"* && "$redacted" != *"user:pass"* ]] || fail "release result leaked credentials: $redacted"
ok "release-action redacts credentials and URL queries from both result fields"

for prefix in jpat jat; do
  redaction_input="${prefix}_$(printf X%.0s {1..32})"
  redacted="$(jq -cn --argjson ok false --arg message "$redaction_input" --arg log "$redaction_input" -f "$here/redact-release-result.jq")"
  [[ "$redacted" != *"$redaction_input"* ]] || fail "release result leaked a variable-format token"
done
ok "release-action redacts bare agent and variable-format personal tokens"

# A forged/malformed host argument must still produce the claimed ask's failure result.
rm -f "$R/claimed"
jq -n '{actions: [{id: "ra-20261012T181000Z-3", action: "deploy", repo: "acme/forge-deploy", release: "bad-release", requested_by: "operator"}]}' >"$R/pending.json"
action poll >/dev/null 2>&1 || fail "poll exited before reporting malformed arguments"
[[ "$(jq -r '.message' "$R/results.jsonl" | tail -1)" == "exit 64:"* ]] || fail "malformed argument was not reported"
ok "release-action reports malformed arguments after claiming an ask"

# The local deployment lock survives any forge claim expiry and is checked before cache writes.
(
  exec 9>"$R/state/deploy.lock"
  flock 9
  rc=0; action deploy "$rel" >/dev/null 2>&1 || rc=$?
  [[ $rc == 75 ]] || fail "a concurrent deployment was not refused"
) || exit 1
ok "release-action refuses a second deployment while the host lock is held"

rm -f "$R/claimed"
jq -n --arg rel "$rel" '{actions: [{id: "ra-20261012T180500Z-2", action: "deploy", repo: "acme/forge-deploy", release: $rel, requested_by: "operator"}]}' >"$R/pending.json"
action poll >/dev/null 2>&1 || fail "valid poll after malformed argument failed"


# Claimed elsewhere first: nothing runs. Then a failing deploy is reported as failed.
: >"$R/results.jsonl"
action poll >/dev/null 2>&1 || fail "poll failed when the ask was already claimed"
[[ ! -s "$R/results.jsonl" ]] || fail "poll ran an ask another poller had claimed"
rm -f "$R/claimed"; touch "$R/deploy-fails"
action poll >/dev/null 2>&1 || fail "poll itself failed on a failing deploy"
[[ "$(jq -r '.ok' "$R/results.jsonl" | tail -1)" == false ]] || fail "a failed deploy was reported as done"
tail -n 1 "$R/results.jsonl" | jq -r '.message' | grep -q "^exit 3" || fail "the failure does not say how it ended"
rm -f "$R/claimed" "$R/deploy-fails"
jq -n '{actions: [{id: "ra-20261012T181000Z-3", action: "shell", repo: "acme/forge-deploy",
  requested_by: "operator", requested_at: "2026-10-12T18:10:00Z"}]}' >"$R/pending.json"
action poll >/dev/null 2>&1 || fail "poll failed on an unknown action"
[[ "$(jq -r '.ok' "$R/results.jsonl" | tail -1)" == false ]] || fail "an unknown action was reported as done"
ok "release-action poll runs an ask once, and reports a failed or unknown step as failed"

# --- auto-tag.sh: cut the next split tag from a green main, then the pin bump stack ---
# Stand-ins: local bare repos as the jeryu-deploy and crate remotes, and a curl that answers the
# status API, the pull request list, the pull request POST, the events route and the heartbeat,
# recording every body. No network, no real token, and the real bump-hosted-pin.sh on main.
G="$T/tag"; mkdir -p "$G/bin" "$G/remotes"
gt() { git -C "$1" -c user.name=t -c user.email=t@t "${@:2}"; }
crate_commit() { # REPO SUBJECT [AUTHOR_EMAIL] [PATH]: one commit on the crate's main, pushed
  local repo="$1" subject="$2" email="${3:-dev@forge.invalid}" path="${4:-src/lib.rs}"
  mkdir -p "$(dirname "$G/$repo/$path")"
  printf '%s\n' "$subject" >>"$G/$repo/$path"
  git -C "$G/$repo" add -A
  GIT_AUTHOR_NAME=author GIT_AUTHOR_EMAIL="$email" GIT_COMMITTER_NAME=author \
    GIT_COMMITTER_EMAIL="$email" git -C "$G/$repo" commit -q -m "$subject"
  git -C "$G/$repo" push -q "$G/remotes/$repo.git" main
  git -C "$G/$repo" rev-parse HEAD
}
mk_crate() { # REPO: a crate repository tagged <repo>-v5.0.0-split.1, with that tag's support branch
  local repo="$1" d="$G/$1"
  mkdir -p "$d/src" "$d/docs"
  gt "$d" init -q -b main
  local packages="$repo"
  [[ "$repo" != jeryu-core ]] || packages="jeryu-core jeryu-proof jeryu-frozen"
  [[ "$repo" != jeryu-ci-runner ]] || packages="jeryu-runnerd"
  printf '[workspace]\nmembers = ["crates/*"]\n' >"$d/Cargo.toml"
  for package in $packages; do
    mkdir -p "$d/crates/$package/src"
    printf '[package]\nname = "%s"\nversion = "5.0.0"\nedition = "2021"\n' "$package" >"$d/crates/$package/Cargo.toml"
    : >"$d/crates/$package/src/lib.rs"
  done
  printf 'initial\n' >"$d/src/lib.rs"; printf '# %s\n' "$repo" >"$d/docs/readme.md"
  gt "$d" add -A; gt "$d" commit -q -m "initial $repo"
  gt "$d" tag -a -m "$repo-v5.0.0-split.1" "$repo-v5.0.0-split.1"
  git clone -q --bare "$d" "$G/remotes/$repo.git"
  git -C "$G/remotes/$repo.git" update-ref \
    "refs/heads/preserve/hosted-cargo/$repo-v5.0.0-split.1" "$(git -C "$d" rev-parse HEAD)"
  git -C "$d" rev-parse HEAD
}
core_c1="$(mk_crate jeryu-core)"
runner_c1="$(mk_crate jeryu-ci-runner)"

D="$G/deploy"; mkdir -p "$D/.cargo" "$D/crates/jeryu-api" "$D/scripts/release" "$D/ops/ci"
cat >"$D/.cargo/hosted-pin-refs.tsv" <<EOF
# repository|immutable tag|commit|hosted support ref
jeryu-ci-runner|jeryu-ci-runner-v5.0.0-split.1|$runner_c1|refs/heads/preserve/hosted-cargo/jeryu-ci-runner-v5.0.0-split.1
jeryu-core|jeryu-core-v5.0.0-split.1|$core_c1|refs/heads/preserve/hosted-cargo/jeryu-core-v5.0.0-split.1
EOF
printf 'jeryu-core\t1\njeryu-ci-runner\t2\n' >"$D/scripts/release/hosted-crates.tsv"
cat >"$D/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/jeryu-api"]

# A deliberately frozen source that [patch] redirects: a bump must never move it.
[patch."https://github.com/neverhuman/jeryu-core.git"]
jeryu-core = { git = "http://127.0.0.1:8787/git/jeryu/jeryu-core.git", tag = "jeryu-core-v5.0.0-split.1" }
jeryu-frozen = { git = "http://127.0.0.1:8787/git/jeryu/jeryu-core.git", tag = "jeryu-core-v5.0.0-split.1" }

[patch."https://github.com/neverhuman/jeryu-ci-runner.git"]
jeryu-runnerd = { git = "http://127.0.0.1:8787/git/jeryu/jeryu-ci-runner.git", tag = "jeryu-ci-runner-v5.0.0-split.1" }
EOF
cat >"$D/crates/jeryu-api/Cargo.toml" <<'EOF'
[package]
name = "jeryu-api"
version = "5.0.0"
edition = "2021"
[dependencies]
jeryu-runnerd = { git = "http://127.0.0.1:8787/git/jeryu/jeryu-ci-runner.git", tag = "jeryu-ci-runner-v5.0.0-split.1" }
jeryu-core = { git = "http://127.0.0.1:8787/git/jeryu/jeryu-core.git", tag = "jeryu-core-v5.0.0-split.1", package = "jeryu-core" }
jeryu-frozen = { git = "https://github.com/neverhuman/jeryu-core.git", tag = "jeryu-core-v5.0.0-split.0", package = "jeryu-frozen" }
EOF
mkdir -p "$D/crates/jeryu-api/src"
: >"$D/crates/jeryu-api/src/lib.rs"
cp "$here/../../ops/ci/hosted-git-env.sh" "$D/ops/ci/"
mkdir -p "$D/.cargo"; cp "$here/../../.cargo/hosted-gitconfig" "$here/../../.cargo/config.toml" "$D/.cargo/"
cp "$here/../../rust-toolchain.toml" "$D/"
# Cargo uses real local Git remotes through the same transport setup as production.
export CARGO_HOME="$G/cargo-home"
export GIT_CONFIG_GLOBAL="$G/gitconfig"
git config --file "$GIT_CONFIG_GLOBAL" url."file://$G/remotes/".insteadOf http://127.0.0.1:8787/git/jeryu/
git clone -q --bare "$G/jeryu-core" "$G/frozen/jeryu-core.git"
git config --file "$GIT_CONFIG_GLOBAL" url."file://$G/frozen/".insteadOf https://github.com/neverhuman/
# Frozen source tags exist even though patches redirect their packages.
git -C "$G/frozen/jeryu-core.git" tag jeryu-core-v5.0.0-split.0 "$core_c1"
(cd "$D" && source ops/ci/hosted-git-env.sh && cargo metadata --format-version 1 >/dev/null)
cp "$here/bump-hosted-pin.sh" "$D/scripts/release/bump-hosted-pin.sh"
chmod +x "$D/scripts/release/bump-hosted-pin.sh"
gt "$D" init -q -b main; gt "$D" add -A; gt "$D" commit -q -m deploy
git clone -q --bare "$D" "$G/remotes/jeryu-deploy.git"

echo 1 >"$G/pr-number"; echo '[]' >"$G/pulls.json"; echo success >"$G/gate"
: >"$G/events.jsonl"; : >"$G/beats.jsonl"; : >"$G/prs.jsonl"; : >"$G/curl-args"
echo "not-a-real-tag-token" >"$G/token"; chmod 600 "$G/token"
cat >"$G/bin/curl" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$G/curl-args"
url="\${!#}"; method=GET; prev=""
for arg in "\$@"; do [ "\$prev" = -X ] && method="\$arg"; prev="\$arg"; done
case "\$method \$url" in
  "POST https://git.neverhuman.org/api/v1/events") cat >>"$G/events.jsonl"; echo >>"$G/events.jsonl"; echo '{"ok":true}' ;;
  "POST https://git.neverhuman.org/api/v1/runners/heartbeat")
    cat >>"$G/beats.jsonl"; echo >>"$G/beats.jsonl"; echo '{"accepted":true}' ;;
  "POST https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-deploy/pulls")
    n="\$(cat "$G/pr-number")"; echo \$(( n + 1 )) >"$G/pr-number"
    cat >>"$G/prs.jsonl"; echo >>"$G/prs.jsonl"
    jq -cn --argjson n "\$n" '{number: \$n}' ;;
  "GET https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-deploy/pulls?"*)
    page="\${url##*page=}"; page="\${page%%&*}"
    if [ "\$page" = 1 ]; then cat "$G/pulls.json"; else echo '[]'; fi ;;
  "GET https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-"*"/commits/"*"/status")
    state="\$(cat "$G/gate")"; repo="\${url#*/repos/jeryu/}"; repo="\${repo%%/*}"
    jq -n --arg state "\$state" --arg context "\$repo/required" '{state: \$state, statuses: [
      {context: \$context, state: \$state, updated_at: "2026-10-21T10:00:00Z"},
      {context: \$context, state: "pending", updated_at: "2026-10-21T09:00:00Z"}]}' ;;
  *) echo "unexpected curl: \$method \$url" >&2; exit 22 ;;
esac
EOF
chmod +x "$G/bin/curl"
# The tagging identity and the credential path are site settings with no default, so every
# invocation names them. The identity below is this test's, not a site's. It starts in / as the
# timer starts outside any repository.
auto_tag() ( # [--dry-run]
  cd / && PATH="$G/bin:$PATH" JERYU_DEPLOY_REMOTE="$G/remotes/jeryu-deploy.git" \
    JERYU_CRATE_REMOTE_PREFIX="$G/remotes" JERYU_DEPLOY_REPO=jeryu/jeryu-deploy \
    JERYU_AUTO_TAG_STATE="$G/state" JERYU_AUTO_TAG_COALESCE="${TAG_COALESCE-0}" \
    JERYU_TAG_GIT_NAME="${TAG_GIT_NAME-releasebot}" JERYU_TAG_GIT_EMAIL="${TAG_GIT_EMAIL-releasebot@forge.invalid}" \
    JERYU_TAG_TOKEN_FILE="${TAG_TOKEN_FILE-$G/token}" JERYU_BASE="${TAG_BASE:-https://git.neverhuman.org}" \
    bash "$here/auto-tag.sh" "$@"
)
crate_tags() { git -C "$G/remotes/$1.git" for-each-ref --format='%(refname:short)' refs/tags; }
support_ref() { git -C "$G/remotes/$1.git" rev-parse --verify -q "refs/heads/preserve/hosted-cargo/$2" || true; }
deploy_branches() { git -C "$G/remotes/jeryu-deploy.git" for-each-ref --format='%(refname:short)' 'refs/heads/auto/*'; }
newest_event() { tail -n 2 "$G/events.jsonl" | grep . | tail -n 1; }

rc=0; TAG_BASE=https://git.neverhuman.org.evil.invalid auto_tag >"$G/run.log" 2>&1 || rc=$?
[[ $rc == 2 && ! -s "$G/curl-args" ]] || fail "auto-tag sent a request to a noncanonical origin"
chmod 644 "$G/token"; rc=0; auto_tag >"$G/run.log" 2>&1 || rc=$?
[[ $rc == 2 && ! -s "$G/curl-args" ]] || fail "auto-tag accepted a group-readable token file"
chmod 600 "$G/token"
rc=0; TAG_TOKEN_FILE= auto_tag >"$G/run.log" 2>&1 || rc=$?
[[ $rc == 2 ]] || fail "auto-tag ran without JERYU_TAG_TOKEN_FILE"
grep -q "JERYU_TAG_TOKEN_FILE is unset" "$G/run.log" || fail "the refusal does not name the credential path"
rc=0; TAG_GIT_NAME= auto_tag >"$G/run.log" 2>&1 || rc=$?
[[ $rc == 2 ]] || fail "auto-tag ran without a tagging identity"
grep -q "JERYU_TAG_GIT_NAME" "$G/run.log" || fail "the refusal does not name the identity"
[[ ! -s "$G/curl-args" && -z "$(deploy_branches)" ]] || fail "a refused tick acted anyway"
ok "auto-tag refuses a foreign origin, a loose token file and unset site settings before any request"

# Documentation only, and a Jeryu Split Bot re-render, earn no tag.
crate_commit jeryu-core "docs: say more" dev@forge.invalid docs/readme.md >/dev/null
crate_commit jeryu-core "chore(jankurai): re-render from jeryu-tool main abc1234" split-bot@localhost \
  rendered/jankurai.txt >/dev/null
auto_tag >"$G/run.log" 2>&1 || { cat "$G/run.log" >&2; fail "auto-tag failed on a documentation-only main"; }
[[ "$(crate_tags jeryu-core)" == jeryu-core-v5.0.0-split.1 ]] || fail "auto-tag tagged a documentation-only main"
grep -q "nothing but documentation or a re-render" "$G/run.log" || fail "auto-tag did not say why it cut no tag"
[[ -z "$(deploy_branches)" ]] || fail "auto-tag opened a bump with no new tag"
ok "auto-tag cuts no tag for documentation-only and re-render commits since the tag"

# A source change that main's required gate has not passed waits.
mkdir -p "$G/jeryu-core/crates/jeryu-gitd/src"
printf '[package]\nname = "jeryu-gitd"\nversion = "5.0.0"\nedition = "2021"\n' >"$G/jeryu-core/crates/jeryu-gitd/Cargo.toml"
: >"$G/jeryu-core/crates/jeryu-gitd/src/lib.rs"
printf '\n[dependencies]\njeryu-gitd = { path = "../jeryu-gitd" }\n' >>"$G/jeryu-core/crates/jeryu-core/Cargo.toml"
core_c2="$(crate_commit jeryu-core "add a function")"
echo pending >"$G/gate"
auto_tag >"$G/run.log" 2>&1 || { cat "$G/run.log" >&2; fail "auto-tag failed while a gate was pending"; }
[[ "$(crate_tags jeryu-core)" == jeryu-core-v5.0.0-split.1 ]] || fail "auto-tag tagged a main that is not green"
grep -q "does not have jeryu-core/required success" "$G/run.log" || fail "auto-tag did not say it waits for the gate"
echo success >"$G/gate"
ok "auto-tag waits while the crate's main head has no required success"

# A dry run says what it would do and changes nothing.
: >"$G/events.jsonl"; : >"$G/beats.jsonl"
auto_tag --dry-run >"$G/run.log" 2>&1 || { cat "$G/run.log" >&2; fail "the dry run failed"; }
grep -q "would tag ${core_c2:0:12} as jeryu-core-v5.0.0-split.2" "$G/run.log" || { cat "$G/run.log" >&2; fail "the dry run did not name the tag it would cut"; }
grep -q "would open a bump pull request: jeryu-core jeryu-core-v5.0.0-split.1 -> jeryu-core-v5.0.0-split.2" "$G/run.log" \
  || fail "the dry run did not name the bump it would open"
[[ "$(crate_tags jeryu-core)" == jeryu-core-v5.0.0-split.1 ]] || fail "the dry run cut a tag"
[[ -z "$(deploy_branches)" && ! -s "$G/prs.jsonl" && ! -s "$G/events.jsonl" && ! -s "$G/beats.jsonl" ]] \
  || fail "the dry run pushed, posted or beat"
ok "auto-tag --dry-run prints the tag and the bump it would make, and writes nothing anywhere"

# The live tick: the tag and its preserve branch land together, main gains no commit, and the
# bump pull request follows.
deploy_main_before="$(git -C "$G/remotes/jeryu-deploy.git" rev-parse refs/heads/main)"
core_main_before="$(git -C "$G/remotes/jeryu-core.git" rev-parse refs/heads/main)"
auto_tag >"$G/run.log" 2>&1 || { cat "$G/run.log" >&2; fail "auto-tag failed on the happy path"; }
[[ "$(crate_tags jeryu-core | LC_ALL=C sort | tr '\n' ' ')" == "jeryu-core-v5.0.0-split.1 jeryu-core-v5.0.0-split.2 " ]] \
  || fail "auto-tag did not cut split.2: $(crate_tags jeryu-core | tr '\n' ' ')"
[[ "$(git -C "$G/remotes/jeryu-core.git" rev-list -n 1 jeryu-core-v5.0.0-split.2)" == "$core_c2" ]] \
  || fail "split.2 does not name the green head"
[[ "$(git -C "$G/remotes/jeryu-core.git" cat-file -t jeryu-core-v5.0.0-split.2)" == tag ]] \
  || fail "split.2 is not an annotated tag"
[[ "$(support_ref jeryu-core jeryu-core-v5.0.0-split.2)" == "$core_c2" ]] \
  || fail "split.2 has no preserve/hosted-cargo branch at its commit"
[[ "$(git -C "$G/remotes/jeryu-core.git" rev-parse refs/heads/main)" == "$core_main_before" ]] \
  || fail "tagging added a commit to the crate's main (a VERSION stamp)"
[[ "$(git -C "$G/remotes/jeryu-deploy.git" rev-parse refs/heads/main)" == "$deploy_main_before" ]] \
  || fail "auto-tag pushed to jeryu-deploy main"
[[ "$(awk -F'\t' 'END { print $1, $2, $3 }' "$G/state/tagged.tsv")" == "jeryu-core jeryu-core-v5.0.0-split.2 $core_c2" ]] \
  || fail "the tag was not recorded"
jq -es '[.[] | select(.kind == "tag.cut")] | length == 1' "$G/events.jsonl" >/dev/null || fail "no single tag.cut event"
jq -es --arg tag jeryu-core-v5.0.0-split.2 '[.[] | select(.kind == "tag.cut")][0]
  | .repo == "jeryu/jeryu-core" and .source == "auto-tag" and .needs_human == false
    and .detail.tag == $tag and .detail.from_tag == "jeryu-core-v5.0.0-split.1"
    and .detail.support_ref == ("refs/heads/preserve/hosted-cargo/" + $tag)' "$G/events.jsonl" >/dev/null \
  || fail "the tag.cut event is not the contract's"
ok "auto-tag cuts the next annotated split tag with its preserve branch, records it, and stamps no VERSION commit"

# The bump that follows: exactly the pin's files, the frozen split.0 line untouched, one PR.
bump_branch="$(deploy_branches)"
[[ "$bump_branch" == "auto/pin-jeryu-core-2-${core_c2:0:12}" ]] || fail "unexpected bump branch: $bump_branch"
git -C "$G/remotes/jeryu-deploy.git" diff --name-only main "$bump_branch" >"$G/bump-files"
[[ "$(LC_ALL=C sort "$G/bump-files" | tr '\n' ' ')" == ".cargo/hosted-pin-refs.tsv Cargo.lock Cargo.toml crates/jeryu-api/Cargo.toml " ]] \
  || fail "the bump changed $(tr '\n' ' ' <"$G/bump-files")"
git -C "$G/remotes/jeryu-deploy.git" show "$bump_branch:crates/jeryu-api/Cargo.toml" >"$G/bumped-api-manifest"
grep -q 'jeryu-core-v5.0.0-split.2", package = "jeryu-core"' "$G/bumped-api-manifest" || fail "the bump did not move the pin line"
grep -q 'jeryu-core-v5.0.0-split.0", package = "jeryu-frozen"' "$G/bumped-api-manifest" || fail "the bump moved a frozen [patch] source"
git -C "$G/remotes/jeryu-deploy.git" show "$bump_branch:Cargo.lock" \
  | grep -q "jeryu-core.git?tag=jeryu-core-v5.0.0-split.2#$core_c2" || fail "Cargo.lock does not name the new tag and commit"
git -C "$G/remotes/jeryu-deploy.git" show "$bump_branch:.cargo/hosted-pin-refs.tsv" \
  | grep -qx "jeryu-core|jeryu-core-v5.0.0-split.2|$core_c2|refs/heads/preserve/hosted-cargo/jeryu-core-v5.0.0-split.2" \
  || fail "the pin policy row was not bumped"
[[ "$(jq -s length "$G/prs.jsonl")" == 1 ]] || fail "auto-tag did not open exactly one bump"
jq -e '.title == "release: pin jeryu-core jeryu-core-v5.0.0-split.2" and .base == "main"
  and (.body | contains("preserve/hosted-cargo")) and (.body | contains("dependency-sources.sh"))' \
  "$G/prs.jsonl" >/dev/null || fail "the bump pull request is not the contract's"
jq -es '[.[] | select(.kind == "pin.bump_opened")][0] | .pr == 1 and .repo == "jeryu/jeryu-deploy"
  and .detail.dependency == "jeryu/jeryu-core" and .detail.to == "jeryu-core-v5.0.0-split.2"' \
  "$G/events.jsonl" >/dev/null || fail "no pin.bump_opened event for the crate bump"
jq -es '.[-1] | .runnerId == "'"$(hostname -s)"'/auto-tag" and .labels == ["automation"]
  and .last.recipe == "auto-tag"' "$G/beats.jsonl" >/dev/null || fail "the heartbeat is not the contract's"
ok "auto-tag opens one pin bump pull request that moves only the pin's three places, never a frozen source"

# A bump already open: no second stack, and no tag is held up by it.
jq -n --arg b "$bump_branch" '[{number: 1, state: "open", title: "release: pin jeryu-core jeryu-core-v5.0.0-split.2",
  head: {ref: $b, sha: "0000000000000000000000000000000000000000", repo: {full_name: "jeryu/jeryu-deploy"}},
  base: {ref: "main", repo: {full_name: "jeryu/jeryu-deploy"}}}]' >"$G/pulls.json"
prs_before="$(jq -s length "$G/prs.jsonl")"
auto_tag >"$G/run.log" 2>&1 || { cat "$G/run.log" >&2; fail "auto-tag failed with a bump open"; }
grep -q "crate pin bump pull requests are open" "$G/run.log" || fail "auto-tag did not say it waits for the open bump"
[[ "$(jq -s length "$G/prs.jsonl")" == "$prs_before" ]] || fail "auto-tag opened a second stack beside an open bump"
ok "auto-tag builds no second stack while a crate pin bump is open"

# Coalescing: a new green source commit right after a tag waits for the window.
core_c3="$(crate_commit jeryu-core "another function")"
TAG_COALESCE=3600 auto_tag >"$G/run.log" 2>&1 || { cat "$G/run.log" >&2; fail "auto-tag failed while coalescing"; }
grep -q "coalescing until the window" "$G/run.log" || fail "auto-tag did not coalesce inside the window"
[[ "$(crate_tags jeryu-core | grep -c .)" == 2 ]] || fail "auto-tag cut a second tag inside the coalescing window"
ok "auto-tag cuts at most one tag per repository per window"

# A tag that lost its preserve branch is repaired before anything new is cut: the consumer's
# dependency-source gate reads that branch back, and a tag without it breaks the gate.
git -C "$G/remotes/jeryu-core.git" update-ref -d refs/heads/preserve/hosted-cargo/jeryu-core-v5.0.0-split.2
TAG_COALESCE=3600 auto_tag >"$G/run.log" 2>&1 || { cat "$G/run.log" >&2; fail "auto-tag failed on a missing preserve branch"; }
[[ "$(support_ref jeryu-core jeryu-core-v5.0.0-split.2)" == "$core_c2" ]] || fail "the missing preserve branch was not repaired"
grep -q "pushed the missing refs/heads/preserve/hosted-cargo/jeryu-core-v5.0.0-split.2" "$G/run.log" \
  || fail "the repair was not reported"
jq -es 'any(.[]; .kind == "tag.preserve_repaired")' "$G/events.jsonl" >/dev/null || fail "no tag.preserve_repaired event"
ok "auto-tag repairs a tag whose preserve/hosted-cargo branch is missing, before cutting anything new"

# A push the forge refuses must leave neither the tag nor its preserve branch: a tag whose
# preserve branch never landed is exactly what breaks the consumer's dependency-source gate.
# The stand-in git refuses only a push that carries a tag, as a protected ref would.
tag_real_git="$(command -v git)"
cat >"$G/bin/git" <<EOF
#!/usr/bin/env bash
if [ -e "$G/refuse-tag-push" ]; then
  case " \$* " in *" push "*refs/tags/*) echo "remote: refusing to create a tag" >&2; exit 1 ;; esac
fi
exec "$tag_real_git" "\$@"
EOF
chmod +x "$G/bin/git"
: >"$G/refuse-tag-push"; : >"$G/events.jsonl"
tags_before="$(crate_tags jeryu-core | grep -c .)"
auto_tag >"$G/run.log" 2>&1 || { cat "$G/run.log" >&2; fail "a refused tag push failed the tick"; }
[[ "$(crate_tags jeryu-core | grep -c .)" == "$tags_before" ]] || fail "a refused push left a tag behind"
[[ -z "$(support_ref jeryu-core jeryu-core-v5.0.0-split.3)" ]] \
  || fail "a refused push left the preserve branch without its tag"
[[ -z "$(git -C "$G/state/jeryu-core.git" tag -l jeryu-core-v5.0.0-split.3)" ]] \
  || fail "a refused push left the tag in the timer's cache, so a retry would skip it"
jq -es '[.[] | select(.kind == "tag.failed")][0] | .needs_human == true' "$G/events.jsonl" >/dev/null \
  || fail "a refused push did not ask for a person"
rm -f "$G/refuse-tag-push"
ok "a tag push the forge refuses leaves neither the tag nor its preserve branch, and asks for a person"

# A main that no longer descends from the tag is left for a person, with an event that asks.
git -C "$G/remotes/jeryu-core.git" update-ref refs/heads/main "$core_c1"
git -C "$G/jeryu-core" reset -q --hard "$core_c1"
: >"$G/events.jsonl"
auto_tag >"$G/run.log" 2>&1 || { cat "$G/run.log" >&2; fail "auto-tag failed on a rewritten main"; }
grep -q "does not descend from jeryu-core-v5.0.0-split." "$G/run.log" || fail "auto-tag did not report the rewritten history"
jq -es '[.[] | select(.kind == "tag.diverged")][0] | .needs_human == true' "$G/events.jsonl" >/dev/null \
  || fail "a rewritten main did not ask for a person"
[[ "$(crate_tags jeryu-core | grep -c .)" == 2 ]] || fail "auto-tag tagged a rewritten main"
ok "auto-tag leaves a main that no longer descends from its tag for a person"

# The stack: two crates behind, in dependency order, each bump based on the one before it.
git -C "$G/remotes/jeryu-core.git" update-ref refs/heads/main "$core_c3"
git -C "$G/jeryu-core" reset -q --hard "$core_c3"
runner_c2="$(crate_commit jeryu-ci-runner "runner change")"
echo '[]' >"$G/pulls.json"; : >"$G/prs.jsonl"; : >"$G/events.jsonl"
rm -rf "$G/state/jeryu-core.git"
auto_tag >"$G/run.log" 2>&1 || { cat "$G/run.log" >&2; fail "auto-tag failed on the two-crate stack"; }
[[ "$(crate_tags jeryu-core | grep -c .)" == 3 && "$(crate_tags jeryu-ci-runner | grep -c .)" == 2 ]] \
  || fail "auto-tag did not tag both crates"
[[ "$(support_ref jeryu-ci-runner jeryu-ci-runner-v5.0.0-split.2)" == "$runner_c2" ]] \
  || fail "the runner tag has no preserve branch"
[[ "$(jq -s length "$G/prs.jsonl")" == 2 ]] || fail "auto-tag did not open two bumps"
jq -es '.[0].title == "release: pin jeryu-core jeryu-core-v5.0.0-split.3" and .[0].base == "main"
  and .[1].title == "release: pin jeryu-ci-runner jeryu-ci-runner-v5.0.0-split.2"
  and .[1].base == .[0].head' "$G/prs.jsonl" >/dev/null \
  || { cat "$G/prs.jsonl" >&2; fail "the stack is not in dependency order, based on the bump before it"; }
runner_branch="$(jq -rs '.[1].head' "$G/prs.jsonl")"
git -C "$G/remotes/jeryu-deploy.git" show "$runner_branch:.cargo/hosted-pin-refs.tsv" >"$G/stacked-policy"
grep -q "jeryu-core|jeryu-core-v5.0.0-split.3|$core_c3|" "$G/stacked-policy" \
  || fail "the stacked bump lost the core pin it is based on"
grep -q "jeryu-ci-runner|jeryu-ci-runner-v5.0.0-split.2|$runner_c2|" "$G/stacked-policy" \
  || fail "the stacked bump does not carry the runner pin"
ok "auto-tag bumps dependent crates in dependency order as one stack, so main never pins a mismatched pair"

# A bump somebody already decided is never reopened, and the rest of the stack waits with it:
# stacking the next bump on a branch that will not land is how main ends up pinning a mismatched
# pair. The tick ends nonzero, so the timer's status says a person is needed.
core_bump_branch="$(jq -rs '.[0].head' "$G/prs.jsonl")"
jq -n --arg b "$core_bump_branch" '[{number: 9, state: "closed", title: "release: pin jeryu-core jeryu-core-v5.0.0-split.3",
  head: {ref: $b, sha: "0000000000000000000000000000000000000000", repo: {full_name: "jeryu/jeryu-deploy"}},
  base: {ref: "main", repo: {full_name: "jeryu/jeryu-deploy"}}}]' >"$G/pulls.json"
prs_before="$(jq -s length "$G/prs.jsonl")"
rc=0; auto_tag >"$G/run.log" 2>&1 || rc=$?
[[ $rc == 1 ]] || { cat "$G/run.log" >&2; fail "a decided bump did not end the tick nonzero"; }
grep -q "#9 already decided the bump" "$G/run.log" || fail "auto-tag did not name the decided bump"
[[ "$(jq -s length "$G/prs.jsonl")" == "$prs_before" ]] || fail "auto-tag reopened a decided bump or stacked past it"
echo '[]' >"$G/pulls.json"
ok "auto-tag never reopens a bump a person decided, and stops the stack behind it"

# A stack pushed by an earlier tick that never opened its pull requests: the recipe runs again in
# auto-tag's own checkout, and each pushed branch is adopted only when its tree is the recipe's.
: >"$G/prs.jsonl"
auto_tag >"$G/run.log" 2>&1 || { cat "$G/run.log" >&2; fail "auto-tag failed resuming a pushed stack"; }
[[ "$(jq -s length "$G/prs.jsonl")" == 2 ]] || { cat "$G/run.log" >&2; fail "auto-tag did not reopen the pushed stack"; }
jq -es --arg core "$core_bump_branch" --arg runner "$runner_branch" \
  '.[0].head == $core and .[1].head == $runner and .[1].base == $core' "$G/prs.jsonl" >/dev/null \
  || fail "the resumed stack did not adopt the pushed branches"
grep -q "is already pushed with the recipe's tree" "$G/run.log" || fail "auto-tag did not say it checked the pushed tree"
ok "auto-tag resumes a pushed stack only after the recipe reproduces each branch's tree"

# A pushed branch that carries anything else (here a rust-toolchain.toml whose path is a cargo the
# runner would execute) is never checked out for the next bump, and the stack stops for a person.
core_bump_sha="$(git -C "$G/remotes/jeryu-deploy.git" rev-parse "refs/heads/$core_bump_branch")"
mkdir -p "$G/evil-toolchain/bin"
printf '#!/usr/bin/env bash\ntouch "%s/evil-ran"\n' "$G" >"$G/evil-toolchain/bin/cargo"; chmod +x "$G/evil-toolchain/bin/cargo"
rm -rf "$G/tamper"; git clone -q "$G/remotes/jeryu-deploy.git" "$G/tamper"
git -C "$G/tamper" checkout -q "$core_bump_branch"
printf '[toolchain]\npath = "%s/evil-toolchain"\n' "$G" >"$G/tamper/rust-toolchain.toml"
gt "$G/tamper" add rust-toolchain.toml; gt "$G/tamper" commit -q -m tamper
git -C "$G/tamper" push -q -f origin "HEAD:refs/heads/$core_bump_branch"
: >"$G/prs.jsonl"; : >"$G/events.jsonl"
rc=0; auto_tag >"$G/run.log" 2>&1 || rc=$?
[[ $rc == 1 ]] || { cat "$G/run.log" >&2; fail "a tampered pushed bump did not end the tick nonzero"; }
grep -q "the pushed $core_bump_branch is not the tree the recipe makes" "$G/run.log" \
  || { cat "$G/run.log" >&2; fail "auto-tag did not refuse the tampered pushed bump"; }
[[ ! -s "$G/prs.jsonl" ]] || fail "auto-tag opened a pull request for a tampered pushed bump"
[[ ! -e "$G/evil-ran" ]] || fail "the tampered branch's toolchain ran"
jq -es '[.[] | select(.kind == "pin.bump_failed")][0] | .needs_human == true and .detail.branch != null' \
  "$G/events.jsonl" >/dev/null || fail "a tampered pushed bump did not ask for a person"
git -C "$G/remotes/jeryu-deploy.git" update-ref "refs/heads/$core_bump_branch" "$core_bump_sha"
ok "auto-tag never runs the recipe in a pushed branch, and refuses one whose tree is not the recipe's"

# Once the stack has landed, every pin is current and the tick is a silent no-op that still beats.
git -C "$G/remotes/jeryu-deploy.git" update-ref refs/heads/main "$runner_branch"
for stale in $(deploy_branches); do git -C "$G/remotes/jeryu-deploy.git" update-ref -d "refs/heads/$stale"; done
: >"$G/beats.jsonl"
auto_tag >"$G/run.log" 2>&1 || { cat "$G/run.log" >&2; fail "an idle tick failed"; }
grep -q "no tag was cut this tick" "$G/run.log" || fail "an idle tick did not say so"
[[ "$(jq -s length "$G/beats.jsonl")" == 1 ]] || fail "an idle tick did not beat exactly once"
ok "an idle auto-tag tick cuts nothing and beats once"

# bump-hosted-pin.sh on its own: every refusal, and nothing half-edited.
BP="$G/bumpcheck"; git clone -q "$G/remotes/jeryu-deploy.git" "$BP" 2>/dev/null
# From the commit whose pin is still split.1, so these cases are about the recipe alone.
git -C "$BP" checkout -q "$deploy_main_before"
bump() ( cd "$BP" && bash "$here/bump-hosted-pin.sh" "$@" )
rc=0; bump ../evil jeryu-core-v5.0.0-split.9 "$core_c3" >"$G/bump.log" 2>&1 || rc=$?
[[ $rc == 64 ]] || fail "bump-hosted-pin accepted a path as a repository"
rc=0; bump jeryu-core jeryu-core-v5.0.0-split.9 deadbeef >"$G/bump.log" 2>&1 || rc=$?
[[ $rc == 64 ]] || fail "bump-hosted-pin accepted a short commit"
rc=0; bump jeryu-core jeryu-ci-runner-v5.0.0-split.9 "$core_c3" >"$G/bump.log" 2>&1 || rc=$?
[[ $rc == 64 ]] || fail "bump-hosted-pin accepted another repository's tag"
rc=0; bump jeryu-core jeryu-core-v5.0.0-split.0 "$core_c3" >"$G/bump.log" 2>&1 || rc=$?
[[ $rc == 1 ]] && grep -q "is not after" "$G/bump.log" || fail "bump-hosted-pin walked a pin backwards"
rc=0; bump jeryu-core jeryu-core-v5.1.0-split.9 "$core_c3" >"$G/bump.log" 2>&1 || rc=$?
[[ $rc == 1 ]] && grep -q "version line" "$G/bump.log" || fail "bump-hosted-pin changed a version line"
[[ -z "$(git -C "$BP" status --porcelain)" ]] || fail "a refused bump left the checkout edited"
# Resolution errors and a tag/commit mismatch must refuse before policy changes. A failed
# resolution leaves the manifests (and at most the lock) edited for a person, nothing else, and
# never runs the checkout's own transport script: the recipe sources the one beside it.
printf 'touch %q\n' "$G/checkout-transport-ran" >"$BP/ops/ci/hosted-git-env.sh"
for target in jeryu-core-v5.0.0-split.8 jeryu-core-v5.0.0-split.2; do
  rc=0; bump jeryu-core "$target" "$core_c3" >"$G/bump.log" 2>&1 || rc=$?
  [[ $rc == 1 ]] || fail "bump-hosted-pin accepted an unresolved or mismatched tag"
  git -C "$BP" diff --exit-code -- .cargo/hosted-pin-refs.tsv >/dev/null \
    || fail "a failed resolution updated the pin policy"
  left="$(git -C "$BP" diff --name-only -- . ':!ops/ci/hosted-git-env.sh' | LC_ALL=C sort | tr '\n' ' ')"
  [[ "$left" == "Cargo.lock Cargo.toml crates/jeryu-api/Cargo.toml " \
     || "$left" == "Cargo.toml crates/jeryu-api/Cargo.toml " ]] \
    || fail "a failed resolution left more than the manifests and the lock: $left"
  git -C "$BP" restore Cargo.toml crates/jeryu-api/Cargo.toml Cargo.lock
done
[[ ! -e "$G/checkout-transport-ran" ]] || fail "the bump ran the checkout's own hosted-git-env.sh"
git -C "$BP" checkout -q -- ops/ci/hosted-git-env.sh
ok "bump-hosted-pin refuses a missing tag and a tag at the wrong commit, running only its own transport"

# Cargo's own configuration is part of the boundary: a checkout whose .cargo/config.toml is not
# the recipe's, or a Cargo config in a directory above the checkout, is refused before any edit.
printf '[build]\nrustc = "%s"\n' "$G/evil-rustc" >>"$BP/.cargo/config.toml"
rc=0; bump jeryu-core jeryu-core-v5.0.0-split.2 "$core_c2" >"$G/bump.log" 2>&1 || rc=$?
[[ $rc == 1 ]] && grep -q "differs from the reviewed one" "$G/bump.log" \
  || fail "bump-hosted-pin resolved under the checkout's own Cargo config"
git -C "$BP" checkout -q -- .cargo/config.toml
# rustup reads rust-toolchain.toml, whose `[toolchain] path` names any cargo: only the reviewed one.
printf 'path = "%s"\n' "$G/evil-toolchain" >>"$BP/rust-toolchain.toml"
rc=0; bump jeryu-core jeryu-core-v5.0.0-split.2 "$core_c2" >"$G/bump.log" 2>&1 || rc=$?
[[ $rc == 1 ]] && grep -q "rust-toolchain.toml differs from the reviewed one" "$G/bump.log" \
  || fail "bump-hosted-pin resolved under the checkout's own rust-toolchain.toml"
git -C "$BP" checkout -q -- rust-toolchain.toml
: >"$BP/rust-toolchain"
rc=0; bump jeryu-core jeryu-core-v5.0.0-split.2 "$core_c2" >"$G/bump.log" 2>&1 || rc=$?
[[ $rc == 1 ]] && grep -q "legacy rust-toolchain file" "$G/bump.log" \
  || fail "bump-hosted-pin resolved beside a legacy rust-toolchain file"
rm -f "$BP/rust-toolchain"
mkdir -p "$G/above/.cargo"; printf '[build]\njobs = 1\n' >"$G/above/.cargo/config.toml"
git clone -q "$BP" "$G/above/checkout" 2>/dev/null
# A copy of the reviewed config above the checkout is what a nested lane sees; it passes the check
# (and the bump then fails later, at the missing tag, which is not this case's point).
cp "$here/../../.cargo/config.toml" "$G/above/.cargo/config.toml"
rc=0; ( cd "$G/above/checkout" && bash "$here/bump-hosted-pin.sh" jeryu-core jeryu-core-v5.0.0-split.8 "$core_c2" ) \
  >"$G/bump.log" 2>&1 || rc=$?
! grep -q "Cargo would read" "$G/bump.log" || fail "the reviewed config above a nested checkout was refused"
git -C "$G/above/checkout" checkout -q -- .
printf '[build]\njobs = 1\n' >"$G/above/.cargo/config.toml"
rc=0; ( cd "$G/above/checkout" && bash "$here/bump-hosted-pin.sh" jeryu-core jeryu-core-v5.0.0-split.2 "$core_c2" ) \
  >"$G/bump.log" 2>&1 || rc=$?
[[ $rc == 1 ]] && grep -q "Cargo would read $G/above/.cargo/config.toml" "$G/bump.log" \
  || fail "bump-hosted-pin resolved under a Cargo config above the checkout"
[[ -z "$(git -C "$BP" status --porcelain)" && -z "$(git -C "$G/above/checkout" status --porcelain)" ]] \
  || fail "a Cargo config refusal left the checkout edited"
rm -rf "$G/above"
# The archived overlay: with no caller GIT_CONFIG_GLOBAL the transport needs the recipe tree's own
# .cargo/hosted-gitconfig, and a recipe tree without it refuses rather than fall back.
R="$G/recipe-no-overlay"; mkdir -p "$R/scripts/release" "$R/ops/ci"
mkdir -p "$R/.cargo"; cp "$here/../../.cargo/config.toml" "$R/.cargo/"
cp "$here/../../rust-toolchain.toml" "$R/"
cp "$here/bump-hosted-pin.sh" "$R/scripts/release/"; cp "$here/../../ops/ci/hosted-git-env.sh" "$R/ops/ci/"
rc=0; ( cd "$BP" && env -u GIT_CONFIG_GLOBAL bash "$R/scripts/release/bump-hosted-pin.sh" \
  jeryu-core jeryu-core-v5.0.0-split.2 "$core_c2" ) >"$G/bump.log" 2>&1 || rc=$?
[[ $rc == 1 ]] && grep -q "hosted Git overlay must be a one-link regular file: $R/.cargo/hosted-gitconfig" "$G/bump.log" \
  || fail "a recipe tree without its overlay did not refuse: $(cat "$G/bump.log")"
[[ -z "$(git -C "$BP" status --porcelain)" ]] || fail "an overlay refusal left the checkout edited"
ok "bump-hosted-pin refuses an unreviewed Cargo config or toolchain file and a recipe tree without its overlay"
git -C "$G/remotes/jeryu-core.git" tag jeryu-core-v5.0.0-split.9 "$core_c3"
bump jeryu-core jeryu-core-v5.0.0-split.9 "$core_c3" >"$G/bump.log" 2>&1 \
  || { cat "$G/bump.log" >&2; fail "bump-hosted-pin refused a forward bump"; }
grep -q "bumped jeryu-core jeryu-core-v5.0.0-split.1 -> jeryu-core-v5.0.0-split.9" "$G/bump.log" \
  || fail "bump-hosted-pin did not report what it moved"
(cd "$BP" && cargo metadata --locked --format-version 1) >"$G/metadata.json" \
  || fail "the dependency-adding tag left Cargo.lock stale"
jq -e '.packages[] | select(.name == "jeryu-core") | .dependencies[] | select(.name == "jeryu-gitd")' \
  "$G/metadata.json" >/dev/null || fail "the bumped core is missing its new dependency"
ok "bump-hosted-pin resolves a tag that adds jeryu-gitd and passes cargo metadata --locked"
rc=0; bump jeryu-core jeryu-core-v5.0.0-split.9 "$core_c3" >"$G/bump.log" 2>&1 || rc=$?
[[ $rc == 1 ]] || fail "bump-hosted-pin bumped a pin that was already there"
ok "bump-hosted-pin refuses a foreign tag, a malformed commit, a backward or version-line move, and leaves nothing half-edited"

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
