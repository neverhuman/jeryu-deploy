#!/usr/bin/env bash
# Shared local CI defaults. Keep this file source-only.
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/ci-env.sh"

JERYU_CI_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
JERYU_JANKURAI_VERSION="${JANKURAI_VERSION:-jankurai 1.6.10}"

# Default to the jeryu-OWNED global auditor (~/.jeryu/bin/jankurai) when present,
# else the legacy cargo-home install. An explicit JERYU_JANKURAI_BIN always wins —
# the agent sandbox sets it to its baked /opt/rust/cargo/bin/jankurai (--network
# none) and must NOT be repointed here.
_jeryu_default_jankurai() {
  if [ -x "${HOME}/.jeryu/bin/jankurai" ]; then
    printf '%s' "${HOME}/.jeryu/bin/jankurai"
  else
    printf '%s' "${CARGO_HOME:-$HOME/.cargo}/bin/jankurai"
  fi
}
JERYU_JANKURAI_BIN="${JERYU_JANKURAI_BIN:-$(_jeryu_default_jankurai)}"

# ensure_pinned_jankurai
#
# Always verify and execute the pinned auditor so stale PATH shadows (e.g.
# ~/.local/bin before ~/.cargo/bin) cannot change audit semantics mid-lane. When
# resolved to the jeryu-owned global binary, (re)install it from the jeryu-tool
# control plane (tool-manifest.toml pin); otherwise fall back to the repo-local
# ensure-jankurai.sh.
ensure_pinned_jankurai() {
  local tool_installer="${JERYU_CI_ROOT}/../jeryu-tool/ops/install-jankurai.sh"
  if [ "${JERYU_JANKURAI_BIN}" = "${HOME}/.jeryu/bin/jankurai" ] && [ -x "${tool_installer}" ]; then
    bash "${tool_installer}" >/dev/null
  else
    bash "${JERYU_CI_ROOT}/ops/ci/ensure-jankurai.sh" >/dev/null
  fi
  if [ ! -x "${JERYU_JANKURAI_BIN}" ]; then
    echo "pinned jankurai binary missing: ${JERYU_JANKURAI_BIN}" >&2
    return 1
  fi
  if ! "${JERYU_JANKURAI_BIN}" --version | grep -qx "${JERYU_JANKURAI_VERSION}"; then
    echo "wrong pinned jankurai version at ${JERYU_JANKURAI_BIN}: $("${JERYU_JANKURAI_BIN}" --version 2>&1 || true)" >&2
    return 1
  fi
}

jeryu_jankurai() {
  ensure_pinned_jankurai
  "${JERYU_JANKURAI_BIN}" "$@"
}

# jeryu_gate <crate> [args...]
#
# Invoke one of the Rust governance/CI gate binaries through Cargo's release
# runner so local gates never execute stale binaries from a previous build.
jeryu_gate() {
  local crate="$1"; shift
  if [ "$crate" = "jeryu-repogate" ]; then
    cargo run -q --release -p "${crate}" -- "$@"
    return
  fi
  cargo run -q --release -p "${crate}" -- "$@"
}

# jeryu_raw_policy <output-path>
#
# Emit a temporary copy of agent/audit-policy.toml without the dead-language
# allowlist so callers can publish the raw report beside the gate.
jeryu_raw_policy() {
  local out="$1"
  awk '
    BEGIN { skip = 0 }
    /^\[dead_language\]$/ { skip = 1; next }
    skip && /^\[/ { skip = 0 }
    !skip { print }
  ' agent/audit-policy.toml > "${out}"
}
