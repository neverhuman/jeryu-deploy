#!/usr/bin/env bash
# Shared local CI defaults. Keep this file source-only.
set -euo pipefail

# BEGIN GENERATED JANKURAI PIN — DO NOT EDIT
# The governed Jankurai identity is the binary installed on this host and its
# installation receipt: require_jankurai verifies both and exports JERYU_JANKURAI_*
# from the receipt. The one pin of record is jeryu-tool's tool-manifest.toml.
# END GENERATED JANKURAI PIN

# shellcheck source=ops/ci/ci-env.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/ci-env.sh"
# shellcheck source=ops/ci/lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

jeryu_jankurai() {
  run_governed_jankurai "$@"
}

# jeryu_gate <crate> [args...]
#
# Invoke one of the Rust governance/CI gate binaries through Cargo's release
# runner so local gates never execute stale binaries from a previous build.
jeryu_gate() {
  local crate="$1"; shift
  if [ "$crate" = "jeryu-repogate" ]; then
    case "${1:-}" in
      affected-plan|ci-lanes-check|ci-lanes-list)
        cargo run --locked -q --release -p jeryu-split-tool -- "$@"
        return
        ;;
      *)
        printf 'unsupported retired monorepo gate for split repository: %s\n' "${1:-<missing>}" >&2
        return 2
        ;;
    esac
  fi
  cargo run --locked -q --release -p "${crate}" -- "$@"
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
