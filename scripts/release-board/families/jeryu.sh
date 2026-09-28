# shellcheck shell=bash disable=SC2034,SC2154  # sets and reads the per-family variables collect.sh owns
# families/jeryu.sh — the jeryu board: the forge server (jeryu-deploy + the pinned web UI and
# crates) from shift branches to production, and the tools that run straight from main.
# Sourced by collect.sh; reads only.
#
# Env: JERYU_AUTO_STAGE_STATE (~/.local/state/jeryu-auto-stage), JERYU_GATE_HOST (xbabe2),
# JERYU_REDTEAM_DIR (~/pr-redteam).

DEPLOY_REPO=jeryu/jeryu-deploy
WEB_REPO=jeryu/jeryu-web
CRATES=(jeryu-core jeryu-ci-runner jeryu-intelligence jeryu-jira jeryu-release-ops)

# lock_crate MIRROR REV CRATE-REPO — "tag<TAB>sha" that jeryu-deploy's Cargo.lock at REV resolves
# CRATE-REPO to (the build consumes Cargo.lock, not jeryu-split.lock.toml's crate rows).
lock_crate() {
  git -C "$1" show "$2:Cargo.lock" 2>/dev/null \
    | sed -n "s|^source = \"git+[^\"]*/$3\(\.git\)\{0,1\}?tag=\([^#\"]*\)#\([0-9a-f]\{40\}\)\"|\2\t\3|p" | head -1
}

