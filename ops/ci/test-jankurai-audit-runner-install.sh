#!/usr/bin/env bash
# test-jankurai-audit-runner-install.sh — run install-jankurai-audit-runner.sh against a throwaway
# HOME with a stand-in systemctl: it unpacks exactly the runner's files from HEAD and records the
# commit, keeps the timer off until a token file is configured, then enables it, and leaves the
# units valid. No service, network or credential.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
T="$(mktemp -d)"
trap '[ -n "${KEEP:-}" ] || rm -rf "$T"' EXIT
pass=0
ok() { pass=$((pass + 1)); echo "ok $pass - $1"; }
fail() { echo "not ok - $1" >&2; exit 1; }

cat >"$T/systemctl" <<STUB
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$T/systemctl.log"
STUB
chmod +x "$T/systemctl"
run() { HOME="$T/home" JERYU_SYSTEMCTL="$T/systemctl" bash "$here/install-jankurai-audit-runner.sh" >"$T/out" 2>&1; }
mkdir -p "$T/home"

run || fail "first install failed: $(cat "$T/out")"
dest="$T/home/.local/share/jeryu-jankurai-audit-runner"
for f in ops/ci/jankurai-audit-runner.sh ops/ci/jankurai-audit-heartbeat.sh ops/ci/jankurai-audit-git-auth.sh \
  ops/ci/submit-jankurai-score.sh \
  ops/ci/lib.sh ops/ci/hosted-git-env.sh .cargo/hosted-gitconfig; do
  cmp -s "$dest/$f" <(git -C "$repo" show "HEAD:$f") || fail "$f is not HEAD's copy"
done
[ "$(cat "$dest/VERSION")" = "$(git -C "$repo" rev-parse HEAD)" ] || fail "VERSION does not name HEAD"
[ "$(find "$dest" -type f | wc -l)" = 8 ] || fail "unpacked more than the runner's files: $(find "$dest" -type f)"
[ ! -d "$dest/.git" ] || fail "the install is a git checkout"
ok "unpacks exactly the runner and what it sources from HEAD, and records the commit"

[ "$(stat -c %a "$T/home/.config/jeryu/jankurai-audit-runner.env")" = 600 ] || fail "env file is not mode 600"
grep -q "enable" "$T/systemctl.log" && fail "the timer was enabled without a token file"
grep -q "stays off" "$T/out" || fail "the run does not say why the timer is off: $(cat "$T/out")"
ok "without JERYU_FORGE_TOKEN_FILE the timer stays off, and the run says so"

printf 'JERYU_FORGE_TOKEN_FILE=%s/token\n' "$T" >"$T/home/.config/jeryu/jankurai-audit-runner.env"
run || fail "second install failed: $(cat "$T/out")"
grep -qx -- "--user enable --now jeryu-jankurai-audit-runner.timer" "$T/systemctl.log" || fail "timer not enabled"
[ -f "$T/home/.config/systemd/user/jeryu-jankurai-audit-runner.service" ] || fail "service unit not installed"
ok "with a token file configured, a re-run updates in place and enables the timer"

grep -q '^OnUnitInactiveSec=' "$here/systemd/jeryu-jankurai-audit-runner.timer" || fail "timer could overlap runs"
grep -q -- '--max 1' "$here/systemd/jeryu-jankurai-audit-runner.service" || fail "service claims more than one job"
grep -q '^MemoryMax=' "$here/systemd/jeryu-jankurai-audit-runner.service" || fail "service has no memory cap"
if command -v systemd-analyze >/dev/null; then
  systemd-analyze verify --user "$T/home/.config/systemd/user/jeryu-jankurai-audit-runner.timer" 2>&1 \
    | grep -v -E "jankurai-audit-runner.sh|Failed to (create|connect)|not executable|No such file" | grep . \
    && fail "systemd-analyze found problems in the units"
fi
ok "one claim at a time, never overlapping, memory-capped, and the units verify"
echo "1..$pass"
