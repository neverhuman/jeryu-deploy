#!/usr/bin/env bash
# test-release-board.sh — run the real collect.sh and lib.sh against a throwaway forge: a local
# bare repo served over file://, a stand-in queue command that lists seven todos (one per work
# category), and a stand-in curl that records the PUT. A demo adapter in a throwaway families
# directory uses the same library calls a site's adapters do. No service, network or credential.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
T="$(mktemp -d)"
trap '[ -n "${KEEP:-}" ] || rm -rf "$T"' EXIT
pass=0
ok() { pass=$((pass + 1)); echo "ok $pass - $1"; }
fail() { echo "not ok - $1" >&2; exit 1; }

export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.invalid GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.invalid
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1

# The collector under test; the demo adapter lives where a site's would, outside the scripts.
mkdir -p "$T/rb" "$T/families" "$T/bin" "$T/forge/git/demo" "$T/out"
cp "$here/collect.sh" "$here/lib.sh" "$T/rb/"
cat >"$T/families/demo.sh" <<'EOF'
collect_demo() {
  local m main prod
  m="$(mirror demo/app)" || return 1
  main="$(git -C "$m" rev-parse refs/heads/main)"
  prod="$(cat "$DEMO_PROD")"
  lane app "App" "demo/app" demo false \
    "$(stage id=main name=main version="$(short "$main")" state=none status=source known=derived)" \
    "$(stage id=prod name=prod version="$(short "$prod")" state=warn status="$(behind "$m" "$prod" "$main") behind" known=host \
        targets="[$(target node-a "$(short "$prod")" ok),$(target node-b old bad)]" human_only=true promote_cmd="tag it" \
        ships="$(git -C "$m" log --format='%h · %s' "$prod..$main" | lines_json)" forge_repo=demo/app forge_env=production)"
  problem "node-b" "ssh timed out"
  main_specs="$m=$main"
  work_json="$(work_summary demo "by trailer, then content" "" "$m=$prod")"
  summary="demo board"
}
EOF

# A repo whose history holds one todo of each kind:
#   t-live     on main, before the prod commit
#   t-merged   on main, after it
#   t-content  on a side branch, and its change re-landed on main without the trailer
#   t-stranded on a side branch only
#   t-none     done, but no commit carries its trailer
#   t-blocked, t-open by status
src="$T/src"
git init -q -b main "$src"
commit() { printf '%s\n' "$1" >"$src/$1.txt"; git -C "$src" add "$1.txt"; git -C "$src" commit -q -m "$2"; }
commit base "base"
commit live $'live change\n\nTodo: t-live'
git -C "$src" rev-parse HEAD >"$T/prod"
commit merged $'merged change\n\nTodo: t-merged'
git -C "$src" checkout -q -b side
commit content $'content change\n\nTodo: t-content'
commit stranded $'stranded change\n\nTodo: t-stranded'
git -C "$src" checkout -q main
printf 'content\n' >"$src/content.txt"; git -C "$src" add content.txt; git -C "$src" commit -q -m "re-land the content change by tree"
git clone -q --bare "$src" "$T/forge/git/demo/app.git"
export DEMO_PROD="$T/prod"

cat >"$T/bin/queue" <<'EOF'
#!/usr/bin/env bash
cat <<'LIST'
t-live      done     now p2 app  live
t-merged    done     now p2 app  merged
t-content   done     now p2 app  content
t-stranded  done     now p2 app  stranded
t-none      done     now p2 app  none
t-blocked   blocked  now p2 app  blocked
t-open      open     now p2 app  open
LIST
EOF
# curl: record a PUT's body and URL and answer 200; anything else fails.
cat >"$T/bin/curl" <<EOF
#!/usr/bin/env bash
url="" body=""
while [ \$# -gt 0 ]; do
  case "\$1" in --data-binary) body="\${2#@}"; shift ;; http*|file*) url="\$1" ;; esac
  shift
