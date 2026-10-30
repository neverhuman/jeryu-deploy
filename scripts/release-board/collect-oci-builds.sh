#!/usr/bin/env bash
# Read one OCI repository into a build report. No registry or deployment writes.
# Usage: collect-oci-builds.sh READ_URL REPOSITORY SOURCE_REPO IMAGE_LOCATION OUTPUT [PLACEMENTS_JSON] [CURL_CONFIG]
# Example READ_URL=http://127.0.0.1:5000, REPOSITORY=acme/app,
# SOURCE_REPO=acme/app, IMAGE_LOCATION=image.example/acme/app.
# A placement file is a complete observed target list. Omit it when unavailable.
# CURL_CONFIG is an optional existing read-credential config; never printed.
set -euo pipefail
umask 077
began=$SECONDS
registry="${1:?registry read URL}" repository="${2:?OCI repository}" source_repo="${3:?source owner/repo}"
image_location="${4:?image repository location}" output="${5:?output report}"
placements="${6:-}" credential_config="${7:-}"
[[ "$registry" =~ ^https?://[^[:space:]@]+$ ]] || { echo 'invalid registry read URL' >&2; exit 2; }
[[ "$repository" =~ ^[a-z0-9][a-z0-9._/-]*$ ]] || { echo 'invalid image repository' >&2; exit 2; }
[[ "$source_repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || { echo 'invalid source repository' >&2; exit 2; }
mkdir -p "$(dirname "$output")"
work="$(mktemp -d "$(dirname "$output")/oci-inventory.XXXXXXXX")"
printf 'OCI inventory evidence: %s\n' "$work" >&2
curl_args=(--silent --show-error --fail --connect-timeout 5 --max-time 15 --max-filesize 4194304)
if [[ -n "$credential_config" ]]; then curl_args+=(--config "$credential_config"); fi
accept='application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json'
read_url="${registry%/}/v2/$repository"
: >"$work/tags.jsonl"
last=""
for ((page=0; page<20; page++)); do
  (( SECONDS - began <= 120 )) || { echo "registry read budget exceeded" >&2; exit 1; }
  query="n=100"
  [[ -z "$last" ]] || query+="&last=$(jq -rn --arg last "$last" '$last|@uri')"
  curl "${curl_args[@]}" "$read_url/tags/list?$query" >"$work/page.json"
  jq -e '(.tags == null) or (.tags | type == "array")' "$work/page.json" >/dev/null
  count="$(jq '.tags // [] | length' "$work/page.json")"
  jq -c '.tags // [] | .[]' "$work/page.json" >>"$work/tags.jsonl"
  [[ "$count" -ge 100 ]] || break
  next="$(jq -r '.tags[-1]' "$work/page.json")"
  [[ "$next" != "$last" ]] || { echo 'registry tag paging did not advance' >&2; exit 1; }
  last="$next"
done
[[ "$page" -lt 20 ]] || { echo 'registry tag listing exceeded 20 pages; report refused' >&2; exit 1; }
jq -s 'unique' "$work/tags.jsonl" >"$work/tags.json"
[[ "$(jq length "$work/tags.json")" -le 500 ]] || { echo 'more than 500 tags; use a bounded receipt producer' >&2; exit 1; }
: >"$work/observed.jsonl"
while IFS= read -r tag; do
  (( SECONDS - began <= 120 )) || { echo "registry read budget exceeded" >&2; exit 1; }
  [[ "$tag" =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$ ]] || { echo 'invalid registry tag' >&2; exit 1; }
  encoded="$(jq -rn --arg tag "$tag" '$tag|@uri')"
  curl "${curl_args[@]}" -D "$work/headers" -H "Accept: $accept" "$read_url/manifests/$encoded" >"$work/manifest.json"
  digest="$(tr -d '\r' <"$work/headers" | awk 'tolower($1)=="docker-content-digest:" {print $2}' | tail -1)"
  [[ "$digest" =~ ^sha256:[a-f0-9]{64}$ ]] || { echo 'manifest has no immutable sha256 digest' >&2; exit 1; }
  metadata="$work/${digest#sha256:}.json"
  if [[ ! -f "$metadata" ]]; then
    if jq -e '.manifests | type == "array"' "$work/manifest.json" >/dev/null; then
      child="$(jq -r '[.manifests[] | select(.platform.os == "linux" and .platform.architecture == "amd64")][0].digest // .manifests[0].digest // empty' "$work/manifest.json")"
      [[ "$child" =~ ^sha256:[a-f0-9]{64}$ ]] || { echo 'image index has no readable platform manifest' >&2; exit 1; }
      curl "${curl_args[@]}" -H "Accept: $accept" "$read_url/manifests/$child" >"$work/manifest.json"
    fi
    config="$(jq -r '.config.digest // empty' "$work/manifest.json")"
    [[ "$config" =~ ^sha256:[a-f0-9]{64}$ ]] || { echo 'image manifest has no config digest' >&2; exit 1; }
    curl "${curl_args[@]}" "$read_url/blobs/$config" >"$metadata"
  fi
  created="$(jq -r '.created // empty' "$metadata")"
  revision="$(jq -r '.config.Labels["org.opencontainers.image.revision"] // .config.Labels["org.jain.source.commit"] // empty' "$metadata")"
  [[ -n "$created" && "$revision" =~ ^[a-fA-F0-9]{40}([a-fA-F0-9]{24})?$ ]] || { echo "image metadata lacks creation time or source commit: $digest" >&2; exit 1; }
  jq -cn --arg digest "$digest" --arg tag "$tag" --arg at "$created" --arg sha "$revision" '{digest:$digest,tag:$tag,built_at:$at,source_sha:$sha}' >>"$work/observed.jsonl"
done < <(jq -r '.[]' "$work/tags.json")
at="$(date -u +%FT%TZ)"
jq -s --arg at "$at" --arg repo "$source_repo" --arg image "$image_location" '
  {observed_at:$at, builds:(group_by(.digest) | map({
    id:.[0].digest, deliverable:$image, built_at:.[0].built_at,
    source_repo:$repo, source_sha:.[0].source_sha, status:"built", log_url:null,
    artifacts:[{repository:$image,digest:.[0].digest,tags:(map(.tag)|unique)}]
  }))}' "$work/observed.jsonl" >"$work/report.json"
if [[ -n "$placements" ]]; then
  [[ -f "$placements" && ! -L "$placements" ]] || { echo 'placements must be a regular JSON file' >&2; exit 1; }
  jq --slurpfile placements "$placements" '. + {placements:$placements[0]}' "$work/report.json" >"$work/complete.json"
  mv "$work/complete.json" "$work/report.json"
fi
mv "$work/report.json" "$output"
printf 'Collected %s immutable candidates into %s\n' "$(jq '.builds|length' "$output")" "$output" >&2
