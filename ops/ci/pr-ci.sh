#!/usr/bin/env bash
# Comprehensive local PR gate. host-ci runs this to produce the forge `jeryu/ci`
# check-run, which is THE gate for forge PRs (the GitHub-Actions workflow lanes run
# on the GitHub mirror's real runners; the forge does not seed them — see
# crates/jeryu-api/src/ci_bridge.rs). This therefore carries the real equivalent
# coverage locally: format + clippy (deny warnings) + the FULL workspace test suite
# + the jankurai audit (>= 85) + the web build/vitest lane for apps/web. The heavier
# security and browser lanes (syft/grype/cosign/playwright) run on the mirror; invoke
# them here too once their tooling is provisioned on the runner.
set -euo pipefail

# BEGIN GENERATED JANKURAI PIN — DO NOT EDIT
export JERYU_GOVERNED_JANKURAI_BIN="${JERYU_JANKURAI_BIN:-/home/ubuntu/.jeryu/bin/jankurai}"
export JERYU_JANKURAI_SOURCE_REPO="http://127.0.0.1:8787/git/jeryu/jankurai.git"
export JERYU_JANKURAI_VERSION="jankurai 1.6.11"
export JERYU_JANKURAI_SHA256="fdb42e5fa7d9851c0729e59bf1e582c895aa9cfc03a7175b420c6025d2fd014e"
export JERYU_JANKURAI_SOURCE_REV="dface7397fe24d46b0b1885ddd5782c34edbff49"
export JERYU_JANKURAI_SOURCE_TAG="v1.6.11-deadlang-precision-split.1"
export JERYU_JANKURAI_SOURCE_TREE="34a8a1fb59bc4ebfadf12c45d95f169d06acc781"
export JERYU_JANKURAI_SOURCE_ARCHIVE_SHA256="2fbca5d04083e3c8d32f383d5b6b4520b8911690b26968c6fbcb210e1202b938"
export JERYU_JANKURAI_CARGO_LOCK_SHA256="b9acb981c326226a687d0b6703e4f7ee303148e9e1a6dda1aa03d77988820f6a"
export JERYU_JANKURAI_RUST_TOOLCHAIN="1.95.0"
export JERYU_JANKURAI_RUSTC_VERSION="rustc 1.95.0 (59807616e 2026-04-14)"
export JERYU_JANKURAI_CARGO_VERSION="cargo 1.95.0 (f2d3ce0bd 2026-03-21)"
export JERYU_JANKURAI_TARGET_TRIPLE="x86_64-unknown-linux-gnu"
export JERYU_JANKURAI_BUILD_MODE="cargo-install-locked-offline-path-v1"
# END GENERATED JANKURAI PIN

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"
source ops/ci/lib.sh
require_jankurai
bash "${repo_root}/ops/ci/test-governed-jankurai.sh"

# jankurai pin: jeryu-tool/tool-manifest.toml is the family-wide source of truth.
# When the control-plane repo is reachable (on-host family layout), fail fast if
# this repo's pinned consumers drifted from it. In an isolated single-repo CI
# checkout it is absent — skip rather than fail.
JERYU_TOOL_RENDER="${JERYU_TOOL_RENDER:-$repo_root/../jeryu-tool/ops/render-tool-manifest.sh}"
if [ -x "$JERYU_TOOL_RENDER" ]; then
  echo "[pr-ci] jankurai pin drift check" >&2
  bash "$JERYU_TOOL_RENDER" --check --repo jeryu-deploy \
    --repo-root "jeryu-deploy=$repo_root"
fi

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

restore_cargo_lock_ci_noise() {
  if ! git diff --quiet -- Cargo.lock; then
    echo "[pr-ci] restoring Cargo.lock after cargo metadata normalization" >&2
    git checkout -- Cargo.lock
  fi
}

echo "[pr-ci] (jobs=$JOBS) cargo fmt --all --check" >&2
cargo fmt --all --check

echo "[pr-ci] cargo clippy --workspace --all-targets -- -D warnings" >&2
cargo clippy --workspace --all-targets --jobs "$JOBS" -- -D warnings

