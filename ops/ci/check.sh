#!/usr/bin/env bash
set -euo pipefail

if [[ -f Cargo.toml ]]; then
  cargo metadata --format-version 1 --no-deps >/dev/null
  if [[ "${JERYU_SPLIT_FULL_CHECK:-0}" == "1" ]]; then
    cargo check --workspace --all-targets --jobs "${JERYU_CI_JOBS:-40}"
  fi
fi

if [[ -f package.json ]]; then
  node -e 'JSON.parse(require("fs").readFileSync("package.json", "utf8"))' >/dev/null
  if [[ -f apps/web/package.json ]]; then
    node -e 'JSON.parse(require("fs").readFileSync("apps/web/package.json", "utf8"))' >/dev/null
  fi
  if [[ "${JERYU_SPLIT_FULL_CHECK:-0}" == "1" ]]; then
    npm --workspace @jeryu/web run typecheck
  fi
fi
printf 'check ok: %s\n' "$(pwd)"
