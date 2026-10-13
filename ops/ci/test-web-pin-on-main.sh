#!/usr/bin/env bash
# Proves ops/ci/web-pin-on-main.sh passes a pin on jeryu-web main and refuses one
# on a side branch, one the repository does not have, and a malformed one,
# against a throwaway jeryu-web. No network.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/web-pin-test.XXXXXX")"
trap 'rm -rf -- "$work"' EXIT
fail() { echo "test-web-pin-on-main: $*" >&2; exit 1; }

git init -q --bare "$work/jeryu-web.git"
git init -q -b main "$work/src"
commit() { # message -> sha
  git -C "$work/src" -c user.name=t -c user.email=t@example.invalid \
    commit -q --allow-empty -m "$1"
  git -C "$work/src" rev-parse HEAD
}
on_main="$(commit "on main")"
git -C "$work/src" push -q "$work/jeryu-web.git" main
git -C "$work/src" switch -q -c side
side="$(commit "never merged")"
git -C "$work/src" push -q "$work/jeryu-web.git" side
git -C "$work/src" switch -q main
newer="$(commit "main moves on")"
git -C "$work/src" push -q "$work/jeryu-web.git" main

check() { # pin -> exit status of the check, output in $work/out
  printf '[[repo]]\nname = "jeryu-other"\ncommit = "%s"\n\n[[repo]]\nname = "jeryu-web"\ncommit = "%s"\n' \
    "$side" "$1" >"$work/lock.toml"
  JERYU_SPLIT_LOCK="$work/lock.toml" JERYU_WEB_REMOTE="${REMOTE:-$work/jeryu-web.git}" \
    JERYU_WEB_PIN_CACHE="$work/cache.git" bash "$root/ops/ci/web-pin-on-main.sh" >"$work/out" 2>&1
}

check "$on_main" || fail "refused a pin that is an ancestor of main: $(cat "$work/out")"
check "$newer" || fail "refused main's head: $(cat "$work/out")"
# The non-ancestor exists in the cache: rejection must prove ancestry,
# rather than merely reject an object that the main-only fetch omitted.
git -C "$work/cache.git" fetch -q "$work/jeryu-web.git" side:refs/heads/side
git -C "$work/cache.git" cat-file -e "$side^{commit}"
if check "$side"; then fail "accepted a pin on a side branch"; fi
grep -q "is not on jeryu-web main" "$work/out" || fail "side-branch refusal did not say why: $(cat "$work/out")"
if check "0123456789abcdef0123456789abcdef01234567"; then fail "accepted an unknown commit"; fi
grep -q "absent from the fetched main history" "$work/out" || fail "missing object was not distinguished from a non-ancestor"
if check "not-a-sha"; then fail "accepted a malformed pin"; fi
grep -q "no 40-hex jeryu-web commit" "$work/out" || fail "malformed refusal did not say why"
if REMOTE="$work/missing.git" check "$on_main"; then fail "passed without reaching jeryu-web"; fi
grep -q "could not fetch" "$work/out" || fail "fetch refusal did not say why"
mkdir "$work/invalid-cache"
if JERYU_SPLIT_LOCK="$work/lock.toml" JERYU_WEB_REMOTE="$work/jeryu-web.git" \
    JERYU_WEB_PIN_CACHE="$work/invalid-cache" bash "$root/ops/ci/web-pin-on-main.sh" >"$work/out" 2>&1; then
  fail "accepted a non-repository cache"
fi
grep -q "invalid local cache" "$work/out" || fail "invalid cache was misreported as a remote failure"

echo "web-pin-on-main ok: only a pin on jeryu-web main passes"
