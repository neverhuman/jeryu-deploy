#!/usr/bin/env bash
# deploy-release.sh RELEASE_ID — switch the forge host to a release staged by
# stage-release.sh, and record it on the forge as a GitHub-shaped deployment of
# jeryu/jeryu-deploy to environment "production":
#
#   1. POST /repos/jeryu/jeryu-deploy/deployments (sha, payload: release id,
#      previous release, binary digest, host), then an in_progress status.
#   2. Run the staged switch.sh on the forge host.
#   3. Append success, or failure (switch.sh prints its own rollback command).
#      The forge's auto_inactive retires the previous production deployment.
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

jq -n --arg sha "$sha" --arg rel "$rel" --arg prev "$(field rollback_target)" \
  --arg bin "$(field binary_sha256)" --arg live "$(field live)" --arg host "$forge_host" \
  '{sha:$sha, ref:"main", environment:"production", description:("release " + $rel),
    payload:{release:$rel, previous_release:$prev, previous_binary:$live, binary_sha256:$bin,
             host:$host, signed:false}}' >"$tmp/deployment.json"

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
status() { # STATE DESCRIPTION
  [[ -n "$deployment_id" ]] || return 0
  jq -n --arg s "$1" --arg d "$2" --arg url "$forge" \
    '{state:$s, description:$d, environment_url:$url}' >"$tmp/status.json"
  api POST "$repo/deployments/$deployment_id/statuses" "$tmp/status.json" \
    | jq -r '"[receipt] status \(.state // "not recorded: \(.message // "?")")"'
}

deployment_id=""
record
status in_progress "switching $forge_host to $rel"
if ssh "$build_host" "ssh $forge_host 'bash ~/.jeryu/incoming/$rel/switch.sh'"; then
  if [[ -z "$deployment_id" ]]; then
    echo "[receipt] retrying against the forge that just started"
    record
  fi
  status success "live on $forge_host"
else
  rc=$?
  status failure "switch.sh exited $rc; see its output for the rollback command"
  exit "$rc"
fi
