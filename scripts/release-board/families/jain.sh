# shellcheck shell=bash disable=SC2034,SC2154  # sets and reads the per-family variables collect.sh owns
# families/jain.sh — the jain board: the Free download from tag to published, its installers,
# the Try pool, the cloud appliance (owned by veox-ai, shown read-only), and the split repos'
# tags against main. Sourced by collect.sh; reads only.
#
# NEVER request www.neverhuman.org/try-free: a GET there leases a real Try slot for ten minutes.
# The Try pool is read from /healthz and nothing else.
#
# Env: JAIN_FREE_HOST (xbabe2), JAIN_DOWNLOADS (https://downloads.neverhuman.org/jain),
# JAIN_TRY_HEALTH (https://www.neverhuman.org/healthz), JAIN_PIN_REPOS (the repos in the pins tab).

JAIN_OWNER=veox

collect_jain() {
  local free_host="${JAIN_FREE_HOST:-xbabe2}" downloads="${JAIN_DOWNLOADS:-https://downloads.neverhuman.org/jain}"
  local web deploy web_main deploy_main tag tag_sha ahead receipt latest
  web="$(mirror "$JAIN_OWNER/jain-web")" || return 1
  deploy="$(mirror "$JAIN_OWNER/jain-deploy")" || return 1
  web_main="$(git -C "$web" rev-parse --verify -q refs/heads/main)"
  deploy_main="$(git -C "$deploy" rev-parse --verify -q refs/heads/main)"
  tag="$(git -C "$web" tag --list 'jain-web-v*' --sort=-v:refname | head -1)"
  tag_sha="$(git -C "$web" rev-parse --verify -q "$tag^{commit}" 2>/dev/null)"
  ahead="$(behind "$web" "$tag_sha" "$web_main")"

  receipt="$(timeout 20 ssh -o BatchMode=yes -o ConnectTimeout=8 "$free_host" \
    'f=$(ls -t ~/free-release-staged/*/receipt.json 2>/dev/null | head -1); [ -n "$f" ] && jq -c "{version, tag, image_id, jain_deploy_commit, jain_web_commit}" "$f"' 2>/dev/null)" \
    || problem "$free_host" "could not read the newest staged Free receipt"
  latest="$(curl --silent --fail --max-time 15 "$downloads/latest.json" 2>/dev/null)" \
    || problem "downloads" "could not read $downloads/latest.json"
  local s_ver s_tag s_img p_ver p_img s_state p_state
  s_ver="$(jq -r '.version // empty' <<<"${receipt:-{\}}")"; s_tag="$(jq -r '.tag // empty' <<<"${receipt:-{\}}")"
  s_img="$(jq -r '.image_id // empty' <<<"${receipt:-{\}}")"
  p_ver="$(jq -r '.version // empty' <<<"${latest:-{\}}")"; p_img="$(jq -r '.image_id // empty' <<<"${latest:-{\}}")"
  s_state=none; [ -n "$s_tag" ] && { s_state=ok; [ "$s_tag" = "$tag" ] || s_state=warn; }
  p_state=none; [ -n "$p_img" ] && { p_state=ok; [ "$p_img" = "$s_img" ] || p_state=warn; }
  lane free-download "Free download" "newest jain-web tag · built, gated and signed on $free_host every 10 min" jain false \
    "$(stage id=tagged name=tagged version="$tag" state="$( [ "${ahead:-0}" -gt 0 ] && echo warn || echo ok)" \
        status="main ${ahead:-?} ahead" known=derived human_only=true promote_cmd="cut the next jain-web-v* tag (humans only)")" \
    "$(stage id=staged name=staged version="${s_ver:-none}" state="$s_state" \
        status="$( [ "$s_tag" = "$tag" ] && echo "= newest tag" || echo "staged ${s_tag:-nothing}")" known=host automatic=true \
        promote_cmd="automatic (free-release-watch.timer on $free_host)")" \
    "$(stage id=published name=published version="${p_ver:-unknown}" state="$p_state" \
        status="$( [ -n "$p_img" ] && [ "$p_img" = "$s_img" ] && echo "= staged" || echo "staged is not published")" known=host \
        targets="[$(target "${downloads#https://}/" "${p_ver:-unknown}" "$p_state")]" human_only=true \
        promote_cmd="ssh $free_host '…/deployment/free/deploy-free-release.sh --release ~/free-release-staged/${s_ver:-<version>}'")"

  # Installers: the public files against jain-deploy main, byte for byte.
  local f want have i_state=ok i_targets=()
  for f in install.sh install.ps1; do
    want="$(git -C "$deploy" show "$deploy_main:deployment/free/$f" 2>/dev/null | sha256sum | cut -c1-12)"
    have="$(curl --silent --fail --max-time 15 "$downloads/$f" 2>/dev/null | sha256sum | cut -c1-12)"
    if [ "$want" = "$have" ]; then i_targets+=("$(target "$f" "$have" ok)")
    else i_state=warn; i_targets+=("$(target "$f" "public $have, main $want" warn)"); fi
  done
  lane installers "Installers" "$JAIN_OWNER/jain-deploy deployment/free/install.{sh,ps1}" jain false \
    "$(stage id=main name=main version="$(short "$deploy_main")" state=none status=source known=derived)" \
    "$(stage id=live name=live version="$( [ "$i_state" = ok ] && echo byte-identical || echo differs)" state="$i_state" \
        status="$( [ "$i_state" = ok ] && echo "in sync" || echo "public files differ from main")" known=host \
        targets="$(printf '%s\n' "${i_targets[@]}" | jq -cs .)" human_only=true \
        promote_cmd="copied by hand via $free_host (no script)")"

  # The cloud appliance, as veox-ai's board last saw it, behind the engine tag jain's lock pins.
  local lock_tag veox_board="$out/veox-ai.json"
  lock_tag="$(git -C "$deploy" show "$deploy_main:jain-split.lock.toml" 2>/dev/null \
    | awk '/^\[\[repo\]\]/ { name = "" } /^(name|repo) *=/ { gsub(/[" ]/, ""); split($0, a, "="); name = a[2] }
           name == "jain-web" && /^tag *=/ { gsub(/[" ]/, ""); split($0, a, "="); print a[2]; exit }')"
  if [ -r "$veox_board" ]; then
    jq -c --argjson pin "$(stage id=engine-pin name="engine pin" version="${lock_tag:-unknown}" \
          state="$( [ -n "$lock_tag" ] && [ "$lock_tag" = "$tag" ] && echo ok || echo warn)" \
          status="$( [ "$lock_tag" = "$tag" ] && echo "= Free's tag" || echo "Free uses ${tag#jain-web-}")" known=derived \
          promote_cmd="a PR editing the jain-web row of jain-split.lock.toml")" \
      '.lanes[] | select(.id == "cloud-appliance") | .read_only = true | .name = "Cloud appliance (owned by veox-ai)" | .stages = [$pin] + .stages' \
      "$veox_board" >>"$lanes_file"
  else
    problem "veox-ai board" "not collected yet; the appliance lane appears after the next veox-ai run"
  fi

  # The Try pool: /healthz only.
  local health try_ver up
  health="$(curl --silent --fail --max-time 15 "${JAIN_TRY_HEALTH:-https://www.neverhuman.org/healthz}" 2>/dev/null)" \
    || problem "try pool" "could not read /healthz"
  try_ver="$(jq -r '.version // empty' <<<"${health:-{\}}")"
  up="$(jq -r '((.uptime_seconds // 0) / 86400 * 10 | floor) / 10' <<<"${health:-{\}}")"
  lane try-pool "Try pool" "www.neverhuman.org · read from /healthz (a GET on /try-free takes a real lease)" jain false \
    "$(stage id=live name=live version="${try_ver:-unknown}" state=none status="promote path unknown" known=host \
        targets="[$(target 'www.neverhuman.org' "${try_ver:-unknown}${up:+ · up $up days}" none)]")"

  # Pins: newest split tag of each repo against its main.
  local rows=() r m rm t on
  for r in ${JAIN_PIN_REPOS:-jain-web jain-deploy jain-core jain-report jain-cli jain-starforge jain-contracts jain-math jain-tui jain-xgboost jain-python jain-jailgun jain-nexus jain-split-ops}; do
    m="$(mirror "$JAIN_OWNER/$r")" || continue
    rm="$(git -C "$m" rev-parse --verify -q refs/heads/main)" || continue
    t="$(git -C "$m" tag --list "$r-v*" --sort=-v:refname | head -1)"
    on=no; [ -n "$t" ] && contains "$m" "$t" "$rm" && on=yes
    rows+=("$(jq -cn --arg r "$r" --arg m "$(short "$rm")" --arg t "${t#"$r"-}" --arg on "$on" \
      --argjson b "$( [ -n "$t" ] && behind "$m" "$t" "$rm" || echo null)" \
      '{repo: $r, cells: [$m, (if $t == "" then "none" else $t end), $on], behind: $b}')")
  done
  pins_json="$(printf '%s\n' "${rows[@]}" | jq -cs '{note: "Newest split tag of each repo against its main. jain-split.lock.toml is not used as a pin set: most rows are PENDING and the Cargo git pins resolve elsewhere.", columns: ["Repo", "Main", "Newest tag", "Tag on main", "Main ahead"], rows: .}')"

  # Work: live when the installed gate runner (jain-deploy) or the published Free (jain-web) holds it.
  local gate
  gate="$(timeout 20 ssh -o BatchMode=yes -o ConnectTimeout=8 "$free_host" 'jq -r .commit ~/gate-runner/installed-main.json' 2>/dev/null)"
  main_specs="$deploy=$deploy_main"$'\n'"$web=$web_main"
  local live_specs=()
  [ -n "$gate" ] && live_specs+=("$deploy=$gate")
  local pub_web; pub_web="$(jq -r '.jain_web_commit // empty' <<<"${receipt:-{\}}")"
  [ -n "$pub_web" ] && [ "$p_img" = "$s_img" ] && live_specs+=("$web=$pub_web")
  work_json="$(work_summary jain "Matched by Todo: trailer, then by content. Live means the installed gate runner or the published Free download holds the change." \
    "Not linkable: the files in jain-deploy/todo." "${live_specs[@]}")" || work_json=null
  [ -n "$work_json" ] || work_json=null
  notes_json="$(jq -cn '{title: "Free download: what publishing would ship", items: [],
    coverage: "No generated notes yet for jain: CHANGELOGs mostly read Unreleased, and the Free download carries no notes."}')"
  summary="Free ${p_ver:-?} published$( [ "$p_img" = "$s_img" ] && echo " · in sync with staged" || echo " · a newer build is staged")$( [ "${ahead:-0}" -gt 0 ] && echo " · jain-web main $ahead ahead of its tag")"
}
