#!/usr/bin/env bash
# install-release-actions.sh — install the release-actions timer for the current user on the
# release host. Copies release-action.sh to ~/.local/share/jeryu-release-actions/ (the deploy
# recipe itself is always taken from the staged release commit) and enables jeryu-release-actions.timer,
# which polls the forge every 30 seconds for steps a deployer asked for from the attention inbox
# (docs/release-actions.md). Once it runs, auto-stage names the installed script in its "deploy
# with" line, and the forge offers a Deploy button while the repository poller check-in is fresh.
# The deploy token (JERYU_DEPLOY_TOKEN_FILE in ~/.config/jeryu/release.env) must belong to a
# deployer: an admin named in the forge's JERYU_DEPLOYERS.
# To turn the button off again:  systemctl --user disable --now jeryu-release-actions.timer
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
dest="$HOME/.local/share/jeryu-release-actions"
units="$HOME/.config/systemd/user"
mkdir -p "$dest" "$units"
install -m 755 "$here/release-action.sh" "$dest/release-action.sh"
install -m 644 "$here/redact-release-result.jq" "$dest/redact-release-result.jq"
install -m 644 "$here/systemd/jeryu-release-actions.service" "$here/systemd/jeryu-release-actions.timer" "$units/"
systemctl --user daemon-reload
systemctl --user enable --now jeryu-release-actions.timer
systemctl --user list-timers jeryu-release-actions.timer --no-pager
