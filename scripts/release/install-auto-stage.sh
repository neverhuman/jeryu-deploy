#!/usr/bin/env bash
# install-auto-stage.sh — install the auto-stage timer for the current user on the release host.
# Copies auto-stage.sh to ~/.local/share/jeryu-auto-stage/ (the staging recipe itself is always
# taken from the commit being staged) and enables jeryu-auto-stage.timer.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
dest="$HOME/.local/share/jeryu-auto-stage"
units="$HOME/.config/systemd/user"
mkdir -p "$dest" "$units"
install -m 755 "$here/auto-stage.sh" "$dest/auto-stage.sh"
install -m 644 "$here/systemd/jeryu-auto-stage.service" "$here/systemd/jeryu-auto-stage.timer" "$units/"
systemctl --user daemon-reload
systemctl --user enable --now jeryu-auto-stage.timer
systemctl --user list-timers jeryu-auto-stage.timer --no-pager
