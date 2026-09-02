#!/usr/bin/env bash
# Canonical Jankurai security entrypoint for this standalone repository.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec bash "${ROOT}/ops/ci/security.sh" "$@"
