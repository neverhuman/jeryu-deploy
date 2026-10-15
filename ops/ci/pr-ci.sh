#!/usr/bin/env bash
# Comprehensive local PR gate. Native host CI runs this to produce the required
# exact-head check, and a trusted hosted runner uses the same entrypoint. It carries
# format + clippy (deny warnings) + the FULL workspace test suite + the Jankurai
# audit (>= 85) + immutable web-bundle validation + the local security lane.
set -euo pipefail

# BEGIN GENERATED JANKURAI PIN — DO NOT EDIT
# The governed Jankurai identity is the binary installed on this host and its
# installation receipt: require_jankurai verifies both and exports JERYU_JANKURAI_*
# from the receipt. The one pin of record is jeryu-tool's tool-manifest.toml.
# END GENERATED JANKURAI PIN

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"
source ops/ci/lib.sh
require_jankurai
bash "${repo_root}/ops/ci/test-governed-jankurai.sh"
# No gate may run with the ci_bridge mock flag set (it would pass without running).
bash "${repo_root}/ops/ci/test-ci-env.sh"
# The production release scripts (switch/rollback) against a throwaway forge home.
bash "${repo_root}/scripts/release/test-release-scripts.sh"
# The release-board collector (/releases) against a throwaway forge and stand-in todoq/curl.
bash "${repo_root}/scripts/release-board/test-release-board.sh"

# Verify only this PR worktree against the family manifest; the post-rollout
# control-plane check verifies the complete canonical family. The renderer is
# jeryu-tool's, outside this checkout: a missing one fails closed, and one inside
# this checkout (which the pull request under test could have written) is refused.
if [ -z "${JERYU_TOOL_RENDER+x}" ]; then
JERYU_TOOL_RENDER="$repo_root/../jeryu-tool/ops/render-tool-manifest.sh"
fi
[ -n "$JERYU_TOOL_RENDER" ] || { echo "[pr-ci] missing JERYU_TOOL_RENDER" >&2; exit 1; }
[ -x "$JERYU_TOOL_RENDER" ] || { echo "[pr-ci] renderer is not executable: $JERYU_TOOL_RENDER" >&2; exit 1; }
render_real="$(realpath "$JERYU_TOOL_RENDER")"
case "$render_real/" in
"$(realpath "$repo_root")/"*) echo "[pr-ci] refusing a renderer inside this checkout: $JERYU_TOOL_RENDER" >&2; exit 1 ;;
esac
echo "[pr-ci] jankurai pin drift check" >&2
consumer_repo="$(awk -F'\"' '/^workspace =/ {print $2; exit}' agent/audit-policy.toml)"
bash "$render_real" --check --repo "$consumer_repo" \
--repo-root "$consumer_repo=$repo_root"

# jeryu governs the worker count from live load (overrides any request). host-ci
# already exports a governed JERYU_CI_JOBS; honor it, else ask the governor, else a
# conservative default. An unbounded fan-out once wedged the host — never default high.
if [ -n "${JERYU_CI_JOBS:-}" ]; then
  JOBS="${JERYU_CI_JOBS}"
elif command -v jeryu-ci-governor >/dev/null 2>&1; then
  JOBS="$(jeryu-ci-governor 2>/dev/null || echo 8)"
else
  JOBS=8
fi
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$JOBS}"
TEST_THREADS="${JERYU_CI_TEST_THREADS:-8}"

cargo_lock_before="$(git hash-object Cargo.lock)"
assert_cargo_lock_unchanged() {
  local cargo_lock_after
  cargo_lock_after="$(git hash-object Cargo.lock)"
  if [ "${cargo_lock_after}" != "${cargo_lock_before}" ]; then
    echo "[pr-ci] Cargo.lock changed during validation; refusing to discard it" >&2
    return 1
  fi
}

# Ordinary compilation -- both clippy passes, the workspace test build and the web-bundle lane --
# builds in the gate runner's retained per-slot cache when the gate host offers one
# (JERYU_CI_BUILD_CACHE_DIR; jeryu-ci-runner ops/pr-gate/README.md, "Reusing ordinary compilation
# without retaining proof"). This recipe measures nothing from a build directory: there is no coverage
# or profile build, and every report it writes (.jankurai/, target/jankurai/) stays outside the cache.
# The runner retains the cache only after a green gate and discards it when tools, recipe or Cargo.lock
# change; cargo still checks every source file's freshness. Unset (a laptop, another host), cargo
# builds into target/ as before.
if [ -n "${JERYU_CI_BUILD_CACHE_DIR:-}" ]; then
  export CARGO_TARGET_DIR="${JERYU_CI_BUILD_CACHE_DIR}"
  echo "[pr-ci] ordinary compilation in ${CARGO_TARGET_DIR} (the gate runner's build cache)" >&2
fi
echo "[pr-ci] (jobs=$JOBS) cargo fmt --all --check" >&2
cargo fmt --all --check

echo "[pr-ci] cargo clippy --workspace --all-targets -- -D warnings" >&2
cargo clippy --locked --workspace --all-targets --jobs "$JOBS" -- -D warnings