collect_jeryu() {
  local deploy web env_json prod_sha prod_rel prod_at web_prod prev_rel main web_main staged
  deploy="$(mirror "$DEPLOY_REPO")" || return 1
  main="$(git -C "$deploy" rev-parse --verify -q refs/heads/main)"
  env_json="$(forge_get "/api/v3/repos/$DEPLOY_REPO/environments")" \
    || problem "forge environments" "could not read $DEPLOY_REPO environments"
  prod_sha="$(jq -r '.environments[]? | select(.name == "production") | .current.deployment.sha // empty' <<<"${env_json:-{\}}")"
  prod_rel="$(jq -r '.environments[]? | select(.name == "production") | .current.deployment.payload.release // empty' <<<"${env_json:-{\}}")"
  prod_at="$(jq -r '.environments[]? | select(.name == "production") | .current.deployment.created_at // empty' <<<"${env_json:-{\}}")"
  web_prod="$(jq -r '.environments[]? | select(.name == "production") | .current.deployment.payload.jeryu_web_commit // empty' <<<"${env_json:-{\}}")"
  prev_rel="$(jq -r '.environments[]? | select(.name == "production") | .current.deployment.payload.previous_release // empty' <<<"${env_json:-{\}}")"
  staged="$(cat "${JERYU_AUTO_STAGE_STATE:-$HOME/.local/state/jeryu-auto-stage}/latest" 2>/dev/null)" \
    || problem "auto-stage" "no staged release recorded"

  # Shift branches: the newest one per kind that still holds work main lacks.
  local shift_stage="" br ahead pr_count
  for br in $(git -C "$deploy" for-each-ref --sort=-refname --format='%(refname:short)' 'refs/heads/nightshift/*' 'refs/heads/bulletshift/*'); do
    ahead="$(git -C "$deploy" cherry "$main" "$br" 2>/dev/null | grep -c '^+')"
    [ "${ahead:-0}" -gt 0 ] || continue
    pr_count="$(forge_get "/api/v3/repos/$DEPLOY_REPO/pulls?state=open" 2>/dev/null | jq --arg b "$br" '[.[]? | select(.head.ref == $b)] | length' 2>/dev/null)"
    shift_stage="$(stage id=shift name=shift version="$br" state=warn \
      status="$ahead unmerged$( [ "${pr_count:-0}" = 0 ] && echo ", no PR" || echo ", PR open")" known=derived \
      promote_cmd="todoq shift pr jeryu $br")"
    break
  done

  local p_behind p_state s_state
  p_behind="$(behind "$deploy" "$prod_sha" "$main")"
  p_state=none; [ -n "$prod_sha" ] && { p_state=ok; [ "${p_behind:-0}" -gt 0 ] && p_state=warn; }
  s_state=none; [ -n "$staged" ] && { s_state=ok; [ "$staged" = "$prod_rel" ] || s_state=warn; }
  local ships idle_envs
  ships="$(git -C "$deploy" log --format='%h · %s' "$prod_sha..$main" 2>/dev/null | lines_json)"
  idle_envs="$(jq -r '[.environments[]?.name] as $have | ["dev","canary","stable"] - $have | join(" · ")' <<<"${env_json:-{\}}")"
  lane forge-server "Forge server" "$DEPLOY_REPO + web + ${#CRATES[@]} crates · built on xbabe2 · served from atomicsoul" jeryu false \
    ${shift_stage:+"$shift_stage"} \
    "$(stage id=main name=main version="$(short "$main")" state=none status=source known=derived)" \
    "$(stage id=staged name=staged version="${staged:-none}" state="$s_state" \
        status="$( [ "$staged" = "$prod_rel" ] && echo "= prod" || echo "waiting for deploy-release.sh")" known=host automatic=true \
        promote_cmd="automatic (jeryu-auto-stage.timer, 5 min, once main is green)")" \
    "$(stage id=production name=production version="$(short "$prod_sha")${prod_at:+ · ${prod_at:0:16}}" state="$p_state" \
        status="${p_behind:-?} behind" known=reported human_only=true ships="$ships" \
        targets="[$(target 'atomicsoul · git.neverhuman.org' "${prod_rel:-unknown}" "$( [ -n "$prod_sha" ] && echo ok || echo none)")]" \
        promote_cmd="scripts/release/deploy-release.sh \$(cat ~/.local/state/jeryu-auto-stage/latest)" \
        rollback="~/.jeryu/releases/<release>/rollback.sh on atomicsoul → ${prev_rel:-unknown}" \
        forge_repo="$DEPLOY_REPO" forge_env=production)" \
    ${idle_envs:+"$(stage id=never-deployed name="$idle_envs" version="no deployment yet" state=none status="never deployed" known=reported never_deployed=true)"}

  # The web UI: main, the pin in jeryu-split.lock.toml, and what production's payload names.
  local web_pin w_state
  web="$(mirror "$WEB_REPO")" && web_main="$(git -C "$web" rev-parse --verify -q refs/heads/main)"
  web_pin="$(git -C "$deploy" show "$main:jeryu-split.lock.toml" 2>/dev/null \
    | awk '/^\[\[repo\]\]/ { name = "" } /^name *=/ { gsub(/[" ]/, ""); split($0, a, "="); name = a[2] }
           name == "jeryu-web" && /^commit *=/ { gsub(/[" ]/, ""); split($0, a, "="); print a[2]; exit }')"
  w_state=ok; [ "$(behind "$web" "$web_pin" "$web_main")" = 0 ] || w_state=warn
  lane web-ui "Web UI" "$WEB_REPO · pinned into the forge server by commit + bundle hash" jeryu false \
    "$(stage id=main name=main version="$(short "$web_main")" state=none status=source known=derived)" \
    "$(stage id=pinned name=pinned version="$(short "$web_pin")" state="$w_state" \
        status="$(behind "$web" "$web_pin" "$web_main") behind" known=derived automatic=true \
        promote_cmd='auto-pin opens "release: pin jeryu-web <sha7>"; the PR merges like any other')" \
    "$(stage id=production name=production version="$(short "$web_prod")" \
        state="$( [ -n "$web_prod" ] && [ "$web_prod" = "$web_pin" ] && echo ok || echo warn)" \
        status="$( [ -n "$web_prod" ] && [ "$web_prod" = "$web_pin" ] && echo "= pinned" || echo "pin not deployed")" known=reported)"

  # Pinned vs released: every crate as main has it, as Cargo.lock pins it, and as prod pins it.
  local rows=() crate m tag_sha tag sha prod_tag_sha ptag c_behind note live_specs=("$deploy=$prod_sha")
  [ -n "$web" ] && [ -n "$web_prod" ] && live_specs+=("$web=$web_prod")
  main_specs="$deploy=$main"$'\n'"${web:+$web=$web_main}"
  rows+=("$(jq -cn --arg w "$(short "$web_main")" --arg p "$(short "$web_pin")" --arg d "$(short "$web_prod")" \
    --argjson b "$(behind "$web" "$web_pin" "$web_main" | grep . || echo null)" \
    '{repo: "jeryu-web", cells: [$w, $p, $d], behind: $b}')")
  for crate in "${CRATES[@]}"; do
    m="$(mirror "jeryu/$crate")" || continue
    main_specs+=$'\n'"$m=$(git -C "$m" rev-parse --verify -q refs/heads/main)"
    tag_sha="$(lock_crate "$deploy" "$main" "$crate")"; tag="${tag_sha%%$'\t'*}"; sha="${tag_sha##*$'\t'}"
    prod_tag_sha="$(lock_crate "$deploy" "${prod_sha:-$main}" "$crate")"; ptag="${prod_tag_sha%%$'\t'*}"
    [ -n "${prod_tag_sha##*$'\t'}" ] && live_specs+=("$m=${prod_tag_sha##*$'\t'}")
    c_behind="$(behind "$m" "$sha" refs/heads/main)"
    note=""
    [ "${c_behind:-0}" -gt 0 ] && git -C "$m" tag --points-at refs/heads/main | grep -q . && note="a tag at main exists; the pin has not moved"
    rows+=("$(jq -cn --arg r "$crate" --arg mm "$(short "$(git -C "$m" rev-parse refs/heads/main)")" \
      --arg t "${tag#"$crate"-}" --arg pt "${ptag#"$crate"-}" --argjson b "${c_behind:-null}" --arg n "$note" \
      '{repo: $r, cells: [$mm, $t, $pt], behind: $b} + (if $n != "" then {note: $n} else {} end)')")
  done
  rows+=("$(jq -cn --arg m "$(short "$main")" --arg p "$(short "$prod_sha")" --argjson b "${p_behind:-null}" \
    '{repo: "jeryu-deploy", cells: [$m, "—", $p], behind: $b}')")
  pins_json="$(printf '%s\n' "${rows[@]}" | jq -cs '{note: "Crates read from Cargo.lock (what the build consumes); the web UI from jeryu-split.lock.toml; production from the deployed commit.", columns: ["Repo", "Main", "Pinned", "In prod", "Behind"], rows: .}')"

  # Tools that run straight from main: the gate runner, pr-redteam, jankurai.
  local gate_host="${JERYU_GATE_HOST:-xbabe2}" gate gate_main rt="${JERYU_REDTEAM_DIR:-$HOME/pr-redteam}" rt_head rt_dirty rt_remote
  gate="$(timeout 20 ssh -o BatchMode=yes -o ConnectTimeout=8 "$gate_host" 'jq -r .commit ~/gate-runner/installed-main.json' 2>/dev/null)" \
    || problem "$gate_host" "could not read the installed gate runner"
  gate_main="$(timeout 20 git ls-remote "$base/git/veox/jain-deploy.git" refs/heads/main 2>/dev/null | cut -f1)"
  rt_head="$(git -C "$rt" rev-parse --short=7 HEAD 2>/dev/null)"
  rt_dirty="$(git -C "$rt" status --porcelain 2>/dev/null | grep -c .)"
  rt_remote="$(git -C "$rt" remote 2>/dev/null | grep -c .)"
  local rt_state=ok rt_status="clean, tracked"
  if [ -z "$rt_head" ]; then rt_state=none; rt_status="not found"
  elif [ "$rt_remote" = 0 ] || [ "$rt_dirty" -gt 0 ]; then rt_state=bad; rt_status="local fork"; fi
  lane tools "Tools" 'installed per host · effectively "main is production"' jeryu false \
    "$(stage id=gate-runner name="gate runner" version="$(short "$gate")" \
        state="$( [ -n "$gate" ] && [ "$gate" = "$gate_main" ] && echo ok || echo warn)" \
        status="$( [ -n "$gate" ] && [ "$gate" = "$gate_main" ] && echo "= main" || echo "behind main")" known=host automatic=true \
        targets="[$(target "$gate_host · pr-gate-runner" "$(short "$gate") · auto-installs every 10 min" "$( [ "$gate" = "$gate_main" ] && echo ok || echo warn)")]")" \
    "$(stage id=pr-redteam name=pr-redteam version="${rt_head:-unknown}$( [ "${rt_dirty:-0}" -gt 0 ] && echo " + $rt_dirty edits")" \
        state="$rt_state" status="$rt_status" known=host human_only=true \
        targets="[$(target "$(hostname -s) · $rt" "$( [ "$rt_remote" = 0 ] && echo "no remote" || echo "tracked")" "$rt_state")]" \
        promote_cmd="ops/pr-redteam/install.sh from jeryu-ci-runner main")"

  work_json="$(work_summary jeryu "Matched by Todo: trailer, then by content. Live means production's release holds the change." "" "${live_specs[@]}")" || work_json=null
  [ -n "$work_json" ] || work_json=null
  notes_json="$(jq -cn --argjson items "$ships" '{title: "Forge server: what the next deploy would ship", items: $items,
    coverage: "No hand-written release notes: this list is generated from main since the production commit."}')"
  summary="production $(short "$prod_sha") · ${p_behind:-?} behind main$( [ -n "$shift_stage" ] && echo " · unmerged shift work")"
}