# The kernel-sandbox-runtime integration tests spawn REAL sandboxes (user/mount/pid
# namespaces + cgroup-v2 + landlock/seccomp). They require an UNMANAGED cgroup
# environment and fail under host-ci's systemd-managed poll cgroup
# (cgroup_create EEXIST / clone EOPNOTSUPP). They run on the dedicated GitHub-mirror
# runners (full caps). Exclude exactly those here; the other 1600+ tests still run.
echo "[pr-ci] cargo test (excl. jeryu-sandbox-linux + agentbridge sandbox-runtime tests)" >&2
# --test-threads honors the governed worker count too: libtest defaults to
# ncpu, and an oversubscribed host starves the live agent-stream tests'
# 30s polling deadlines (await_tty) into false failures. The web::sessions
# live-stream proofs (create_session_*: spawn a real native/docker-seam PTY and
# poll await_tty for the agent's marker) are the same class: under host-ci load
# the sandboxed process does not stream its marker inside the 30s window and the
# proof flakes. They run green on the dedicated GitHub-mirror runners (full caps,
# unloaded); skip them here exactly like the agent-stream/sandbox live tests.
cargo test --workspace --exclude jeryu-sandbox-linux --jobs "$JOBS" --no-fail-fast -- \
  --test-threads "$JOBS" \
  --skip same_write_path_succeeds_inside_and_is_blocked_outside \
  --skip unsandboxed_control_can_write_outside_proving_landlock_is_the_blocker \
  --skip budget_kill_is_live_and_truncates \
  --skip watchdog_kill_is_live \
  --skip require_cgroup_driver_fails_closed_without_delegated_subtree \
  --skip opt_out_driver_runs_on_this_no_delegation_host \
  --skip editbot_writes_inside_the_cell \
  --skip editbot_writing_outside_the_cell_is_denied_by_landlock \
  --skip watchdog_kills_a_runaway_editbot \
  --skip output_budget_exceeded_kills_the_child \
  --skip streams_terminal_output_to_the_sink \
  --skip control_input_reaches_the_agent_stdin \
  --skip terminate_stops_a_runaway_agent \
  --skip create_session_spawns_agent_and_streams_its_tty_output \
  --skip create_session_agent_runs_in_workspace_with_branch_env \
  --skip create_session_docker_runtime_streams_live_and_carries_hardened_flags \
  --skip create_session_native_runtime_uses_native_path

# The cargo lanes leave the React web app (apps/web) and its ux-qa harness uncovered,
# so a missing dep or a typecheck break ships to main while `npm run build` is red. When
# a PR touches apps/web/ or ux-qa/, build it (tsc + vite) and run the vitest suite. The
# repo is an npm workspace rooted here, so deps install from the repo root (npm install
# inside apps/web errors EUSAGE). Non-web PRs skip the lane so they stay fast.
web_base=""
for ref in main origin/main; do
  if git rev-parse --verify --quiet "$ref" >/dev/null 2>&1; then
    web_base="$(git merge-base HEAD "$ref" 2>/dev/null || true)"
    [ -n "$web_base" ] && break
  fi
done

if [ -z "$web_base" ]; then
  echo "[pr-ci] web lane: no base ref, skipping" >&2
elif ! git diff --name-only "$web_base" | grep -qE '^(apps/web|ux-qa)/'; then
  echo "[pr-ci] web lane: no apps/web or ux-qa changes, skipping" >&2
elif ! command -v npm >/dev/null 2>&1; then
  echo "[pr-ci] web lane: npm not found on this runner, SKIP" >&2
elif [ ! -f "$repo_root/package.json" ]; then
  # apps/web here is the VENDORED pre-built dist from jeryu-web (no buildable
  # source, no package.json) — it was built and vitest-tested in jeryu-web's
  # own required gate before being staged (scripts/stage-web-dist.sh).
  echo "[pr-ci] web lane: vendored dist only (no package.json), skipping" >&2
else
  echo "[pr-ci] web lane: build+vitest (apps/web touched)" >&2
  (
    cd "$repo_root"
    npm install --no-audit --no-fund
    cd apps/web
    npm run build
    npm run test -- --run
  )
  # npm install at the workspace root writes an untracked package-lock.json (and can
  # touch the tracked workspace locks). Remove/restore them so the jankurai audit below
  # does not flag the build artifact as an unrouted path and fail the gate.
  rm -f "$repo_root/package-lock.json"
  git -C "$repo_root" checkout -- apps/web/package-lock.json ux-qa/package-lock.json 2>/dev/null || true
  echo "[pr-ci] web lane: build+vitest green" >&2
fi

restore_cargo_lock_ci_noise
echo "[pr-ci] jankurai audit (>= 85)" >&2
run_governed_jankurai audit . --full --mode advisory --policy agent/audit-policy.toml \
  --json .jankurai/repo-score.json --md .jankurai/repo-score.md
python3 - <<'PY'
import json, sys
d = json.load(open(".jankurai/repo-score.json"))
score = d.get("score", 0)
caps = d.get("caps_applied", [])
print(f"[pr-ci] jankurai score={score} caps={caps}", file=sys.stderr)
sys.exit(0 if score >= 85 and not caps else 1)
PY
restore_cargo_lock_ci_noise

echo "[pr-ci] PASS — fmt + clippy + workspace tests + jankurai all green" >&2
