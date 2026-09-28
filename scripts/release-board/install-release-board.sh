#!/usr/bin/env bash
# install-release-board.sh — install the release-board collector for the current user on the
# release host (xbabe0): copies collect.sh, lib.sh and families/ to
# ~/.local/share/jeryu-release-board/ and enables jeryu-release-board.timer. Release scripts find
# the installed collect.sh there and run it with --trigger release when they finish.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
dest="$HOME/.local/share/jeryu-release-board"
units="$HOME/.config/systemd/user"
mkdir -p "$dest/families" "$units"
install -m 755 "$here/collect.sh" "$dest/collect.sh"
install -m 644 "$here/lib.sh" "$dest/lib.sh"
install -m 644 "$here"/families/*.sh "$dest/families/"
git -C "$here" rev-parse --short=12 HEAD >"$dest/VERSION" 2>/dev/null || true
install -m 644 "$here/systemd/jeryu-release-board.service" "$here/systemd/jeryu-release-board.timer" "$units/"
systemctl --user daemon-reload
systemctl --user enable --now jeryu-release-board.timer
systemctl --user list-timers jeryu-release-board.timer --no-pager
