#!/usr/bin/env bash
# test-jankurai-audit-heartbeat.sh — drive ops/ci/jankurai-audit-heartbeat.sh (the jankurai audit
# runner's heartbeat) against a stand-in curl, hostname and auditor binary: the exact beat shapes
# (idle, auditing, finished), the result kept between runs, `code` and `tools`, the one retry
# without them on a 422, and that no failure of the beat ever fails the run. No service, network
# or credential.
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

mkdir -p "$T/bin" "$T/install"
# curl: record the arguments and the body, answer with the next queued status (default 200);
# "down" answers like an unreachable forge.
cat >"$T/bin/curl" <<STUB
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$T/curl-args"
cat >>"$T/beats.jsonl"
status=200
if [ -s "$T/statuses" ]; then status="\$(head -n 1 "$T/statuses")"; sed -i 1d "$T/statuses"; fi
[ "\$status" = down ] && { printf 000; exit 7; }
printf '%s' "\$status"
STUB
printf '#!/usr/bin/env bash\necho gate-a\n' >"$T/bin/hostname"
printf '#!/usr/bin/env bash\necho "jankurai 1.6.11"\n' >"$T/jankurai"
chmod +x "$T/bin/curl" "$T/bin/hostname" "$T/jankurai"
digest="$(sha256sum "$T/jankurai" | awk '{print $1}')"
commit=0123456789abcdef0123456789abcdef01234567
printf '%s\n' "$commit" >"$T/install/VERSION"
printf 'header = "Authorization: Bearer test-token"\n' >"$T/auth.conf"

# run <script>: source the heartbeat as the runner does (set -euo pipefail) and run the script.
# shellcheck disable=SC2016 # $1 and $2 are the inner shell's
run() {
  : >"$T/beats.jsonl"; : >"$T/curl-args"
  env PATH="$T/bin:$PATH" API=https://forge.invalid AUTH_CONFIG="$T/auth.conf" ROOT="$T/install" \
    JERYU_GOVERNED_JANKURAI_BIN="$T/jankurai" JERYU_AUDIT_STATE_DIR="$T/state" \
    bash -c 'set -euo pipefail; source "$1"; eval "$2"; echo "run finished"' _ \
    "$here/jankurai-audit-heartbeat.sh" "$1" >"$T/out" 2>"$T/err"
}
beat() { sed -n "${1}p" "$T/beats.jsonl"; }
posts() { grep -c . "$T/beats.jsonl" || true; }

run 'audit_beat_init; audit_beat' || fail "an idle beat failed the run: $(cat "$T/err")"
[ "$(posts)" = 1 ] || fail "an idle run must post exactly one beat, posted $(posts)"
jq -e --arg d "$digest" '. == {runnerId: "gate-a/jankurai-audit", host: "gate-a", slot: 0,
    labels: ["jankurai-audit"], intervalSeconds: 30,
    tools: [{name: "jankurai", version: "1.6.11", sha256: $d}]}' <<<"$(beat 1)" >/dev/null \
  || fail "idle beat shape: $(beat 1)"
grep -q -- "https://forge.invalid/api/v1/runners/heartbeat" "$T/curl-args" || fail "beat went elsewhere"
grep -q -- "--config $T/auth.conf" "$T/curl-args" || fail "the token did not go through the config file"
grep -q -- "--max-time 5" "$T/curl-args" || fail "the beat is not time-boxed"
grep -q "test-token" "$T/curl-args" && fail "the token reached curl's arguments"
ok "an idle run beats once: who, the kind, a 30 s interval and the measured auditor; no code without a repo"

