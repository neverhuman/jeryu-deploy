#!/usr/bin/env bash
# Bind the manifest drift consumer to this repository before the generated gate runs.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
policy="${1:-$root/agent/audit-policy.toml}"
workspace="$(awk -F '"' '/^workspace =/ {print $2; exit}' "$policy")"
if [[ "$workspace" != jeryu-deploy ]]; then
  echo "audit workspace must be exactly jeryu-deploy" >&2
  exit 1
fi
