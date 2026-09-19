#!/usr/bin/env bash
# Source to export JERYU_WEB_DIST for a release-profile build: the jeryu-web
# dist pinned in jeryu-split.lock.toml, which crates/jeryu-api/build.rs verifies
# against the lock's web_dist_sha256 and embeds. An existing JERYU_WEB_DIST is
# kept (build.rs still verifies it); otherwise the pinned commit is built into
# target/web-dist by scripts/release/build-web-dist.sh, which needs network and
# docker.
if [ -z "${JERYU_WEB_DIST:-}" ]; then
  _web_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  _web_built="$(bash "${_web_root}/scripts/release/build-web-dist.sh" "${_web_root}/target/web-dist" | tail -1)" || {
    echo "web-dist-env: building the pinned jeryu-web dist failed" >&2
    return 1 2>/dev/null || exit 1
  }
  JERYU_WEB_DIST="${_web_root}/target/web-dist/${_web_built%% *}/dist"
  unset _web_root _web_built
fi
export JERYU_WEB_DIST
