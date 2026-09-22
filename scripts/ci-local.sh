#!/usr/bin/env bash
# Alias for the single local gate entry point, ops/ci/pr-ci.sh. Kept so existing
# muscle memory and scripts keep working; do not add steps here.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec bash "${repo_root}/ops/ci/pr-ci.sh" "$@"
