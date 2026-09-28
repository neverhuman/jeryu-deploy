# shellcheck shell=bash disable=SC2034,SC2154  # sets and reads the per-family variables collect.sh owns
# families/veox-ai.sh — the veox-ai board: the cloud app on xbabe0 and every fleet node, the
# cloud appliance by channel, and the website. Sourced by collect.sh; reads only.
#
# Env: VEOX_SRV (/srv/veox), VEOX_FLEET_NODES ("xbabe1 xbabe3"), VEOX_REGISTRY
# (http://localhost:5000), VEOX_WWW_CONVOY (~/ai-veox-www/.claude/worktrees/_convoy),
# CLOUDFLARE_API_TOKEN + CLOUDFLARE_ACCOUNT_ID (optional; the website's production stage says
# `unverified` without them).

APP_REPO=veox-ai/ai-veox-app
WWW_REPO=veox-ai/ai-veox-www
APPLIANCE=veox/cloud-appliance-cuda

# app_version MIRROR SHA — "v0.8.12 · 96d8374" when SHA is a stable tag, else "96d8374".
app_version() {
  local tag
  tag="$(git -C "$1" describe --tags --exact-match --match 'v[0-9]*.[0-9]*.[0-9]*' "$2" 2>/dev/null)"
  if [ -n "$tag" ]; then printf '%s · %s' "$tag" "$(short "$2")"; else short "$2"; fi
}

# node_containers NODE — "name<TAB>sha12<TAB>appliance-digest" for each veox worker/gateway.
node_containers() {
  timeout 20 ssh -o BatchMode=yes -o ConnectTimeout=8 "$1" '
    for c in $(docker ps --format "{{.Names}}" | grep -E "^veox-(prod|dev)-(worker|gateway)"); do
      img=$(docker inspect "$c" --format "{{.Config.Image}}")
      app=$(docker inspect "$c" --format "{{range .Config.Env}}{{println .}}{{end}}" | sed -n "s/^VEOX_APPLIANCE_IMAGE=.*@//p")
      printf "%s\t%s\t%s\n" "$c" "${img##*:}" "$app"
    done' 2>/dev/null
}

# channel_digest TAG — the registry digest a channel tag points at.
channel_digest() {
  curl --silent --max-time 10 -I \
    -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json' \
    "$registry/v2/$APPLIANCE/manifests/$1" 2>/dev/null | tr -d '\r' | sed -n 's/^[Dd]ocker-[Cc]ontent-[Dd]igest: //p'
}

