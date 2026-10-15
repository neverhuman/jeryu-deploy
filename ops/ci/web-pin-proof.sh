#!/usr/bin/env bash
# Required proof: reviewed web main ancestry, forward movement from Deploy's
# protected base, and the exact pin's latest native required status. No bypass.
# Uses the same read credential as git, or a mode-0600 JERYU_WEB_PIN_TOKEN_FILE.
set -euo pipefail
set +x
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
fail() { echo "web pin proof: $*" >&2; exit 1; }
field() {
  awk -v key="$1" '
    /^\[\[repo\]\]/ { inweb = 0 }
    /^name = "jeryu-web"$/ { inweb = 1 }
    inweb && $1 == key && $2 == "=" { n++; v = $3; gsub(/"/, "", v) }
    END { if (n == 1) print v; else exit 1 }'
}
# CI fetches origin/main before evaluating a head, also used by jankurai-gate.
# A queue replay therefore compares to the queue base; a normal PR to its merge
# base. Main itself compares to its parent so a landed bump is verified again.
base="$(git merge-base HEAD refs/remotes/origin/main)" || fail "fetch origin/main first"
if [[ "$base" == "$(git rev-parse HEAD)" ]]; then
  base="$(git rev-parse 'HEAD^')" || fail "cannot resolve previous Deploy commit"
fi
previous="$(git show "$base:jeryu-split.lock.toml" | field commit)" || fail "base has no unique web pin"
pin="$(field commit < jeryu-split.lock.toml)" || fail "lock has no unique web pin"
required="$(field required_check < jeryu-split.lock.toml)" || fail "lock has no required check"
[[ "$previous" =~ ^[0-9a-f]{40}$ && "$pin" =~ ^[0-9a-f]{40}$ ]] || fail "invalid commit"
[[ "$required" == jeryu-web/required ]] || fail "unexpected required check"
# Do not allow the ancestry helper to read a different lock or cache.
JERYU_SPLIT_LOCK="$root/jeryu-split.lock.toml" JERYU_WEB_PIN_CACHE="$root/target/ci/jeryu-web.git" \
  bash "$root/ops/ci/web-pin-on-main.sh"
git -C target/ci/jeryu-web.git merge-base --is-ancestor "$previous" "$pin" \
  || fail "$pin does not descend from base pin $previous; use the release rollback procedure"

# Always use this canonical API origin, even when a test/local mirror supplies
# git objects. Credentials never enter argv, artifacts, or redirected requests.
token_file="${JERYU_WEB_PIN_TOKEN_FILE:-}"
if [[ -n "$token_file" ]]; then
  [[ -f "$token_file" && ! -L "$token_file" && "$(stat -c '%a' "$token_file")" == 600 ]] \
    || fail "JERYU_WEB_PIN_TOKEN_FILE must be a regular mode-0600 file"
  token="$(cat "$token_file")"
else
  credential="$(printf 'protocol=https\nhost=git.neverhuman.org\npath=git/jeryu/jeryu-web.git\n\n' \
    | GIT_TERMINAL_PROMPT=0 git credential fill)" || fail "cannot obtain read credential"
  token="$(sed -n 's/^password=//p' <<<"$credential")"
  unset credential
fi
[[ "$token" =~ ^[A-Za-z0-9._~+/-]+=*$ ]] || fail "invalid read credential"
url="https://git.neverhuman.org/api/v3/repos/jeryu/jeryu-web/commits/$pin/status"
status="$(printf 'header = "Authorization: Bearer %s"\n' "$token" | \
  curl --disable --silent --show-error --fail --proto '=https' --max-time 60 --config - \
    -H 'Accept: application/json' "$url")" || fail "native status unavailable"
unset token
result="$(jq -ce -L "$root/scripts/release" --arg pin "$pin" --arg required "$required" '
  include "latest-statuses";
  select(.sha == $pin and .state == "success")
  | latest_statuses | map(select(.context == $required))
  | select(length == 1 and .[0].state == "success") | .[0]
  | {context, state, id, updated_at, created_at}
' <<<"$status")" || fail "$required is not unambiguously successful at $pin"
printf 'web pin proof: base=%s previous=%s pin=%s\n' "$base" "$previous" "$pin"
printf 'web pin proof: %s\nweb pin proof: %s\n' "$url" "$result"
