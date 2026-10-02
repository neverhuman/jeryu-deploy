#!/usr/bin/env bash
#
# jankurai-audit-runner.sh - claim queued audit jobs and run them here.
#
# Authoritative jankurai scoring runs on the gate runners, not on the forge.
# A push records one audit job per head (branch, head sha, base sha); this
# script claims jobs, runs the governed `diff-audit` against the job's base in a
# throwaway clone, and submits the report through the authenticated ingest,
# which is what records the score and completes `jankurai/proof`.
#
# One pass, one claim batch: run it from a slot loop or a timer, never as an
# unbounded fan-out. Each audit is niced and time-boxed, because this machine
# also runs the PR gate.
#
# Each run reports a runner heartbeat as "<short host>/jankurai-audit" (see
# ops/ci/jankurai-audit-heartbeat.sh): `current` while an audit runs, and the
# last audit's conclusion — scored (the forge recorded the governed report),
# tool-failed (the auditor wrote no usable report), refused (the forge refused
# the submission) or failed (the head could not be fetched; nothing ran). A run
# that claims nothing beats with the previous result unchanged.
#
# Usage:
#   ops/ci/jankurai-audit-runner.sh [--max N] [--once]
#
# Env:
#   JERYU_API                forge API base        (default http://127.0.0.1:8787)
#   JERYU_FORGE_GIT_BASE     git base for clones   (default "$JERYU_API/git")
#   JERYU_FORGE_TOKEN_FILE   runner PAT file, mode 0600 (required)
#   JERYU_AUDIT_RUNNER_ID    runner id reported with each claim and report
#   JERYU_AUDIT_TIMEOUT      seconds per audit     (default 900)
#   JERYU_AUDIT_RUNNER_REPO  owner/name of this runner's code, for the heartbeat
#   JERYU_AUDIT_HEARTBEAT    0 to send no heartbeat (default 1)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=ops/ci/lib.sh
source "${ROOT}/ops/ci/lib.sh"
# shellcheck source=ops/ci/jankurai-audit-heartbeat.sh
source "${ROOT}/ops/ci/jankurai-audit-heartbeat.sh"
# shellcheck source=ops/ci/jankurai-audit-git-auth.sh
source "${ROOT}/ops/ci/jankurai-audit-git-auth.sh"

API="${JERYU_API:-http://127.0.0.1:8787}"
API="${API%/}"
GIT_BASE="${JERYU_FORGE_GIT_BASE:-${API}/git}"
GIT_BASE="${GIT_BASE%/}"
RUNNER_ID="${JERYU_AUDIT_RUNNER_ID:-$(hostname)/audit0}"
AUDIT_TIMEOUT="${JERYU_AUDIT_TIMEOUT:-900}"
MAX=1

while [ "$#" -gt 0 ]; do
  case "$1" in
    --max) MAX="$2"; shift 2 ;;
    # Accepted so a slot loop and a one-shot timer share one entrypoint.
    --once) shift ;;
    -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ "${MAX}" =~ ^[1-8]$ ]] || { echo "--max must be 1..8" >&2; exit 2; }

require_jankurai

TOKEN_FILE="${JERYU_FORGE_TOKEN_FILE:-}"
[ -n "${TOKEN_FILE}" ] || { echo "JERYU_FORGE_TOKEN_FILE is required (runner PAT)" >&2; exit 2; }
[ -f "${TOKEN_FILE}" ] && [ ! -L "${TOKEN_FILE}" ] || { echo "token file must be a regular file" >&2; exit 2; }
[ "$(stat -c '%a' "${TOKEN_FILE}")" = "600" ] || { echo "token file must be mode 0600" >&2; exit 2; }
WORK_DIR="$(mktemp -d -t jankurai-audit-XXXXXX)"
trap 'audit_beat_stop_keepalive; rm -rf "${WORK_DIR}"' EXIT
AUTH_CONFIG="${WORK_DIR}/curl-auth.conf"
token="$(cat "${TOKEN_FILE}")"
[[ "${token}" =~ ^[A-Za-z0-9._~+/-]+=*$ ]] || { echo "token file must contain one nonempty bearer value" >&2; exit 2; }
( umask 077; printf 'header = "Authorization: Bearer %s"\n' "${token}" > "${AUTH_CONFIG}" )
unset token
# Private repositories are cloned with the same token; it reaches git only through this 0600 file.
GIT_AUTH_CONFIG="${WORK_DIR}/git-auth.conf"
audit_git_auth_config "${GIT_AUTH_CONFIG}" "${GIT_BASE}" "${TOKEN_FILE}" || exit 2
api_curl() { curl --disable --config "${AUTH_CONFIG}" "$@"; }
audit_beat_init

claimed="${WORK_DIR}/claimed.json"
api_curl -fsS -X POST -H 'Content-Type: application/json' \
  --data-binary "$(printf '{"runner_id":"%s","max":%s}' "${RUNNER_ID}" "${MAX}")" \
  "${API}/api/v1/jankurai-audits/claim" -o "${claimed}"

