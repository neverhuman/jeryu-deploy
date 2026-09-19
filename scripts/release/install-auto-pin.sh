#!/usr/bin/env bash
# install-auto-pin.sh — install the auto-pin timer for the current user on the release host.
# Copies auto-pin.sh to ~/.local/share/jeryu-auto-pin/ (the build recipe itself is always taken
# from jeryu-deploy main) and enables jeryu-auto-pin.timer. To turn it off again:
#   systemctl --user disable --now jeryu-auto-pin.timer
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
dest="$HOME/.local/share/jeryu-auto-pin"
units="$HOME/.config/systemd/user"
mkdir -p "$dest" "$units"
install -m 755 "$here/auto-pin.sh" "$dest/auto-pin.sh"
install -m 644 "$here/systemd/jeryu-auto-pin.service" "$here/systemd/jeryu-auto-pin.timer" "$units/"
systemctl --user daemon-reload
systemctl --user enable --now jeryu-auto-pin.timer
systemctl --user list-timers jeryu-auto-pin.timer --no-pager
