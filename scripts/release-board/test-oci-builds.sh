#!/usr/bin/env bash
# Exercise the OCI reader with a networkless registry and retained evidence.
set -euo pipefail
umask 077
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
mkdir -p "$repo/target/oci-build-fixtures"
fixture="$(mktemp -d "$repo/target/oci-build-fixtures/run.XXXXXXXX")"
mkdir -p "$fixture/bin"
cat >"$fixture/bin/curl" <<'CURL'
#!/usr/bin/env bash
set -euo pipefail
headers="" url=""
while [[ $# -gt 0 ]]; do
  case "$1" in -D) headers="$2"; shift ;; http*) url="$1" ;; esac
  shift
done
digest="sha256:$(printf 'a%.0s' {1..64})"
config="sha256:$(printf 'b%.0s' {1..64})"
case "$url" in
  */tags/list*) printf '%s\n' '{"tags":["dev","rc-1"]}' ;;
  */manifests/*)
    [[ -z "$headers" ]] || printf 'Docker-Content-Digest: %s\r\n' "$digest" >"$headers"
    printf '{"config":{"digest":"%s"}}\n' "$config" ;;
  */blobs/*)
    if [[ "${OCI_FIXTURE_MISSING_SOURCE:-0}" = 1 ]]; then
      printf '%s\n' '{"created":"2026-10-01T12:00:00Z","config":{"Labels":{}}}'
    else
      printf '{"created":"2026-10-01T12:00:00Z","config":{"Labels":{"org.opencontainers.image.revision":"%s"}}}\n' "$(printf 'c%.0s' {1..40})"
    fi ;;
  *) exit 22 ;;
esac
CURL
chmod 700 "$fixture/bin/curl"
export PATH="$fixture/bin:$PATH"
bash "$here/collect-oci-builds.sh" http://registry.test acme/app acme/app image.example/acme/app "$fixture/report.json"
jq -e '(.builds|length)==1 and (.builds[0].artifacts[0].tags==["dev","rc-1"]) and (.builds[0].source_sha|length)==40 and (has("placements")|not)' "$fixture/report.json" >/dev/null
printf '%s\n' 'ok - duplicate tags share one immutable candidate without inferring an environment'
cp "$fixture/report.json" "$fixture/accepted.json"
if OCI_FIXTURE_MISSING_SOURCE=1 bash "$here/collect-oci-builds.sh" http://registry.test acme/app acme/app image.example/acme/app "$fixture/report.json"; then exit 1; fi
cmp "$fixture/report.json" "$fixture/accepted.json"
printf '%s\n' 'ok - unreadable provenance refuses a partial replacement'
printf 'retained evidence: %s\n' "$fixture"
