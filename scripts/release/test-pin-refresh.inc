# Sourced by the release fixture suite: exercise the same-head recovery with real Git.
refresh_main="$(git -C "$P/deploy.git" rev-parse main)"
refresh_before="$(git -C "$P/deploy.git" rev-parse "$b1")"
git -C "$P/web.git" update-ref refs/heads/main "$h1"
echo 'security fix' >"$P/deploy/Cargo.lock"
g "$P/deploy" add Cargo.lock; g "$P/deploy" commit -q -m 'fix audited dependency'
g "$P/deploy" push -q "$P/deploy.git" main
refresh_main_new="$(git -C "$P/deploy.git" rev-parse main)"
refresh_fixture() {
  approved_stale_pr
  jq 'map(.base = {ref: "main", repo: {full_name: "jeryu/jeryu-deploy"}})' "$P/pulls.json" >"$P/pulls.next"
  mv "$P/pulls.next" "$P/pulls.json"
  echo "$failed_required" >"$P/checks.json"
}
refresh_unchanged() {
  auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "refresh skip failed"; }
  [[ "$(git -C "$P/deploy.git" rev-parse "$b1")" == "$refresh_before" ]] || fail "ineligible refresh pushed"
  grep -q 'skipping' "$P/run.log" || fail "refresh skip was silent"
}
for status in success pending missing running cancelled; do
  refresh_fixture
  jq -n --arg status "$status" '{checks:[{required:true,status:$status}]}' >"$P/checks.json"
  refresh_unchanged
done
refresh_fixture
echo '{"checks":[]}' >"$P/checks.json"
touch "$P/readback-failure"
refresh_unchanged
rm "$P/readback-failure"
refresh_fixture
cp "$P/state/opened.tsv" "$P/refresh-owned"
: >"$P/state/opened.tsv"
refresh_unchanged
cp "$P/refresh-owned" "$P/state/opened.tsv"
refresh_fixture
echo '{"entries":[{"number":60,"state":"building"}],"page":{"has_more":false}}' >"$P/queue.json"
refresh_unchanged
echo '{"entries":[],"page":{"has_more":false}}' >"$P/queue.json"
refresh_fixture
jq '.summary.head_sha = "0000000000000000000000000000000000000000"' "$P/detail.json" >"$P/after-checks-detail.json"
refresh_unchanged
mv "$P/after-checks-detail.json" "$P/after-checks-detail.saved"
for mutation in '.state = "closed"' '.head.repo.full_name = "someone/else"' '.base.ref = "release/stable"'; do
  refresh_fixture
  jq "map($mutation)" "$P/pulls.json" >"$P/pulls.next"
  mv "$P/pulls.next" "$P/pulls.json"
  refresh_unchanged
