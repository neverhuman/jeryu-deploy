#!/usr/bin/env bash
# Proves ops/site-specifics/scan.sh: it refuses to run without a terms file,
# finds every term case-insensitively, reports a per-path count, and in CI mode
# fails only on matches an allowlist does not cover.
#
# The terms used here are invented (acme, globex, node-a): no site data is in
# this repository, including its tests.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scan="${root}/ops/site-specifics/scan.sh"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fail() { printf 'test-scan: %s\n' "$*" >&2; exit 1; }

terms="${work}/site-terms"
cat > "$terms" <<'TERMS'
# invented terms, as a site's own file would look
acme
globex
node-[a-z]
TERMS

repo="${work}/checkout"
mkdir -p "${repo}/docs" "${repo}/tests"
printf 'host = "node-a"\nowner = "ACME"\n' > "${repo}/config.toml"
printf 'deploys to Globex\n' > "${repo}/docs/guide.md"
printf 'nothing to see\n' > "${repo}/tests/clean.txt"
git -C "$repo" init -q
git -C "$repo" add -A
git -C "$repo" -c user.email=t@example -c user.name=t commit -qm init

# 1. No terms file: refuse, and say which variable is missing.
if out="$(env -u JERYU_SITE_TERMS_FILE "$scan" "$repo" 2>&1)"; then
  fail "scanned with no terms file"
fi
case "$out" in
  *JERYU_SITE_TERMS_FILE*) ;;
  *) fail "refusal did not name the variable: ${out}" ;;
esac

# A terms file that does not exist is also a refusal, not an empty scan.
if JERYU_SITE_TERMS_FILE="${work}/absent" "$scan" "$repo" >/dev/null 2>&1; then
  fail "scanned with a terms file that does not exist"
fi

# 2. Every term is found, case-insensitively, as file:line:term.
out="$(JERYU_SITE_TERMS_FILE="$terms" "$scan" "$repo")"
for expected in 'config.toml:1:node-[a-z]' 'config.toml:2:acme' 'docs/guide.md:1:globex'; do
  case "$out" in
    *"$expected"*) ;;
    *) fail "missing match ${expected} in: ${out}" ;;
  esac
done
case "$out" in
  *clean.txt*) fail "reported a file with no term: ${out}" ;;
esac
case "$out" in
  *'checkout: 3 matches in 2 files (3 scanned), 3 not allowlisted'*) ;;
  *) fail "wrong count line in: ${out}" ;;
esac

# The matched line is never printed: it can be site data.
case "$out" in
  *'owner = '*) fail "printed the matched line: ${out}" ;;
esac

# 3. --quiet prints the count without the matches.
out="$(JERYU_SITE_TERMS_FILE="$terms" "$scan" --quiet "$repo")"
case "$out" in
  *config.toml*) fail "--quiet printed a match: ${out}" ;;
esac
case "$out" in
  *'3 matches'*) ;;
  *) fail "--quiet dropped the count: ${out}" ;;
esac

# 4. CI mode fails on any match with no allowlist.
if JERYU_SITE_TERMS_FILE="$terms" "$scan" --ci "$repo" >/dev/null 2>&1; then
  fail "--ci passed with unallowlisted matches"
fi

# 5. An allowlist covers a whole path, or one term in one path.
allow="${work}/site-allow"
cat > "$allow" <<'ALLOW'
# a path, and a single term within a path
docs/*
config.toml:acme
ALLOW
out="$(JERYU_SITE_TERMS_FILE="$terms" JERYU_SITE_ALLOW_FILE="$allow" "$scan" "$repo")"
case "$out" in
  *'config.toml:1:node-[a-z]'*) ;;
  *) fail "allowlist hid a match it does not cover: ${out}" ;;
esac
case "$out" in
  *'config.toml:2:acme'*) fail "term allowlist did not cover its match: ${out}" ;;
  *'docs/guide.md'*) fail "path allowlist did not cover its match: ${out}" ;;
esac
case "$out" in
  *'3 matches in 2 files (3 scanned), 1 not allowlisted'*) ;;
  *) fail "wrong count line with an allowlist: ${out}" ;;
esac
if JERYU_SITE_TERMS_FILE="$terms" JERYU_SITE_ALLOW_FILE="$allow" "$scan" --ci "$repo" >/dev/null 2>&1; then
  fail "--ci passed over a match the allowlist does not cover"
fi

# 6. Fully allowlisted: CI mode passes.
printf 'config.toml\n' >> "$allow"
JERYU_SITE_TERMS_FILE="$terms" JERYU_SITE_ALLOW_FILE="$allow" "$scan" --ci "$repo" >/dev/null

# An allowlist file that does not exist is a refusal, so a typo cannot pass CI.
if JERYU_SITE_TERMS_FILE="$terms" JERYU_SITE_ALLOW_FILE="${work}/absent" "$scan" --ci "$repo" >/dev/null 2>&1; then
  fail "accepted an allowlist file that does not exist"
fi

# 7. Untracked files are not published, so they are not scanned.
printf 'node-b\n' > "${repo}/untracked.txt"
out="$(JERYU_SITE_TERMS_FILE="$terms" "$scan" "$repo")"
case "$out" in
  *untracked.txt*) fail "scanned an untracked file: ${out}" ;;
esac

# 8. A directory that is not a git checkout falls back to every file in it.
plain="${work}/plain"
mkdir -p "$plain"
printf 'node-c\n' > "${plain}/notes.txt"
out="$(JERYU_SITE_TERMS_FILE="$terms" "$scan" "$plain")"
case "$out" in
  *'notes.txt:1:node-[a-z]'*) ;;
  *) fail "no match in a plain directory: ${out}" ;;
esac

# 9. Several paths at once report a per-path count and a total.
out="$(JERYU_SITE_TERMS_FILE="$terms" "$scan" --quiet "$repo" "$plain")"
case "$out" in
  *'checkout: 3 matches'*) ;;
  *) fail "missing the first path count: ${out}" ;;
esac
case "$out" in
  *'plain: 1 matches'*) ;;
  *) fail "missing the second path count: ${out}" ;;
esac
case "$out" in
  *'total: 4 matches, 4 not allowlisted'*) ;;
  *) fail "missing the total: ${out}" ;;
esac

echo "site-specifics ok: refuses without a terms file, finds every term, allowlist gates CI mode"
