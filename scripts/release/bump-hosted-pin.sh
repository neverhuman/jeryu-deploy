#!/usr/bin/env bash
# bump-hosted-pin.sh REPO TAG COMMIT — move one hosted crate pin of this jeryu-deploy checkout
# from the tag it pins now to TAG at COMMIT. It edits files and nothing else: no commit, no push,
# no forge request. Cargo may fetch dependencies. auto-tag.sh takes it from jeryu-deploy main
# (git archive), so a bump uses the reviewed recipe, and a person can run it by
# hand in a checkout to prepare the same bump.
#
# A hosted crate pin lives in three places, and all three move together:
#   1. `.cargo/hosted-pin-refs.tsv`: the row ops/ci/dependency-sources.sh reads back from the
#      forge (tag, commit, and the preserve/hosted-cargo support branch of that tag);
#   2. the manifests (`Cargo.toml`, `crates/*/Cargo.toml`): every `tag = "<pinned>"` on a line
#      whose `git` URL names this repository;
#   3. `Cargo.lock`: resolved dependency graph and git source identities.
# `jeryu-split.lock.toml` is not touched: its crate `tag`/`commit` fields are not read by cargo
# and are not the pin (docs/pipeline-events.md, Pins).
#
# The manifests keep deliberately frozen `-split.0` pins that `[patch]` redirects, so only the
# exact tag string this repository pins today is replaced, and a pin that is itself `-split.0` is
# refused rather than guessed about. Every edit is read back: the old tag must be gone from the
# manifest lines, the new one must appear there exactly as often as the old one did, and the
# resolved lock must pass --locked with the requested commit. No file outside the three may differ. Any mismatch leaves the checkout for a person and exits nonzero.
#
# Exit: 0 and `bumped <repo> <old tag> -> <tag>` on stdout; 64 on a usage error; 1 on anything else.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
say() { echo "[bump-hosted-pin] $*" >&2; }
die() { say "$*"; exit 1; }

