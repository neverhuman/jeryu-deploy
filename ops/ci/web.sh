#!/usr/bin/env bash
# Validate the jeryu-web pin and prove that the Deploy-owned API serves the SPA.
# This repository carries no web source, npm workspace or built SPA: a release
# builds the jeryu-web commit pinned in jeryu-split.lock.toml outside the
# checkout (scripts/release/build-web-dist.sh), and crates/jeryu-api/build.rs
# refuses a dist whose manifest hash differs from the lock's web_dist_sha256.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"
source "${ROOT}/ops/ci/common.sh"

if [ -e package.json ] || [ -e apps/web/package.json ]; then
  echo "web gate: unexpected npm source manifest in Deploy; jeryu-web owns buildable source" >&2
  exit 1
fi
if [ -n "$(git ls-files -- apps/web)" ]; then
  echo "web gate: a built SPA is tracked under apps/web; jeryu-web is pinned by commit in jeryu-split.lock.toml" >&2
  git ls-files -- apps/web | head -5 >&2
  exit 1
fi

cargo run --locked -q -p jeryu-split-tool -- verify-lock --lock jeryu-split.lock.toml
echo "web gate: jeryu-web pin in jeryu-split.lock.toml is well formed"
cargo test --locked -p jeryu-api --features web --jobs "${JERYU_CI_JOBS}" --test web_dist_pin
echo "web gate: a tampered, stale or missing web dist fails the build"
cargo test --locked -p jeryu-api --features web --jobs "${JERYU_CI_JOBS}" \
  browser_repo_routes_serve_the_spa_shell
echo "web gate: Deploy API SPA integration passed"
