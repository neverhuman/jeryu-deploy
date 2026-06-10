#!/usr/bin/env bash
set -euo pipefail
if find . -path './.git' -prune -o -name '.env' -type f -print | grep -q .; then
  printf 'security check failed: committed .env file found\n' >&2
  exit 1
fi
printf 'security bootstrap ok\n'