run 'export JERYU_AUDIT_RUNNER_REPO=acme/gate-scripts; audit_beat_init; audit_beat' || fail "beat with code failed"
jq -e --arg c "$commit" '.code.repo == "acme/gate-scripts" and .code.commit == $c
    and (.code.installedAt | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z$"))
    and (.code | keys | length) == 3' <<<"$(beat 1)" >/dev/null || fail "code shape: $(beat 1)"
mv "$T/install/VERSION" "$T/VERSION.kept"
run 'export JERYU_AUDIT_RUNNER_REPO=acme/gate-scripts; audit_beat_init; audit_beat' || fail "beat without VERSION failed"
jq -e 'has("code") | not' <<<"$(beat 1)" >/dev/null || fail "code sent with no VERSION: $(beat 1)"
mv "$T/VERSION.kept" "$T/install/VERSION"
ok "code names JERYU_AUDIT_RUNNER_REPO at the installed VERSION commit, and is left out without either"

sha=89abcdef0123456789abcdef0123456789abcdef
run "audit_beat_init; audit_beat_start acme/widgets $sha; audit_beat_finish acme/widgets $sha scored 41" \
  || fail "an audit's beats failed the run: $(cat "$T/err")"
[ "$(posts)" = 2 ] || fail "an audit must beat at its start and its end, posted $(posts)"
jq -e --arg s "$sha" '.current.repo == "acme/widgets" and .current.sha == $s
    and .current.recipe == "jankurai audit" and (.current.startedAt | endswith("Z"))
    and (.current | has("pr") | not) and (has("last") | not)' <<<"$(beat 1)" >/dev/null \
  || fail "start beat shape: $(beat 1)"
jq -e --arg s "$sha" '(has("current") | not) and .last.repo == "acme/widgets" and .last.sha == $s
    and .last.recipe == "jankurai audit" and .last.conclusion == "scored" and .last.seconds == 41
    and (.last.finishedAt | endswith("Z")) and (.last | has("reason") | not)' <<<"$(beat 2)" >/dev/null \
  || fail "finish beat shape: $(beat 2)"
last="$(jq -c .last <<<"$(beat 2)")"
run 'audit_beat_init; audit_beat' || fail "idle beat after an audit failed"
[ "$(jq -c .last <<<"$(beat 1)")" = "$last" ] || fail "an idle run changed the last result: $(beat 1)"
ok "an audit beats current then last; the next idle run re-sends that last unchanged"

run "audit_beat_init; audit_beat_start acme/widgets $sha; audit_beat_finish acme/widgets $sha refused 3 'the forge refused the report'" \
  || fail "a refused audit failed the run"
jq -e '.last.conclusion == "refused" and .last.reason == "the forge refused the report"' <<<"$(beat 2)" >/dev/null \
  || fail "reason not carried: $(beat 2)"
ok "a result can carry the runner's reason"

printf '422\n200\n' >"$T/statuses"
run 'export JERYU_AUDIT_RUNNER_REPO=acme/gate-scripts; audit_beat_init; audit_beat' || fail "a 422 failed the run"
[ "$(posts)" = 2 ] || fail "a 422 must be retried exactly once, posted $(posts)"
jq -e 'has("code") and has("tools")' <<<"$(beat 1)" >/dev/null || fail "first try lacked code/tools"
jq -e '(has("code") or has("tools")) | not' <<<"$(beat 2)" >/dev/null || fail "retry still sent code/tools: $(beat 2)"
jq -e '.last.conclusion == "refused"' <<<"$(beat 2)" >/dev/null || fail "retry dropped more than code/tools"
printf '422\n422\n422\n' >"$T/statuses"
run 'export JERYU_AUDIT_RUNNER_REPO=acme/gate-scripts; audit_beat_init; audit_beat' || fail "two 422s failed the run"
[ "$(posts)" = 2 ] || fail "a second 422 was retried again"
grep -q "not accepted (HTTP 422; ignored)" "$T/err" || fail "a refused beat was not logged: $(cat "$T/err")"
: >"$T/statuses"
ok "a 422 (an older forge) is retried once without code and tools, and never more"

printf 'down\n' >"$T/statuses"
run "audit_beat_init; audit_beat" || fail "an unreachable forge failed the run"
grep -q "run finished" "$T/out" || fail "the run stopped after a failed beat"
grep -q "not accepted (HTTP 000; ignored)" "$T/err" || fail "an unreachable forge was not logged"
run 'export JERYU_AUDIT_HEARTBEAT=0; audit_beat_init; audit_beat; audit_beat_start acme/widgets 0123456; audit_beat_finish acme/widgets 0123456 failed 1' \
  || fail "a disabled heartbeat failed the run"
[ "$(posts)" = 0 ] || fail "JERYU_AUDIT_HEARTBEAT=0 still posted"
rm -f "$T/install/VERSION"; printf '#!/usr/bin/env bash\nexit 1\n' >"$T/bin/hostname"
run 'audit_beat_init; audit_beat' || fail "no hostname failed the run"
[ "$(posts)" = 0 ] || fail "a beat without a host name was sent"
printf '#!/usr/bin/env bash\necho gate-a\n' >"$T/bin/hostname"; printf '%s\n' "$commit" >"$T/install/VERSION"
ok "an unreachable forge, a disabled heartbeat or no host name never fail the run"

run "export JERYU_AUDIT_BEAT_EVERY=1; audit_beat_init; audit_beat_start acme/widgets $sha; sleep 2.5;
     audit_beat_finish acme/widgets $sha scored 2; n=\$(grep -c . \"$T/beats.jsonl\"); sleep 1.5;
     [ \"\$(grep -c . \"$T/beats.jsonl\")\" = \"\$n\" ] || { echo 'keepalive outlived the audit' >&2; exit 1; }" \
  || fail "keepalive: $(cat "$T/err")"
[ "$(jq -s '[.[] | select(has("current"))] | length' "$T/beats.jsonl")" -ge 3 ] \
  || fail "a running audit did not keep beating: $(cat "$T/beats.jsonl")"
jq -e 'has("current") | not' <<<"$(tail -n 1 "$T/beats.jsonl")" >/dev/null || fail "the last beat still shows the audit running"
ok "a running audit keeps beating, and the keepalive stops when it ends"

# The runner wires it in, and every conclusion it can send is one the forge accepts.
runner="$here/jankurai-audit-runner.sh"
grep -qF "source \"\${ROOT}/ops/ci/jankurai-audit-heartbeat.sh\"" "$runner" || fail "the runner does not source the heartbeat"
grep -q 'audit_beat_stop_keepalive' "$runner" || fail "the runner's exit does not stop the keepalive"
accepted="$(grep -o 'JANKURAI_AUDIT_CONCLUSIONS: &\[&str\] = &\[[^]]*\]' \
  "$repo/crates/jeryu-api/src/web/control_plane/gate_runners.rs")" || fail "the forge's audit conclusions moved"
for conclusion in scored tool-failed refused failed; do
  grep -Eq "(conclusion=|\" )${conclusion}( |\"|$)" "$runner" || fail "the runner never sends $conclusion"
  [[ "$accepted" == *"\"$conclusion\""* ]] || fail "the forge does not accept $conclusion"
done
ok "the runner sources the heartbeat, and sends only conclusions the forge accepts"
echo "1..$pass"