mapfile -t JOBS < <(python3 - "${claimed}" <<'PY'
import json
import sys

doc = json.load(open(sys.argv[1]))
for ticket in doc.get("tickets", []):
    print("\t".join([
        ticket["owner"], ticket["repo"], ticket["branch"],
        ticket["headSha"], ticket["baseSha"], ticket.get("auditMode", "diff"),
    ]))
PY
)

if [ "${#JOBS[@]}" -eq 0 ]; then
  echo "[audit-runner] no audit work claimed"
  audit_beat
  exit 0
fi

# The audit is the heavy part of this machine's work and shares it with the PR
# gate, so it runs niced, time-boxed, and one job after another.
audit_one() {
  local owner="$1" repo="$2" branch="$3" head="$4" base="$5" mode="$6"
  local src="${WORK_DIR}/${repo}-${head}"
  local out="${src}/target/jankurai/diff/diff-score.json"
  local exit_code=0 started="${SECONDS}"

  if [ "${mode}" = "full" ]; then
    echo "[audit-runner] ${owner}/${repo}@${head} (${branch}, whole tree: no commit base)"
  else
    echo "[audit-runner] ${owner}/${repo}@${head} (${branch} vs ${base})"
  fi
  audit_beat_start "${owner}/${repo}" "${head}"
  if ! GIT_TERMINAL_PROMPT=0 git -c include.path="${GIT_AUTH_CONFIG}" clone -q \
      "${GIT_BASE}/${owner}/${repo}.git" "${src}" 2>&1; then
    echo "[audit-runner] clone failed for ${owner}/${repo}@${head}" >&2
    audit_beat_finish "${owner}/${repo}" "${head}" failed "$((SECONDS - started))" "clone failed"
    return 0
  fi
  if ! git -C "${src}" checkout -q --detach "${head}" 2>&1; then
    echo "[audit-runner] ${head} is not in the clone" >&2
    audit_beat_finish "${owner}/${repo}" "${head}" failed "$((SECONDS - started))" \
      "head is not in the clone"
    rm -rf "${src}"
    return 0
  fi
  # Forced scoring for unconfigured repositories: a head that carries no policy
  # of its own is audited against the jeryu-managed default, dropped into the
  # throwaway clone only (untracked files do not enter the diff set).
  if [ ! -f "${src}/agent/audit-policy.toml" ]; then
    mkdir -p "${src}/agent"
    cat > "${src}/agent/audit-policy.toml" <<'POLICY'
schema_version = "1.0.0"
workspace = "unconfigured"
minimum_score = 85
hard_findings_allowed = 0
required_tool = "jankurai"
required_tool_version = "1.6.11"

[scan]
excluded_paths = [".jankurai/", "apps/web/dist/"]
POLICY
  fi
  mkdir -p "$(dirname "${out}")"
  if [ "${mode}" = "full" ]; then
    # A first main, an orphan or unrelated branch: there is no commit to diff
    # against, and the ticketed base is the "none" marker (the empty tree), so
    # it is never passed to the auditor — `diff-audit` against a tree answers
    # "nothing to audit" and scores nothing. The whole tree is audited instead,
    # the same invocation the `<repo>/required` gate uses. A nonzero exit is
    # not a tool failure here (a low score exits nonzero too); the forge reads
    # the report.
    ( cd "${src}" &&
      timeout "${AUDIT_TIMEOUT}" nice -n 10 ionice -c3 \
        "${JERYU_GOVERNED_JANKURAI_BIN}" audit . \
        --json "${out}" --no-score-history ) || exit_code=$?
    if [ -s "${out}" ]; then
      exit_code=0
    fi
  else
    # --advisory-only: always write the JSON and exit 0; the forge derives the
    # strict verdict from the report itself.
    timeout "${AUDIT_TIMEOUT}" nice -n 10 ionice -c3 \
      "${JERYU_GOVERNED_JANKURAI_BIN}" diff-audit "${src}" \
      --base-ref "${base}" --json "${out}" --advisory-only || exit_code=$?
  fi

  local conclusion=scored reason=""
  if [ "${exit_code}" -ne 0 ]; then
    conclusion=tool-failed reason="jankurai exited ${exit_code}"
  fi
  if ! JERYU_AUDIT_RUNNER_ID="${RUNNER_ID}" \
    bash "${ROOT}/ops/ci/submit-jankurai-score.sh" \
      --repo "${owner}/${repo}" --branch "${branch}" --head "${head}" \
      --base "${base}" --audit-mode "${mode}" \
      --score-json "${out}" --tool-exit "${exit_code}"; then
    echo "[audit-runner] ${owner}/${repo}@${head} report refused" >&2
    conclusion=refused reason="the forge refused the report"
  fi
  audit_beat_finish "${owner}/${repo}" "${head}" "${conclusion}" "$((SECONDS - started))" "${reason}"
  rm -rf "${src}"
}

for job in "${JOBS[@]}"; do
  IFS=$'\t' read -r owner repo branch head base mode <<<"${job}"
  audit_one "${owner}" "${repo}" "${branch}" "${head}" "${base}" "${mode}"
done
