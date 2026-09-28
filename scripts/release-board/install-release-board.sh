#!/usr/bin/env bash
# install-release-board.sh — install the release-board collector for the current user on the
# host that runs releases: copies collect.sh and lib.sh to ~/.local/share/jeryu-release-board/
# and enables jeryu-release-board.timer. Release scripts find the installed collect.sh there
# and run it with --trigger release when they finish.
#
# What to collect is yours to configure, and none of it belongs in this repository:
#   ~/.config/jeryu/release-board.env          JERYU_BASE=<the forge's URL>, and any secrets an
#                                              adapter needs (a token file path, API tokens)
#   ~/.config/jeryu/release-board/families/    one adapter per family (see examples/acme.sh)
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
dest="$HOME/.local/share/jeryu-release-board"
config="$HOME/.config/jeryu"
units="$HOME/.config/systemd/user"
mkdir -p "$dest" "$units" "$config/release-board/families"
chmod 700 "$config/release-board"
install -m 755 "$here/collect.sh" "$dest/collect.sh"
install -m 644 "$here/lib.sh" "$dest/lib.sh"
git -C "$here" rev-parse --short=12 HEAD >"$dest/VERSION" 2>/dev/null || true
if [ ! -e "$config/release-board.env" ]; then
  install -m 600 /dev/null "$config/release-board.env"
  echo "created $config/release-board.env: set JERYU_BASE (and JERYU_BOARD_TOKEN_FILE) there"
fi
ls "$config/release-board/families/"*.sh >/dev/null 2>&1 \
  || echo "no adapters yet in $config/release-board/families/: copy and edit $here/examples/acme.sh"
install -m 644 "$here/systemd/jeryu-release-board.service" "$here/systemd/jeryu-release-board.timer" "$units/"
systemctl --user daemon-reload
systemctl --user enable --now jeryu-release-board.timer
systemctl --user list-timers jeryu-release-board.timer --no-pager
