#!/usr/bin/env bash
# Exercise the required verifier against synthetic git history and native API
# responses. No network, service, production credential or checkout mutation.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mkdir -p "$root/target/ci"
work="$(mktemp -d "$root/target/ci/web-proof-test.XXXXXX")"
trap 'rm -rf -- "$work"' EXIT
fail() { echo "test-web-pin-proof: $*" >&2; exit 1; }
g() { git -C "$1" -c user.name=t -c user.email=t@example.invalid "${@:2}"; }
mkdir -p "$work/web" "$work/deploy/ops/ci" "$work/deploy/scripts/release" "$work/bin"
g "$work/web" init -q -b main
g "$work/web" commit -q --allow-empty -m first
first="$(g "$work/web" rev-parse HEAD)"
g "$work/web" commit -q --allow-empty -m second
second="$(g "$work/web" rev-parse HEAD)"
git clone -q --bare "$work/web" "$work/web.git"
cp "$root/ops/ci/"{web-pin-proof,web-pin-on-main}.sh "$work/deploy/ops/ci/"
cp "$root/scripts/release/latest-statuses.jq" "$work/deploy/scripts/release/"
lock() {
  printf '[[repo]]\nname = "jeryu-web"\ncommit = "%s"\nrequired_check = "jeryu-web/required"\n' "$1" >"$work/deploy/jeryu-split.lock.toml"
}
g "$work/deploy" init -q -b main
lock "$first"
g "$work/deploy" add .; g "$work/deploy" commit -q -m base
base="$(g "$work/deploy" rev-parse HEAD)"
g "$work/deploy" update-ref refs/remotes/origin/main "$base"
lock "$second"
g "$work/deploy" commit -qam pin
cat >"$work/bin/curl" <<'CURL'
#!/usr/bin/env bash
cat >/dev/null
[[ "$*" != *"test-token"* && "${!#}" == "https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-web/commits/"*"/status" ]] || exit 95
[[ ! -e "$FIXTURE/refuse" ]] || exit 22
cat "$FIXTURE/status.json"
CURL
chmod +x "$work/bin/curl"
printf 'test-token\n' >"$work/token"; chmod 600 "$work/token"
export FIXTURE="$work"
status() { # latest state, combined state, sha
  jq -n --arg state "$1" --arg combined "${2:-success}" --arg sha "${3:-$second}" '
    {sha: $sha, state: $combined, statuses: [
      {context:"jeryu-web/required", state:$state, updated_at:"2026-10-15T16:40:22.1Z", id:"latest"},
      {context:"jeryu-web/required", state:"pending", updated_at:"2026-10-15T16:40:22Z", id:"prior"}]}' >"$work/status.json"
}
check() {
  PATH="$work/bin:$PATH" JERYU_WEB_REMOTE="$work/web.git" JERYU_WEB_PIN_TOKEN_FILE="$work/token" \
    bash "$work/deploy/ops/ci/web-pin-proof.sh" >"$work/out" 2>&1
}
refuse() { if check; then fail "accepted $1"; fi; }
status success
check || fail "refused valid forward pin: $(cat "$work/out")"
grep -q '"id":"latest"' "$work/out" || fail "did not report the latest result"
for state in pending failure error; do
  status "$state"
  jq '.statuses[1].state = "success"' "$work/status.json" >"$work/next"; mv "$work/next" "$work/status.json"
  refuse "$state required result after older success"
done
status success pending; refuse "pending combined status"
status success success "$first"; refuse "status from another commit"
status success
jq '.statuses += [{context:"jeryu-web/required",state:"failure",updated_at:"2026-10-15T16:40:22.100000000Z"}]' "$work/status.json" >"$work/next"
mv "$work/next" "$work/status.json"; refuse "conflicting latest timestamps"
for mutation in 'del(.statuses)' '.statuses = []' '.statuses |= map(.context = "other")' '.statuses[0].updated_at = "bad"'; do
  status success; jq "$mutation" "$work/status.json" >"$work/next"; mv "$work/next" "$work/status.json"
  refuse "$mutation"
done
status success; touch "$work/refuse"; refuse "unavailable native status"; rm "$work/refuse"
chmod 644 "$work/token"; refuse "loose credential file"; chmod 600 "$work/token"
status success
# Main itself must compare to its parent, not to itself.
g "$work/deploy" update-ref refs/remotes/origin/main HEAD
check || fail "main pin did not compare to parent: $(cat "$work/out")"
# A child rolling back to first is still on web main, but must fail direction.
lock "$first"; g "$work/deploy" commit -qam rollback
status success success "$first"
refuse "backward pin on a PR"
grep -q 'does not descend' "$work/out" || fail "rollback failed for the wrong reason"
g "$work/deploy" update-ref refs/remotes/origin/main HEAD
refuse "backward pin on main"
g "$work/deploy" update-ref -d refs/remotes/origin/main
refuse "missing protected base"
echo "web-pin-proof ok: forward/main pins, exact/latest status, ambiguity, credentials and failures"
