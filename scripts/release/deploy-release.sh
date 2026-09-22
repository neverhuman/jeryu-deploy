#!/usr/bin/env bash
# deploy-release.sh RELEASE_ID — switch the forge host to a release staged by
# stage-release.sh, and record it on the forge as a GitHub-shaped deployment of
# jeryu/jeryu-deploy to environment "production":
#
#   1. POST /repos/jeryu/jeryu-deploy/deployments (sha, payload: release id,
#      previous release, binary digest, the pinned jeryu-web commit and dist
#      hash, host), then an in_progress status.
#   2. Run the staged switch.sh on the forge host.
#   3. Append success, or failure (switch.sh prints its own rollback command).
#      The forge's auto_inactive retires the previous production deployment.
#
# Everything switch.sh prints is shown live and kept in
# ~/.local/state/jeryu-release/logs/<release>-<UTC stamp>.log (0600, newest 30;
# JERYU_RELEASE_LOG_DIR overrides the directory). Both statuses carry the log's
# path (log_path); a failure's description adds the log's last meaningful line
# and the status carries its last 20 lines (log_tail), which the forge puts in
# the deploy.status event's detail.
#
# The record never decides the deploy. If the forge that is live before the
# switch cannot record it (for instance the release that introduces the
# deployments API), it is recorded right after the switch instead; if it still
# cannot be, the switch stands and the script says so.
#
# Recording needs an admin token: JERYU_DEPLOY_TOKEN_FILE (default
# ~/.config/jeryu/credentials/git-neverhuman-org-alton2.pat). It is read into a
# 0600 curl config, never put on argv. Env: JERYU_BUILD_HOST (xbabe2),
# JERYU_FORGE_HOST (atomicsoul), JERYU_FORGE_URL (https://git.neverhuman.org).
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
rel="${1:?usage: deploy-release.sh RELEASE_ID}"
[[ "$rel" =~ ^prod-[0-9]{8}T[0-9]{6}Z-[0-9a-f]+-unsigned$ ]] || { echo "not a release id: $rel" >&2; exit 1; }
build_host="${JERYU_BUILD_HOST:-xbabe2}"
forge_host="${JERYU_FORGE_HOST:-atomicsoul}"
forge="${JERYU_FORGE_URL:-https://git.neverhuman.org}"
token_file="${JERYU_DEPLOY_TOKEN_FILE:-$HOME/.config/jeryu/credentials/git-neverhuman-org-alton2.pat}"
repo=/repos/jeryu/jeryu-deploy

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
umask 077
[[ -r "$token_file" ]] || { echo "deploy token $token_file is not readable" >&2; exit 1; }
printf 'header = "Authorization: Bearer %s"\n' "$(cat "$token_file")" >"$tmp/curl.cfg"
api() { # METHOD PATH [JSON-FILE]
  local args=(--silent --show-error --max-time 60 -X "$1" --config "$tmp/curl.cfg" -H 'Accept: application/json')
  [[ -z "${3:-}" ]] || args+=(-H 'Content-Type: application/json' --data "@$3")
  curl "${args[@]}" "$forge$2"
}

meta="$(ssh "$build_host" "ssh -n $forge_host 'set -e; d=~/.jeryu/incoming/$rel; cat \$d/RELEASE.txt; echo binary_sha256=\$(sha256sum \$d/bundle/jeryu | cut -c1-64); echo live=\$(readlink ~/.jeryu/bin/jeryu)'")"
field() { sed -n "s/^$1=//p" <<<"$meta" | head -1; }
sha="$(field jeryu_deploy_commit | cut -d' ' -f1)"
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || { echo "staged RELEASE.txt has no 40-hex jeryu_deploy_commit" >&2; exit 1; }

# Empty for a release staged before jeryu-web was pinned by commit.
web_commit="$(field jeryu_web_commit)"; web_sha="$(field web_dist_sha256)"

jq -n --arg sha "$sha" --arg rel "$rel" --arg prev "$(field rollback_target)" \
  --arg bin "$(field binary_sha256)" --arg live "$(field live)" --arg host "$forge_host" \
  --arg web "$web_commit" --arg web_sha "$web_sha" \
  '{sha:$sha, ref:"main", environment:"production", description:("release " + $rel),
    payload:{release:$rel, previous_release:$prev, previous_binary:$live, binary_sha256:$bin,
             jeryu_web_commit:$web, web_dist_sha256:$web_sha, host:$host, signed:false}}' >"$tmp/deployment.json"

record() {
  local created
  created="$(api POST "$repo/deployments" "$tmp/deployment.json" || true)"
  deployment_id="$(jq -er '.id | select(type == "number")' <<<"$created" 2>/dev/null)" || {
    deployment_id=""
    echo "[receipt] not recorded: $(jq -r '.message // "no JSON"' <<<"$created" 2>/dev/null | head -c 120)"
    return 0
  }
  echo "[receipt] deployment $deployment_id recorded for $rel at ${sha:0:12}"
}
status() { # STATE DESCRIPTION [LOG_PATH [LOG_TAIL]]
  [[ -n "$deployment_id" ]] || return 0
  jq -n --arg s "$1" --arg d "$2" --arg url "$forge" --arg path "${3:-}" --arg tail "${4:-}" \
    '{state:$s, description:$d, environment_url:$url}
     + (if $path != "" then {log_path:$path} else {} end)
     + (if $tail != "" then {log_tail:$tail} else {} end)' >"$tmp/status.json"
  api POST "$repo/deployments/$deployment_id/statuses" "$tmp/status.json" \
    | jq -r '"[receipt] status \(.state // "not recorded: \(.message // "?")")"'
}

log_dir="${JERYU_RELEASE_LOG_DIR:-$HOME/.local/state/jeryu-release/logs}"
mkdir -p "$log_dir"; chmod 700 "$log_dir"
log="$log_dir/$rel-$(date -u +%Y%m%dT%H%M%SZ).log"
: >"$log"; chmod 600 "$log"
find "$log_dir" -maxdepth 1 -type f -name '*.log' -printf '%T@ %p\n' | sort -rn | tail -n +31 \
  | cut -d' ' -f2- | xargs -r rm -f --
# The log without colour codes or blank lines, and never the deploy token.
clean_log() {
  local text
  text="$(sed -E $'s/\x1b\\[[0-9;?]*[A-Za-z]//g; s/\r//g' "$log" | grep -v '^[[:space:]]*$' || true)"
  printf '%s\n' "${text//"$(cat "$token_file")"/[redacted]}"
}

deployment_id=""
record
status in_progress "switching $forge_host to $rel"
set +e
ssh "$build_host" "ssh $forge_host 'bash ~/.jeryu/incoming/$rel/switch.sh'" 2>&1 | tee -a "$log"
rc=${PIPESTATUS[0]}
set -e
echo "[receipt] switch output kept in $log"
if [[ $rc == 0 ]]; then
  if [[ -z "$deployment_id" ]]; then
    echo "[receipt] retrying against the forge that just started"
    record
  fi
  status success "live on $forge_host" "$log"
else
  why="$(clean_log | grep -v 'rollback: bash ' | tail -n 1 | cut -c1-300)"
  status failure "switch.sh exited $rc${why:+: $why}" "$log" "$(clean_log | tail -n 20)"
  exit "$rc"
fi
