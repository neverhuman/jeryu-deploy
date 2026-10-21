#!/usr/bin/env bash
# install-auto-tag.sh — install the auto-tag timer for the current user on the release host.
# Copies auto-tag.sh, bump-hosted-pin.sh and latest-statuses.jq to ~/.local/share/jeryu-auto-tag/
# (the bump recipe a pull request is built with is always taken from jeryu-deploy main) and
# enables jeryu-auto-tag.timer. To turn it off again:
#   systemctl --user disable --now jeryu-auto-tag.timer
#
# The timer's settings live in an environment file the unit reads, outside this repository,
# because a credential path and an automation identity belong to one installation:
#   ~/.config/jeryu/auto-tag.env, mode 0600, with at least
#     JERYU_TAG_TOKEN_FILE=<path to releasebot's 0600 agent token>
#     JERYU_TAG_GIT_NAME=releasebot
#     JERYU_TAG_GIT_EMAIL=<releasebot's address>
# Until that file exists the timer is installed but every tick refuses by name. Run
# `systemctl --user start jeryu-auto-tag.service` once and read the journal to check it.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
dest="$HOME/.local/share/jeryu-auto-tag"
units="${JERYU_SYSTEMD_USER_DIR:-$HOME/.config/systemd/user}"
mkdir -p "$dest" "$units"
install -m 644 "$here/latest-statuses.jq" "$dest/latest-statuses.jq"
install -m 755 "$here/auto-tag.sh" "$dest/auto-tag.sh"
install -m 755 "$here/bump-hosted-pin.sh" "$dest/bump-hosted-pin.sh"
install -m 644 "$here/systemd/jeryu-auto-tag.service" "$here/systemd/jeryu-auto-tag.timer" "$units/"
systemctl --user daemon-reload
systemctl --user enable --now jeryu-auto-tag.timer
systemctl --user list-timers jeryu-auto-tag.timer --no-pager