[[ $# == 3 ]] || { echo "usage: bump-hosted-pin.sh REPO TAG COMMIT" >&2; exit 64; }
repo="$1" tag="$2" commit="$3"
[[ "$repo" =~ ^jeryu-[a-z0-9-]+$ ]] || { echo "not a hosted crate repository: $repo" >&2; exit 64; }
[[ "$tag" =~ ^${repo}-v[0-9]+\.[0-9]+\.[0-9]+-split\.[0-9]+$ ]] \
  || { echo "not a $repo split tag: $tag" >&2; exit 64; }
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || { echo "not a 40-hex commit: $commit" >&2; exit 64; }

# The recipe's own tree: auto-tag.sh archives scripts/release, ops/ci/hosted-git-env.sh,
# .cargo/hosted-gitconfig, .cargo/config.toml and rust-toolchain.toml from main together, so the Cargo transport the bump resolves through is
# the reviewed one, never whatever the checkout being bumped carries. By hand it is the checkout.
recipe_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
transport="$recipe_root/ops/ci/hosted-git-env.sh"
[[ -f "$transport" && ! -L "$transport" ]] || die "the recipe has no ops/ci/hosted-git-env.sh beside it: $transport"
root="$(git rev-parse --show-toplevel)" || die "not inside a git checkout"
cd "$root"
pin_policy=.cargo/hosted-pin-refs.tsv
[[ -f "$pin_policy" && ! -L "$pin_policy" ]] || die "$pin_policy is not a regular file"
[[ -f Cargo.lock && ! -L Cargo.lock ]] || die "Cargo.lock is not a regular file"

# Cargo also takes configuration from the checkout and from every directory above it (auto-tag's
# checkout sits under mktemp -d), and `build.rustc`, `[env]` or `[source]` there would steer what
# the resolution runs and fetches. Only the reviewed .cargo/config.toml may: the checkout's must
# be byte-identical to the recipe tree's (auto-tag archives it from main), and a directory above
# the checkout may carry only that same file (a lane nested in its main checkout does) and no
# legacy `config`. CARGO_HOME's own config is the runner's.
same_file() { # A B: both absent, or both regular files with the same bytes
  if [[ ! -e "$1" && ! -L "$1" ]]; then [[ ! -e "$2" && ! -L "$2" ]]; return; fi
  [[ -f "$1" && ! -L "$1" && -f "$2" && ! -L "$2" ]] && cmp -s -- "$1" "$2"
}
same_file .cargo/config.toml "$recipe_root/.cargo/config.toml" \
  || die ".cargo/config.toml differs from the reviewed one beside the recipe"
[[ ! -e .cargo/config && ! -L .cargo/config ]] || die "the checkout carries a legacy .cargo/config"
cargo_home="$(cd "${CARGO_HOME:-$HOME/.cargo}" 2>/dev/null && pwd -P || true)"
dir="$(dirname "$(pwd -P)")"
while :; do
  if [[ "$dir/.cargo" != "$cargo_home" ]]; then
    [[ ! -e "$dir/.cargo/config" && ! -L "$dir/.cargo/config" ]] \
      || die "Cargo would read $dir/.cargo/config above the checkout"
    [[ ! -e "$dir/.cargo/config.toml" && ! -L "$dir/.cargo/config.toml" ]] \
      || same_file "$dir/.cargo/config.toml" "$recipe_root/.cargo/config.toml" \
      || die "Cargo would read $dir/.cargo/config.toml above the checkout"
  fi
  [[ "$dir" == / ]] && break
  dir="$(dirname "$dir")"
done
# rustup's cargo proxy takes its toolchain from the first rust-toolchain(.toml) at or above the
# checkout, and `[toolchain] path` there names any binary to run as cargo. The checkout's must be
# the reviewed one (auto-tag archives it from main beside the recipe); without one, nothing above
# the checkout may carry one either. RUSTUP_TOOLCHAIN then pins the reviewed channel outright, so a
# directory override in rustup's settings does not pick the cargo either.
same_file rust-toolchain.toml "$recipe_root/rust-toolchain.toml" \
  || die "rust-toolchain.toml differs from the reviewed one beside the recipe"
[[ ! -e rust-toolchain && ! -L rust-toolchain ]] || die "the checkout carries a legacy rust-toolchain file"
if [[ -f rust-toolchain.toml ]]; then
  channel="$(awk -F '"' '/^channel = "/ { print $2 }' "$recipe_root/rust-toolchain.toml")"
  [[ "$channel" =~ ^[0-9A-Za-z._-]+$ ]] || die "the reviewed rust-toolchain.toml names no plain channel"
  export RUSTUP_TOOLCHAIN="$channel"
else
  dir="$(dirname "$(pwd -P)")"
  while :; do
    for f in "$dir/rust-toolchain" "$dir/rust-toolchain.toml"; do
      [[ ! -e "$f" && ! -L "$f" ]] || die "rustup would read $f above the checkout"
    done
    [[ "$dir" == / ]] && break
    dir="$(dirname "$dir")"
  done
fi

# The lock must already be complete under this runner's cargo before anything is edited, so the
# re-resolution below changes what the new tag needs and not the lock's format.
source "$transport"
cargo metadata --locked --format-version 1 > /dev/null \
  || die "Cargo.lock is not complete under this cargo before the bump; that is a person's fix"
registry_packages() { # the crates.io name/version set of Cargo.lock
  awk '/^\[\[package\]\]/ { name = ""; version = "" }
    /^name = / { name = $3 } /^version = / { version = $3 }
    /^source = "registry\+/ { print name, version }' Cargo.lock | tr -d '"' | LC_ALL=C sort
}
registry_before="$(registry_packages)"

rows="$(awk -F '|' -v repo="$repo" '!/^[[:space:]]*(#|$)/ && $1 == repo { print }' "$pin_policy")"
[[ "$(grep -c . <<<"$rows")" == 1 && -n "$rows" ]] || die "$pin_policy has no single $repo row"
IFS='|' read -r _ old_tag old_commit old_ref extra <<<"$rows"
[[ -z "${extra:-}" ]] || die "the $repo pin row has more fields than the policy's four"
[[ "$old_tag" =~ ^${repo}-v[0-9]+\.[0-9]+\.[0-9]+-split\.[0-9]+$ ]] || die "the pinned tag is malformed: $old_tag"
[[ "$old_commit" =~ ^[0-9a-f]{40}$ ]] || die "the pinned commit is not 40-hex: $old_commit"
[[ "$old_ref" == "refs/heads/preserve/hosted-cargo/$old_tag" ]] || die "the pinned support ref is not the convention: $old_ref"
[[ "$old_tag" != *-split.0 ]] \
  || die "$repo pins $old_tag: a -split.0 pin is also what [patch] redirects, so this bump is a person's"
[[ "$tag" != "$old_tag" ]] || die "$repo already pins $tag"
[[ "$commit" != "$old_commit" ]] || die "$repo already pins ${commit:0:12}"
# Same version line, strictly forward: split.N increments, and a bump never walks back.
old_version="${old_tag#"$repo"-v}"; old_version="${old_version%-split.*}"
version="${tag#"$repo"-v}"; version="${version%-split.*}"
[[ "$version" == "$old_version" ]] || die "$tag changes the version line of $old_tag; that is a person's release"
(( ${tag##*-split.} > ${old_tag##*-split.} )) || die "$tag is not after $old_tag"

# The manifests that pin this repository by its current tag. `git ls-files` so an untracked or
# generated manifest is never edited, and the pin's own lines are the only ones matched.
mapfile -t manifests < <(git ls-files -- Cargo.toml 'crates/*/Cargo.toml' | LC_ALL=C sort)
[[ "${#manifests[@]}" -gt 0 ]] || die "this checkout has no tracked manifests"
pin_lines() { # FILE -> the lines pinning this repository at TAG_ARG
  awk -v repo="$repo" -v want="$2" '
    index($0, "/" repo ".git\"") > 0 && index($0, "tag = \"" want "\"") > 0 { n++ }
    END { print n + 0 }' "$1"
}
expected_total=0 changed=()
for manifest in "${manifests[@]}"; do
  count="$(pin_lines "$manifest" "$old_tag")"
  (( count > 0 )) || continue
  awk -v repo="$repo" -v old="$old_tag" -v new="$tag" '
    index($0, "/" repo ".git\"") > 0 {
      before = "tag = \"" old "\""
      i = index($0, before)
      if (i > 0) {
        $0 = substr($0, 1, i - 1) "tag = \"" new "\"" substr($0, i + length(before))
      }
    }
    { print }' "$manifest" >"$manifest.bump"
  cat "$manifest.bump" >"$manifest"; rm -f "$manifest.bump"
  [[ "$(pin_lines "$manifest" "$old_tag")" == 0 ]] || die "$manifest still pins $old_tag"
  [[ "$(pin_lines "$manifest" "$tag")" == "$count" ]] || die "$manifest did not take $tag on all $count lines"
  expected_total=$((expected_total + count))
  changed+=("$manifest")
done
(( expected_total > 0 )) || die "no manifest pins $repo at $old_tag"

# Cargo.lock: the git source identity of this repository, tag and commit together.
lock_lines() { # TAG COMMIT -> how many lock sources name this repository at that identity
  awk -v repo="$repo" -v want="$1" -v sha="$2" '
    /^source = "git\+/ && index($0, "/" repo ".git?tag=" want "#" sha "\"") > 0 { n++ }
    END { print n + 0 }' Cargo.lock
}
lock_count="$(lock_lines "$old_tag" "$old_commit")"
(( lock_count > 0 )) || die "Cargo.lock has no $repo source at $old_tag#${old_commit:0:12}"
# Let Cargo re-resolve the lock for the new manifests, keeping unrelated locked versions.
# Rewriting source strings alone misses dependencies the new tag adds or drops, so the lock diff
# can carry more than this repository's source lines: the bump PR says so, and review reads it.
# `cargo metadata` runs no build script. The --locked call after it only confirms the written lock
# is complete; what proves the requested commit landed is the source count below, and the
# crates.io package set must not move at all.
cargo metadata --format-version 1 > /dev/null || die "Cargo could not resolve the bumped pin"
cargo metadata --locked --format-version 1 > /dev/null || die "Cargo.lock is not complete after resolution"
[[ "$(lock_lines "$old_tag" "$old_commit")" == 0 ]] || die "Cargo.lock still names $repo at $old_tag"
lock_count="$(lock_lines "$tag" "$commit")"
(( lock_count > 0 )) || die "Cargo.lock did not resolve $tag at $commit"
# A multi-package repository can gain or lose packages, but every new-tag source must
# resolve to the supplied commit, including packages newly added to the graph.
tag_count="$(awk -v repo="$repo" -v tag="$tag" '
  /^source = "git\+/ && index($0, "/" repo ".git?tag=" tag "#") > 0 { n++ }
  END { print n + 0 }' Cargo.lock)"
[[ "$tag_count" == "$lock_count" ]] || die "Cargo.lock resolved $tag at an unexpected commit"
# Hosted Git sources are inventoried by ops/ci/dependency-sources.sh; crates.io packages are not.
# A bump that adds, drops or moves one is a person's, so an automated bump never carries one.
registry_after="$(registry_packages)"
[[ "$registry_after" == "$registry_before" ]] \
  || die "the new tag changes crates.io packages in Cargo.lock; that bump is a person's: $(diff \
    <(printf '%s\n' "$registry_before") <(printf '%s\n' "$registry_after") | grep '^[<>]' | tr '\n' ' ')"
changed+=(Cargo.lock)

# The pin policy row the gate reads back from the forge.
awk -F '|' -v OFS='|' -v repo="$repo" -v tag="$tag" -v sha="$commit" '
  !/^[[:space:]]*(#|$)/ && $1 == repo { print repo, tag, sha, "refs/heads/preserve/hosted-cargo/" tag; next }
  { print }' "$pin_policy" >"$pin_policy.bump"
cat "$pin_policy.bump" >"$pin_policy"; rm -f "$pin_policy.bump"
[[ "$(awk -F '|' -v repo="$repo" -v tag="$tag" -v sha="$commit" \
    '$1 == repo && $2 == tag && $3 == sha && $4 == "refs/heads/preserve/hosted-cargo/" tag { n++ } END { print n + 0 }' \
    "$pin_policy")" == 1 ]] || die "$pin_policy did not take the $repo row"
changed+=("$pin_policy")

# Nothing outside the pin's own files may differ, and each of them must actually have changed.
mapfile -t differs < <(git diff --name-only -- . | LC_ALL=C sort -u)
mapfile -t expected < <(printf '%s\n' "${changed[@]}" | LC_ALL=C sort -u)
[[ "$(printf '%s\n' "${differs[@]}")" == "$(printf '%s\n' "${expected[@]}")" ]] \
  || die "the bump changed $(printf '%s ' "${differs[@]}"), not exactly $(printf '%s ' "${expected[@]}")"
printf 'bumped %s %s -> %s (%s manifest lines, %s lock sources)\n' \
  "$repo" "$old_tag" "$tag" "$expected_total" "$lock_count"