done
[ -n "\$body" ] || exit 22
cp "\$body" "$T/put.json"; printf '%s\n' "\$url" >"$T/put.url"
printf '{"family":"demo"}\n200'
EOF
chmod +x "$T/bin/queue" "$T/bin/curl"
export PATH="$T/bin:$PATH" JERYU_BASE="file://$T/forge" JERYU_RELEASE_BOARD_STATE="$T/state"
export JERYU_RELEASE_BOARD_FAMILIES="$T/families" JERYU_BOARD_QUEUE_CMD=queue
printf 'secret-token\n' >"$T/token"
export JERYU_BOARD_TOKEN_FILE="$T/token"

bash "$T/rb/collect.sh" --out "$T/out" demo 2>"$T/log" || fail "collect.sh demo exited non-zero: $(cat "$T/log")"
b="$T/out/demo.json"
jq -e '.schema == "jeryu.release_board.v1" and .family == "demo" and (.lanes | length) == 1' "$b" >/dev/null \
  || fail "the board has the v1 shape"
ok "collect.sh writes a v1 board for the family"
jq -e '.lanes[0].stages[1] | .forge == {repo: "demo/app", environment: "production"} and .promote.human_only == true
  and (.targets | map(.state)) == ["ok", "bad"] and (.ships | length) == 2' "$b" >/dev/null \
  || fail "the prod stage carries its binding, promote, targets and ships"
ok "a stage carries its forge binding, promote action, targets and what promoting ships"
jq -e '.problems == [{source: "node-b", message: "ssh timed out"}]' "$b" >/dev/null || fail "problems are recorded"
ok "an unreadable source becomes one problem line"
parts="$(jq -c '[.work.parts[] | {(.key): .count}] | add' "$b")"
[ "$parts" = '{"live":1,"merged":2,"stranded":1,"untraceable":1,"blocked":1,"open":1}' ] || fail "work parts: $parts"
ok "the work bar places each todo once (content re-landed on main counts as merged)"
jq -e '.work.total == 7' "$b" >/dev/null || fail "work total"
ok "the work total is every todo in the queue"

bash "$T/rb/collect.sh" --push --trigger release --out "$T/out" demo 2>"$T/log" || fail "push exited non-zero: $(cat "$T/log")"
[ "$(cat "$T/put.url")" = "file://$T/forge/api/v1/release-board/demo" ] || fail "PUT went to $(cat "$T/put.url")"
jq -e '.collector.trigger == "release"' "$T/put.json" >/dev/null || fail "the pushed board names its trigger"
ok "--push PUTs the board to /api/v1/release-board/<family> with its trigger"
grep -q secret-token "$T/out/demo.json" && fail "the token leaked into the board"
ok "the token never reaches the board"

rm -f "$T/put.json"
if JERYU_BOARD_TOKEN_FILE="$T/missing" bash "$T/rb/collect.sh" --push --out "$T/out" demo 2>/dev/null; then
  fail "a push without a token must fail"
fi
[ ! -e "$T/put.json" ] || fail "nothing is sent without a token"
ok "without a token the board is written but not pushed, and the run says so"

bash "$T/rb/collect.sh" --out "$T/all" all 2>"$T/log" || fail "collect.sh all exited non-zero: $(cat "$T/log")"
[ -s "$T/all/demo.json" ] || fail "all did not collect the demo family"
ok "all collects every adapter in the families directory"

if JERYU_BASE="" bash "$T/rb/collect.sh" --out "$T/out" demo 2>"$T/log"; then fail "a run without JERYU_BASE must fail"; fi
grep -q "JERYU_BASE is not set" "$T/log" || fail "the refusal names JERYU_BASE: $(cat "$T/log")"
ok "the forge's URL is configuration: without JERYU_BASE the run refuses and says why"

# The shipped example must stay a working template, and site adapters stay out of the repo.
bash -n "$here/examples/acme.sh" || fail "examples/acme.sh does not parse"
[ ! -d "$here/families" ] || fail "scripts/release-board/families/ is back: site adapters belong in host config"
ok "the example adapter parses, and no site adapter lives in the repository"
echo "1..$pass"
