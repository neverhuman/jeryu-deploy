#!/usr/bin/env bash
# install-jankurai-audit-runner.sh — install the jankurai audit runner for the current user on a
# gate runner host: unpacks jankurai-audit-runner.sh and what it sources, from this checkout's HEAD,
# into ~/.local/share/jeryu-jankurai-audit-runner/ and enables jeryu-jankurai-audit-runner.timer,
# which claims one queued audit every 30 seconds (docs/governed-jankurai.md, "Runners do the work").
#
# The files are unpacked with `git archive`, not cloned: the host gets the exact runner of the
# installed commit (recorded in VERSION) and no second working copy. Re-run it to update.
#
# What it needs is yours to configure, in ~/.config/jeryu/jankurai-audit-runner.env (mode 600,
# created empty on the first run): JERYU_FORGE_TOKEN_FILE (the PAT of a runner identity the forge
# allows to score, JERYU_JANKURAI_SCORERS on the forge; mode 600; required), JERYU_API (the forge's
# API base; the runner's default otherwise) and JERYU_AUDIT_RUNNER_ID (optional). The governed
# jankurai must already be installed on the host (ops/ci/lib.sh `require_jankurai` checks it).
#
# Env: JERYU_SYSTEMCTL (default systemctl; tests point it at a stand-in).
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
systemctl="${JERYU_SYSTEMCTL:-systemctl}"
dest="$HOME/.local/share/jeryu-jankurai-audit-runner"
env_file="$HOME/.config/jeryu/jankurai-audit-runner.env"
units="$HOME/.config/systemd/user"
files=(ops/ci/jankurai-audit-runner.sh ops/ci/submit-jankurai-score.sh ops/ci/lib.sh ops/ci/hosted-git-env.sh
  .cargo/hosted-gitconfig)

commit="$(git -C "$repo" rev-parse --verify HEAD)"
git -C "$repo" diff --quiet HEAD -- "${files[@]}" \
  || { echo "refusing: the runner files differ from HEAD; commit them first, so VERSION names what runs" >&2; exit 1; }
stage="$(mktemp -d "${dest}.new.XXXXXX" 2>/dev/null || { mkdir -p "$(dirname "$dest")"; mktemp -d "${dest}.new.XXXXXX"; })"
trap 'rm -rf "$stage"' EXIT
git -C "$repo" archive --format=tar "$commit" -- "${files[@]}" | tar -x -C "$stage"
printf '%s\n' "$commit" >"$stage/VERSION"
rm -rf "$dest.old"; [ ! -e "$dest" ] || mv "$dest" "$dest.old"
mv "$stage" "$dest"; rm -rf "$dest.old"

mkdir -p "$(dirname "$env_file")" "$units"
if [ ! -e "$env_file" ]; then
  install -m 600 /dev/null "$env_file"
  echo "created $env_file: set JERYU_FORGE_TOKEN_FILE (and JERYU_API) there, then run this again"
fi
install -m 644 "$here/systemd/jeryu-jankurai-audit-runner.service" "$here/systemd/jeryu-jankurai-audit-runner.timer" "$units/"
"$systemctl" --user daemon-reload
if ! grep -qE '^JERYU_FORGE_TOKEN_FILE=.+' "$env_file"; then
  echo "installed ${commit:0:12}; the timer stays off until $env_file sets JERYU_FORGE_TOKEN_FILE" >&2
  exit 0
fi
"$systemctl" --user enable --now jeryu-jankurai-audit-runner.timer
echo "installed ${commit:0:12}; jeryu-jankurai-audit-runner.timer enabled"