echo "[pr-ci] cargo clippy --workspace --all-targets --all-features -- -D warnings" >&2
cargo clippy --locked --workspace --all-targets --all-features --jobs "$JOBS" -- -D warnings

# The kernel-sandbox-runtime integration tests spawn REAL sandboxes (user/mount/pid
# namespaces + cgroup-v2 + landlock/seccomp). They require an UNMANAGED cgroup
# environment and fail under host-ci's systemd-managed poll cgroup
# (cgroup_create EEXIST / clone EOPNOTSUPP). They run on the dedicated GitHub-mirror
# runners (full caps). Exclude exactly those here; the remaining workspace tests run.
echo "[pr-ci] cargo test (excl. the host-ci-skips.sh list)" >&2
# --test-threads uses the separately governed process cap: libtest defaults to
# ncpu, and an oversubscribed host starves the live agent-stream tests'
# 30s polling deadlines (await_tty) into false failures. The web::sessions
# live-stream proofs (create_session_*: spawn a real native/docker-seam PTY and
# poll await_tty for the agent's marker) are the same class: under host-ci load
# the sandboxed process does not stream its marker inside the 30s window and the
# proof flakes. They run green on the dedicated GitHub-mirror runners (full caps,
# unloaded); skip them here exactly like the agent-stream/sandbox live tests.
# JERYU_JANKURAI_BIN without a receipt makes every push audit in the integration
# tests (merge_gating) fail its identity check at once instead of spawning the
# host's governed jankurai for ~10 s per push. Those tests assert refs and merge
# gating, never the advisory jankurai/proof verdict; the lib tests already skip it
# under cfg(test) and prove the audit path with a scripted auditor.
# The exclude/skip lists live in ops/ci/host-ci-skips.sh, one place with reasons.
source ops/ci/host-ci-skips.sh
mapfile -t host_ci_skip_args < <(host_ci_skip_args)
# Test temp directories live on tmpfs when one is writable. Nearly every web test
# opens a fresh SQLite forge, and its migrations commit with an fsync each: on a
# disk that is ~2.2s per fixture (1.55s to open, 0.6s for WebState), on tmpfs
# ~45ms. With several gates syncing one disk at once, that was most of the
# jeryu_api lib run (124-356s). No test asserts durability. JERYU_CI_TEST_TMPDIR
# names another tmpfs; with none writable the tests use the default TMPDIR.
test_tmp=""
for tmpfs in "${JERYU_CI_TEST_TMPDIR:-}" /dev/shm; do
  if [ -n "$tmpfs" ] && [ -d "$tmpfs" ] && [ -w "$tmpfs" ]; then
    test_tmp="$(mktemp -d "$tmpfs/jeryu-pr-ci.XXXXXX")" && break
  fi
done
if [ -n "$test_tmp" ]; then
  trap 'rm -rf -- "$test_tmp"' EXIT
  echo "[pr-ci] test temp directories on $test_tmp" >&2
fi
TMPDIR="${test_tmp:-${TMPDIR:-/tmp}}" JERYU_JANKURAI_BIN=/nonexistent/jankurai \
  cargo test --locked --workspace "${HOST_CI_EXCLUDE_PACKAGES[@]}" --jobs "$JOBS" --no-fail-fast -- \
  --test-threads "$TEST_THREADS" "${host_ci_skip_args[@]}"

echo "[pr-ci] immutable web-bundle integration" >&2
bash "${repo_root}/ops/ci/web.sh"

assert_cargo_lock_unchanged
echo "[pr-ci] jankurai audit (>= 85)" >&2
run_governed_jankurai audit . --full --mode advisory --policy agent/audit-policy.toml \
  --json .jankurai/repo-score.json --md .jankurai/repo-score.md
score="$(jq -r '.score // 0' .jankurai/repo-score.json)"
caps="$(jq -c '.caps_applied // []' .jankurai/repo-score.json)"
echo "[pr-ci] jankurai score=${score} caps=${caps}" >&2
jq -e '(.score // 0) >= 85 and ((.caps_applied // []) | length == 0)' \
  .jankurai/repo-score.json >/dev/null

# This report is the recipe's own floor check, written by the head under test, so it
# is never submitted to the forge: the gate host's auditor scores the head itself
# (jeryu-ci-runner ops/pr-gate, pr-gate-audit.sh) and completes jankurai/proof.

# The pre-approval gate: the verdict the hosted jankurai/proof will publish for
# this head, before the PR exists. Refuses only where the rollout is on
# (agent/jankurai-gate.toml); elsewhere it reports and passes.
echo "[pr-ci] jankurai gate (hosted jankurai/proof verdict for this head)" >&2
bash "${repo_root}/ops/ci/jankurai-gate.sh"

echo "[pr-ci] security lane"
JERYU_SECURITY_NETWORK=1 bash "${repo_root}/ops/ci/security.sh"
assert_cargo_lock_unchanged

echo "[pr-ci] PASS — fmt + clippy + workspace tests + web bundle + jankurai + security all green" >&2