collect_veox_ai() {
  local srv="${VEOX_SRV:-/srv/veox}" nodes="${VEOX_FLEET_NODES:-xbabe1 xbabe3}"
  registry="${VEOX_REGISTRY:-http://localhost:5000}"
  local app www main prod_sha stage_sha dev_sha preview=""
  app="$(mirror "$APP_REPO")" || return 1
  main="$(git -C "$app" rev-parse --verify -q refs/heads/main)"
  git -C "$app" rev-parse --verify -q refs/heads/preview >/dev/null && preview="$(git -C "$app" rev-parse refs/heads/preview)"
  prod_sha="$(cat "$srv/deployed-prod.sha" 2>/dev/null)" || problem "xbabe0 prod" "no $srv/deployed-prod.sha"
  stage_sha="$(cat "$srv/deployed-stage.sha" 2>/dev/null)" || problem "xbabe0 stage" "no $srv/deployed-stage.sha"
  dev_sha="$(cat "$srv/deployed-dev.sha" 2>/dev/null)" || problem "xbabe0 dev" "no $srv/deployed-dev.sha"

  # What each fleet node runs, by environment and role.
  local -a prod_targets=() dev_targets=() prod_app_targets=() dev_app_targets=()
  local node name sha12 digest env role full ver st
  prod_targets+=("$(target 'xbabe0 · cloud.veox.ai + mcp.veox.ai' "$(app_version "$app" "$prod_sha")" ok)")
  dev_targets+=("$(target 'xbabe0 · dev-xbabe0.veox.ai' "$(app_version "$app" "$dev_sha")" ok)")
  local prod_digest dev_digest
  prod_digest="$(cut -d@ -f2 "$srv/deployed-prod.appliance" 2>/dev/null)"
  dev_digest="$(cut -d@ -f2 "$srv/deployed-dev.appliance" 2>/dev/null)"
  prod_app_targets+=("$(target 'xbabe0 · cloud.veox.ai' "${prod_digest:7:8}" ok)")
  dev_app_targets+=("$(target 'xbabe0 dev' "${dev_digest:7:8}" ok)")
  for node in $nodes; do
    local rows
    rows="$(node_containers "$node")"
    if [ -z "$rows" ]; then
      problem "$node" "could not list its veox containers over ssh"
      prod_targets+=("$(target "$node" "" none)"); dev_targets+=("$(target "$node" "" none)")
      continue
    fi
    while IFS=$'\t' read -r name sha12 digest; do
      env="${name#veox-}"; env="${env%%-*}"; role="${name#veox-"$env"-}"; role="${role%-*}"
      full="$(git -C "$app" rev-parse --verify -q "$sha12^{commit}" 2>/dev/null)"
      ver="$( [ -n "$full" ] && app_version "$app" "$full" || printf '%s' "$sha12")"
      if [ "$env" = prod ]; then
        st=bad; [ -n "$full" ] && [ "$full" = "$prod_sha" ] && st=ok
        prod_targets+=("$(target "$node · prod $role" "$ver" "$st")")
        if [ "$role" = worker ]; then
          st=bad; [ "$digest" = "$prod_digest" ] && st=ok
          prod_app_targets+=("$(target "$node · prod worker" "${digest:7:8}" "$st")")
        fi
      else
        st=warn; [ -n "$full" ] && [ "$full" = "$dev_sha" ] && st=ok
        dev_targets+=("$(target "$node · dev $role" "$ver" "$st")")
        if [ "$role" = worker ]; then
          st=warn; [ "$digest" = "$dev_digest" ] && st=ok
          dev_app_targets+=("$(target "$node · dev worker" "${digest:7:8}" "$st")")
        fi
      fi
    done <<<"$rows"
  done
  local prod_json dev_json prod_state dev_state n_behind status ships next_tag newest
  prod_json="$(printf '%s\n' "${prod_targets[@]}" | jq -cs .)"
  dev_json="$(printf '%s\n' "${dev_targets[@]}" | jq -cs .)"
  prod_state="$(worst_of "$prod_json")"
  dev_state="$(worst_of "$dev_json")"
  n_behind="$(behind "$app" "$prod_sha" "$main")"
  status="in sync"; [ "$prod_state" = bad ] && status="skew"
  [ -n "$n_behind" ] && [ "$n_behind" -gt 0 ] && status="$status · $n_behind behind"
  newest="$(git -C "$app" tag --list 'v*' --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1)"
  next_tag="$(awk -F. -v OFS=. '{ $NF = $NF + 1; print }' <<<"${newest:-v0.0.0}")"
  ships="$(git -C "$app" log --format='%h · %s' "$prod_sha..$main" 2>/dev/null | lines_json)"

  lane cloud-app "Cloud app" "$APP_REPO · built on xbabe0 by the deploy timer" veox-ai false \
    "$(stage id=main name=main version="$(short "$main")" state=none status=source known=derived)" \
    "$(stage id=dev name=dev version="$(short "$dev_sha")" state="$dev_state" \
        status="$( [ "$dev_state" = ok ] && echo "in sync" || echo skew)$( [ -n "$preview" ] && echo " · preview")" \
        known=reported targets="$dev_json" automatic=true \
        promote_cmd="merge a PR to forge main, or push a branch to preview; nodes: deploy/fleet-roll.sh" \
        forge_repo="$APP_REPO" forge_env=dev)" \
    "$(stage id=stage name=stage version="$(short "$stage_sha")" \
        state="$( [ "$stage_sha" = "$main" ] && echo ok || echo warn)" \
        status="$( [ "$stage_sha" = "$main" ] && echo "in sync" || echo "$(behind "$app" "$stage_sha" "$main") behind")" \
        known=reported parallel=true automatic=true targets="[$(target 'xbabe0 · 127.0.0.1:8082' "$(short "$stage_sha")" ok)]" \
        promote_cmd="merge a PR to forge main (runs beside dev, not after it)" forge_repo="$APP_REPO" forge_env=stage)" \
    "$(stage id=prod name=prod version="$(app_version "$app" "$prod_sha")" state="$prod_state" status="$status" \
        known=reported targets="$prod_json" human_only=true ships="$ships" \
        promote_cmd="just tag-release $next_tag && git push origin $next_tag   # then: VEOX_ENV=prod deploy/fleet-roll.sh" \
        rollback="a new, higher v* tag on the old commit" forge_repo="$APP_REPO" forge_env=production)"

  # The appliance: one digest per channel, and the digest each target actually runs.
  local accepted d_dev d_canary d_stable d_prev
  accepted="$(curl --silent --max-time 10 "$registry/v2/$APPLIANCE/tags/list" 2>/dev/null \
    | jq -r '.tags // [] | map(select(startswith("accepted-gpu-"))) | sort | last // empty')"
  [ -n "$accepted" ] || problem "registry" "could not list $APPLIANCE tags at $registry"
  d_dev="$(channel_digest dev)"; d_canary="$(channel_digest canary)"; d_stable="$(channel_digest stable)"
  d_prev="$(channel_digest previous-stable)"
  local adev astable
  adev="$(printf '%s\n' "${dev_app_targets[@]}" | jq -cs .)"
  astable="$(printf '%s\n' "${prod_app_targets[@]}" | jq -cs .)"
  appliance_lane_json="$(
    lanes_file=/dev/stdout
    lane cloud-appliance "Cloud appliance" "$APPLIANCE · engine from jain-web tags · moves by channel" veox-ai false \
      "$(stage id=accepted name=accepted version="$( [ -n "$accepted" ] && channel_digest "$accepted" | cut -c8-15)" \
          state=none status="${accepted:-unknown}" known=host)" \
      "$(stage id=dev name="dev channel" version="${d_dev:7:8}" state="$(worst_of "$adev")" \
          status="$( [ "$(worst_of "$adev")" = ok ] && echo "in sync" || echo skew)" known=host \
          targets="$adev" human_only=true promote_cmd="scripts/promote-cloud-appliance.sh gpu=<accepted image>")" \
      "$(stage id=canary name="canary → stage" version="${d_canary:7:8}" state=ok status="in sync" known=host \
          targets="[$(target 'xbabe0 stage' "${d_canary:7:8}" ok)]" human_only=true \
          promote_cmd="scripts/promote-cloud-appliance.sh --deploy promote gpu canary")" \
      "$(stage id=stable name="stable → prod" version="${d_stable:7:8}" state="$(worst_of "$astable")" \
          status="$( [ "$(worst_of "$astable")" = ok ] && echo "in sync" || echo skew)" known=host \
          targets="$astable" human_only=true \
          promote_cmd="VEOX_PROMOTE_DEPLOY_PROD=1 scripts/promote-cloud-appliance.sh --deploy promote gpu stable" \
          rollback="promote-cloud-appliance.sh rollback gpu stable → previous-stable ${d_prev:7:8}")"
  )"
  printf '%s\n' "$appliance_lane_json" >>"$lanes_file"

  # The website: forge main, the dev convoy, and what Cloudflare Pages serves.
  local www_main convoy pages="" pages_at="" gh_main
  www="$(mirror "$WWW_REPO")" && www_main="$(git -C "$www" rev-parse --verify -q refs/heads/main)"
  convoy="$(git -C "${VEOX_WWW_CONVOY:-$HOME/ai-veox-www/.claude/worktrees/_convoy}" rev-parse HEAD 2>/dev/null)" \
    || problem "dev-www" "could not read the convoy checkout"
  if [ -n "${CLOUDFLARE_API_TOKEN:-}" ] && [ -n "${CLOUDFLARE_ACCOUNT_ID:-}" ]; then
    local cf
    cf="$(curl --silent --max-time 15 -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
      "https://api.cloudflare.com/client/v4/accounts/$CLOUDFLARE_ACCOUNT_ID/pages/projects/veox-www" 2>/dev/null)"
    pages="$(jq -r '.result.canonical_deployment.deployment_trigger.metadata.commit_hash // empty' <<<"$cf" 2>/dev/null)"
    pages_at="$(jq -r '.result.canonical_deployment.created_on // empty' <<<"$cf" 2>/dev/null)"
    [ -n "$pages" ] || problem "cloudflare" "could not read the veox-www production deployment"
  else
    problem "cloudflare" "no CLOUDFLARE_API_TOKEN/CLOUDFLARE_ACCOUNT_ID for the collector; the website's production is not read"
  fi
  gh_main="$(timeout 20 git ls-remote https://github.com/neverhuman/ai-veox-www.git refs/heads/main 2>/dev/null | cut -f1)"
  local wstate=none wstatus="not read" wknown=unverified
  if [ -n "$pages" ]; then
    wknown=host
    if [ "$pages" = "$www_main" ]; then wstate=ok; wstatus="in sync"; else wstate=warn; wstatus="behind forge main"; fi
    if [ -n "$gh_main" ] && [ "$gh_main" != "$www_main" ]; then
      wstate=bad; wstatus="forge main is not on GitHub, which is what deploys"
    fi
  fi
  lane website "Website" "$WWW_REPO · Cloudflare Pages project veox-www, deployed from GitHub main" veox-ai false \
    "$(stage id=main name=main version="$(short "$www_main")" state=none status=source known=derived)" \
    "$(stage id=dev-www name=dev-www version="$(short "$convoy")" \
        state="$( [ -n "$convoy" ] && [ "$convoy" = "$www_main" ] && echo ok || echo warn)" \
        status="$( [ -n "$convoy" ] && [ "$convoy" = "$www_main" ] && echo "in sync" || echo "not main")" known=host \
        targets="[$(target 'xbabe0 · dev-www.veox.ai' "$(short "$convoy")" ok)]" automatic=true \
        promote_cmd="merge to main (the convoy timer follows in 5 min)")" \
    "$(stage id=production name=production version="$(short "$pages")" state="$wstate" status="$wstatus" known="$wknown" \
        targets="[$(target 'veox.ai' "$(short "$pages")${pages_at:+ · ${pages_at:0:10}}" "$( [ -n "$pages" ] && echo ok || echo none)")]" \
        human_only=true promote_cmd="merge to GitHub main → GitHub Actions → wrangler pages deploy")"

  # Work: todos live in prod when prod's commit (or the served website) holds their change.
  main_specs="$app=$main"$'\n'"${www:+$www=$www_main}"
  work_json="$(work_summary veox-ai \
    "Matched by Todo: trailer, then by content (a commit whose change prod's tree already holds), because forge main was reset by tree." \
    "Not linkable: the legacy todo/ folders in ai-veox-app and ai-veox-www." \
    "$app=$prod_sha" ${www:+"$www=${pages:-$www_main}"})" || work_json=null
  [ -n "$work_json" ] || work_json=null

  local covered="no"
  git -C "$app" show "$main:crates/veox-app-api/web/releases.json" 2>/dev/null \
    | jq -e --arg v "$newest" '.releases | any(.version == $v)' >/dev/null && covered="yes"
  notes_json="$(jq -cn --argjson items "$ships" --arg newest "$newest" --arg covered "$covered" '
    {title: "Cloud app: what promoting prod would ship", items: $items,
     coverage: ("Hand-written notes live in crates/veox-app-api/web/releases.json; the newest release " + $newest
       + (if $covered == "yes" then " has an entry." else " has NO entry." end)
       + " The website and the appliance have none.")}')"
  summary="prod $(app_version "$app" "$prod_sha")$( [ -n "$n_behind" ] && [ "$n_behind" -gt 0 ] && echo " · $n_behind commit(s) waiting on main")$( [ "$prod_state" = bad ] && echo " · fleet skew")"
}