done
refresh_fixture
jq '.reviews = [{effective:true,state:"CHANGES_REQUESTED"}]' "$P/detail.json" >"$P/detail.next"
mv "$P/detail.next" "$P/detail.json"
refresh_unchanged
# A branch carrying anything beyond the pin must never be replayed automatically.
refresh_extra="$(g "$P/deploy.git" commit-tree -m unrelated "$refresh_main_new^{tree}" -p "$refresh_before")"
git -C "$P/deploy.git" update-ref "refs/heads/$b1" "$refresh_extra"
refresh_fixture
saved_before="$refresh_before"; refresh_before="$refresh_extra"
refresh_unchanged
refresh_before="$saved_before"
git -C "$P/deploy.git" update-ref "refs/heads/$b1" "$refresh_before"
refresh_fixture
touch "$P/refuse-refresh-push"
if auto_pin >"$P/run.log" 2>&1; then fail "a refused refresh push reported success"; fi
[[ "$(git -C "$P/deploy.git" rev-parse "$b1")" == "$refresh_before" ]] || fail "refused refresh changed remote"
rm "$P/refuse-refresh-push"
refresh_fixture
# An older decided pull request on the same branch, listed first, must not shadow the open bump.
jq '[.[0] + {number: 5, state: "closed"}] + .' "$P/pulls.json" >"$P/pulls.next"; mv "$P/pulls.next" "$P/pulls.json"
cp "$P/state/opened.tsv" "$P/refresh-owned"
builds_before="$(lines "$P/builds")"
auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "same-head refresh failed"; }
refresh_after="$(git -C "$P/deploy.git" rev-parse "$b1")"
[[ "$refresh_after" != "$refresh_before" ]] || { cat "$P/run.log" >&2; fail "failed same-head bump was not refreshed"; }
git -C "$P/deploy.git" merge-base --is-ancestor "$refresh_before" "$refresh_after" || fail "refresh rewrote history"
git -C "$P/deploy.git" merge-base --is-ancestor "$refresh_main_new" "$refresh_after" || fail "refresh omitted main"
[[ "$(git -C "$P/deploy.git" show "$refresh_after:Cargo.lock")" == 'security fix' ]] || fail "refresh lost audited lock fix"
[[ "$(git -C "$P/deploy.git" rev-parse "$refresh_after:jeryu-split.lock.toml")" == "$(git -C "$P/deploy.git" rev-parse "$refresh_before:jeryu-split.lock.toml")" ]] || fail "refresh changed the pin"
[[ "$(git -C "$P/deploy.git" diff --name-only "$refresh_main_new" "$refresh_after")" == jeryu-split.lock.toml ]] || fail "refresh included unrelated edits"
cmp "$P/refresh-owned" "$P/state/opened.tsv" || fail "refresh lost ownership"
[[ ! -s "$P/prs.jsonl" && ! -s "$P/closed.log" && "$(lines "$P/builds")" == "$builds_before" ]] || fail "refresh created a duplicate or rebuilt the bundle"
refresh_fixture
refresh_before="$refresh_after"
refresh_unchanged
# A gate that keeps failing is the bump's own: refreshes are spaced and capped.
refresh_main_commit() { # MESSAGE [FILE CONTENT]: one more commit on deploy main
  if [ -n "${2:-}" ]; then printf '%s\n' "$3" >"$P/deploy/$2"; chmod +x "$P/deploy/$2"; g "$P/deploy" add "$2"
  else echo "$1" >>"$P/deploy/Cargo.lock"; g "$P/deploy" add Cargo.lock; fi
  g "$P/deploy" commit -q -m "$1"; g "$P/deploy" push -q "$P/deploy.git" main
}
refresh_main_commit 'another main commit'
refresh_fixture
refresh_unchanged
grep -q 'next refresh after' "$P/run.log" || { cat "$P/run.log" >&2; fail "a second refresh inside the interval was not held back"; }
refresh_fixture
JERYU_AUTO_PIN_REFRESH_AFTER=0 auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "a due second refresh failed"; }
refresh_second="$(git -C "$P/deploy.git" rev-parse "$b1")"
[[ "$refresh_second" != "$refresh_after" ]] || { cat "$P/run.log" >&2; fail "a due second refresh did not push"; }
# Main changes the bundle recipe: the carried hash is stale, so the refresh rebuilds with main's.
dist_c="$(printf c%.0s {1..64})"
refresh_main_commit 'change the bundle recipe' scripts/release/build-web-dist.sh "#!/usr/bin/env bash
echo build >>\"$P/builds\"; echo \"\$2 $dist_c\""
refresh_fixture
builds_before="$(lines "$P/builds")"
JERYU_AUTO_PIN_REFRESH_AFTER=0 auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "a recipe-change refresh failed"; }
refresh_rebuilt="$(git -C "$P/deploy.git" rev-parse "$b1")"
refresh_recipe_main="$(git -C "$P/deploy.git" rev-parse main)"
[[ "$refresh_rebuilt" != "$refresh_second" && "$(lines "$P/builds")" == $((builds_before + 1)) ]] \
  || { cat "$P/run.log" >&2; fail "a recipe change on main did not rebuild the bundle"; }
git -C "$P/deploy.git" merge-base --is-ancestor "$refresh_second" "$refresh_rebuilt" || fail "the rebuilt refresh rewrote history"
grep -qx "web_dist_sha256 = \"$dist_c\"" <(git -C "$P/deploy.git" show "$refresh_rebuilt:jeryu-split.lock.toml") \
  || fail "the refresh kept a hash built by an older recipe"
[[ "$(git -C "$P/deploy.git" diff --name-only "$refresh_recipe_main" "$refresh_rebuilt")" == jeryu-split.lock.toml ]] \
  || fail "the rebuilt refresh is not main plus the pin"
# The cap: a fourth refresh asks for a person instead of pushing again.
refresh_main_commit 'yet another main commit'
refresh_fixture; : >"$P/events.jsonl"
JERYU_AUTO_PIN_REFRESH_AFTER=0 auto_pin >"$P/run.log" 2>&1 || { cat "$P/run.log" >&2; fail "an exhausted refresh failed the tick"; }
[[ "$(git -C "$P/deploy.git" rev-parse "$b1")" == "$refresh_rebuilt" ]] || fail "a bump past its refresh cap was pushed again"
jq -es 'any(.[]; .kind == "pin.bump_failed" and .needs_human == true and .pr == 60)' "$P/events.jsonl" >/dev/null \
  || fail "an exhausted refresh did not ask for a person"
rm -rf "$P/state/refresh"; : >"$P/events.jsonl"
# Restore the surrounding suite's histories and observations.
git -C "$P/deploy.git" update-ref "refs/heads/$b1" "$refresh_after^1"
git -C "$P/deploy.git" update-ref refs/heads/main "$refresh_main"
git -C "$P/deploy" checkout -q -B main "$refresh_main"
git -C "$P/web.git" update-ref refs/heads/main "$h2"
undo_bump
ok "same-head failed bumps incorporate main without rewriting history or losing ownership, rebuild on a recipe change, and back off"
