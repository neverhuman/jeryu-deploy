# shellcheck shell=bash disable=SC2034,SC2154  # sets and reads the per-family variables collect.sh owns
# examples/acme.sh — a release-board adapter for an invented family, to copy.
#
# Adapters are site configuration: they name your hosts, paths and repositories, so they live
# outside this repository, in $JERYU_RELEASE_BOARD_FAMILIES (default
# ~/.config/jeryu/release-board/families/<family>.sh). collect.sh sources lib.sh, then this file,
# then calls collect_<family> (dashes become underscores). Everything it reads is best effort:
# call `problem SOURCE MESSAGE` for anything unreadable and keep going.
#
# This one describes "acme": an app on main, a staging copy and a production copy recorded in
# files on the deploy host, two production nodes that report their version over HTTP, and a todo
# queue named acme. Replace every value below with your own.

ACME_REPO=acme/app                               # owner/name on the forge
ACME_STATE=/srv/acme                             # where the deploy host records deployed commits
ACME_NODES="node-a node-b"                       # production nodes
ACME_HEALTH='https://%s.acme.example/health'     # each node's health URL; %s is the node

collect_acme() {
  local app main stage_sha prod_sha
  app="$(mirror "$ACME_REPO")" || return 1
  main="$(git -C "$app" rev-parse --verify -q refs/heads/main)"
  stage_sha="$(cat "$ACME_STATE/deployed-stage.sha" 2>/dev/null)" || problem stage "no $ACME_STATE/deployed-stage.sha"
  prod_sha="$(cat "$ACME_STATE/deployed-prod.sha" 2>/dev/null)" || problem prod "no $ACME_STATE/deployed-prod.sha"

  # A stage is only ok when every node runs its commit.
  local node running targets=()
  for node in $ACME_NODES; do
    # shellcheck disable=SC2059  # the URL template is this file's own setting
    running="$(curl --silent --fail --max-time 10 "$(printf "$ACME_HEALTH" "$node")" | jq -r '.commit // empty' 2>/dev/null)" \
      || problem "$node" "health endpoint did not answer"
    targets+=("$(target "$node" "$(short "$running")" "$( [ -n "$running" ] && [ "$running" = "$prod_sha" ] && echo ok || echo bad)")")
  done
  local prod_targets prod_state n_behind
  prod_targets="$(printf '%s\n' "${targets[@]}" | jq -cs .)"
  prod_state="$(worst_of "$prod_targets")"
  n_behind="$(behind "$app" "$prod_sha" "$main")"

  lane app "App" "$ACME_REPO · built by the deploy timer" acme false \
    "$(stage id=main name=main version="$(short "$main")" state=none status=source known=derived)" \
    "$(stage id=stage name=stage version="$(short "$stage_sha")" \
        state="$( [ "$stage_sha" = "$main" ] && echo ok || echo warn)" \
        status="$( [ "$stage_sha" = "$main" ] && echo "in sync" || echo "$(behind "$app" "$stage_sha" "$main") behind")" \
        known=host automatic=true promote_cmd="merge a pull request to main")" \
    "$(stage id=prod name=production version="$(short "$prod_sha")" state="$prod_state" \
        status="$( [ "$prod_state" = ok ] && echo "in sync" || echo skew) · ${n_behind:-?} behind" known=host \
        targets="$prod_targets" human_only=true promote_cmd="git tag -a vX.Y.Z && git push origin vX.Y.Z" \
        ships="$(git -C "$app" log --format='%h · %s' "$prod_sha..$main" 2>/dev/null | lines_json)" \
        forge_repo="$ACME_REPO" forge_env=production)"

  main_specs="$app=$main"
  work_json="$(work_summary acme "Matched by Todo: trailer, then by content." "" "$app=$prod_sha")" || work_json=null
  [ -n "$work_json" ] || work_json=null
  summary="production $(short "$prod_sha") · ${n_behind:-?} behind main"
}
