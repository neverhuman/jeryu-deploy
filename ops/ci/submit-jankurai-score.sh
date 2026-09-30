#!/usr/bin/env bash
#
# submit-jankurai-score.sh - hand one audited head's report to the forge.
#
# The forge does not run jankurai. It records an audit job per head (branch,
# head sha, base sha) and completes `jankurai/proof` only from a report that
# comes back through the authenticated ingest and matches that job. This script
# is the one place that submits such a report, used by both audit producers:
#
#   * ops/ci/jankurai-audit-runner.sh, which claims queued jobs, and
#   * ops/ci/pr-ci.sh, whose `<repo>/required` run already audits the head — its
#     report IS the authoritative one, so no runner audits the head again.
#
# Usage:
#   ops/ci/submit-jankurai-score.sh --repo jeryu/jeryu-deploy --branch main \
#     --head <sha> [--base <sha>] --score-json <file> [--audit-mode diff|full] \
#     [--tool-exit N]
#
# With no --base the open audit job for this head supplies it; with no open job
# there is nothing to submit and the script exits 0 without posting (the head
# was audited by whoever holds the job, or needs no audit at all).
#
# Env:
#   JERYU_API                forge API base   (default http://127.0.0.1:8787)
#   JERYU_FORGE_TOKEN_FILE   runner PAT file, mode 0600 (required)
#   JERYU_AUDIT_RUNNER_ID    who ran the audit (default "$(hostname)/ci")
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=ops/ci/lib.sh
source "${ROOT}/ops/ci/lib.sh"

API="${JERYU_API:-http://127.0.0.1:8787}"
API="${API%/}"
REPO=""
BRANCH=""
HEAD_SHA=""
BASE_SHA=""
SCORE_JSON=""
AUDIT_MODE="diff"
TOOL_EXIT=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --repo) REPO="$2"; shift 2 ;;
    --branch) BRANCH="$2"; shift 2 ;;
    --head) HEAD_SHA="$2"; shift 2 ;;
    --base) BASE_SHA="$2"; shift 2 ;;
    --score-json) SCORE_JSON="$2"; shift 2 ;;
    --audit-mode) AUDIT_MODE="$2"; shift 2 ;;
    --tool-exit) TOOL_EXIT="$2"; shift 2 ;;
    -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -n "${REPO}" ] && [ -n "${BRANCH}" ] && [ -n "${HEAD_SHA}" ] ||
  { echo "--repo, --branch and --head are required" >&2; exit 2; }
[[ "${HEAD_SHA}" =~ ^[0-9a-f]{40}$ ]] || { echo "--head must be a full commit sha" >&2; exit 2; }
case "${AUDIT_MODE}" in diff|full) ;; *) echo "--audit-mode must be diff or full" >&2; exit 2 ;; esac

# Only the governed auditor's report is authoritative, so the submission states
# which binary produced it and the forge checks that against its pinned receipt.
require_jankurai
RUNNER_ID="${JERYU_AUDIT_RUNNER_ID:-$(hostname)/ci}"

TOKEN_FILE="${JERYU_FORGE_TOKEN_FILE:-}"
[ -n "${TOKEN_FILE}" ] || { echo "JERYU_FORGE_TOKEN_FILE is required (runner PAT)" >&2; exit 2; }
[ -f "${TOKEN_FILE}" ] && [ ! -L "${TOKEN_FILE}" ] || { echo "token file must be a regular file" >&2; exit 2; }
[ "$(stat -c '%a' "${TOKEN_FILE}")" = "600" ] || { echo "token file must be mode 0600" >&2; exit 2; }
WORK_DIR="$(mktemp -d -t jankurai-submit-XXXXXX)"
trap 'rm -rf "${WORK_DIR}"' EXIT
AUTH_CONFIG="${WORK_DIR}/curl-auth.conf"
token="$(cat "${TOKEN_FILE}")"
# A bearer value must be data, never curl configuration syntax.
[[ "${token}" =~ ^[A-Za-z0-9._~+/-]+=*$ ]] || { echo "token file must contain one nonempty bearer value" >&2; exit 2; }
( umask 077; printf 'header = "Authorization: Bearer %s"\n' "${token}" > "${AUTH_CONFIG}" )
unset token
api_curl() { curl --disable --config "${AUTH_CONFIG}" "$@"; }

if [ -z "${BASE_SHA}" ]; then
  queue_json="${WORK_DIR}/queue.json"
  if ! api_curl -fsS "${API}/api/v1/jankurai-audits" -o "${queue_json}"; then
    echo "[submit-score] the audit queue is unreadable; nothing submitted" >&2
    exit 0
  fi
  BASE_SHA="$(python3 - "${queue_json}" "${REPO}" "${HEAD_SHA}" <<'PY'
import json
import sys

owner, _, name = sys.argv[2].partition("/")
doc = json.load(open(sys.argv[1]))
for ticket in doc.get("tickets", []):
    if ticket["owner"] == owner and ticket["repo"] == name and ticket["headSha"] == sys.argv[3]:
        print(ticket["baseSha"])
        break
PY
)"
  if [ -z "${BASE_SHA}" ]; then
    echo "[submit-score] no open audit job for ${REPO}@${HEAD_SHA}; nothing to submit"
    exit 0
  fi
fi

request="${WORK_DIR}/request.json"
python3 - "${request}" "${BRANCH}" "${HEAD_SHA}" "${BASE_SHA}" "${RUNNER_ID}" \
  "${JERYU_JANKURAI_VERSION}" "${JERYU_JANKURAI_SHA256}" \
  "${JERYU_JANKURAI_RECEIPT_SHA256:-}" "${AUDIT_MODE}" "${TOOL_EXIT}" "${SCORE_JSON}" <<'PY'
import json
import os
import sys

out, branch, head, base, runner, version, digest, receipt, mode, exit_code, score_json = sys.argv[1:12]
report = None
if score_json and os.path.isfile(score_json):
    with open(score_json) as handle:
        report = json.load(handle)
body = {
    "branch": branch,
    "commit_sha": head,
    "base_sha": base,
    "runner_id": runner,
    "jankurai_version": version,
    "jankurai_sha256": digest,
    "audit_mode": mode,
    "tool_exit": int(exit_code),
    "report": report,
}
if receipt:
    body["jankurai_receipt_sha256"] = receipt
with open(out, "w") as handle:
    json.dump(body, handle)
PY

response="${WORK_DIR}/response.json"
status="$(api_curl -sS -o "${response}" -w '%{http_code}' \
  -X POST -H 'Content-Type: application/json' \
  --data-binary "@${request}" \
  "${API}/api/v1/repos/${REPO/\//%2F}/jankurai-scores")"
if [ "${status}" != "201" ]; then
  echo "[submit-score] ${REPO}@${HEAD_SHA} refused (HTTP ${status}): $(cat "${response}")" >&2
  exit 1
fi
echo "[submit-score] ${REPO}@${HEAD_SHA} recorded from ${RUNNER_ID} (${AUDIT_MODE} audit vs ${BASE_SHA})"
