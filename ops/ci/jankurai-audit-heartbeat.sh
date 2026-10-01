#!/usr/bin/env bash
#
# jankurai-audit-heartbeat.sh - the jankurai audit runner's heartbeat. Sourced by
# ops/ci/jankurai-audit-runner.sh, never run on its own.
#
# Every run beats once (POST /api/v1/runners/heartbeat, docs/pipeline-events.md)
# as "<short host>/jankurai-audit" with the `jankurai-audit` label, so /runners
# shows the auditor next to the gate slots it shares a host with:
#
#   * an idle run (no audit claimed) re-sends the previous `last` unchanged;
#   * an audit beats `current` when it starts, keeps beating every
#     JERYU_AUDIT_BEAT_EVERY seconds while it runs (an audit can outlast the
#     forge's 180-second offline threshold), and beats `last` when it ends;
#   * `code` names the installed runner (JERYU_AUDIT_RUNNER_REPO plus the
#     VERSION file the installer writes) and `tools` the governed jankurai it
#     invokes, measured here: `--version` and the binary's sha256.
#
# Best-effort by design: a beat never fails the run, curl is time-boxed, and a
# 422 (an older forge that predates `code`/`tools`) is retried once without them.
#
# Needs from the caller: API (forge API base), AUTH_CONFIG (curl config holding
# the bearer header), ROOT (the install root, holding VERSION) and
# JERYU_GOVERNED_JANKURAI_BIN (set by require_jankurai).
#
# Env:
#   JERYU_AUDIT_HEARTBEAT     1 to beat (default), 0 to stay silent
#   JERYU_AUDIT_RUNNER_REPO   owner/name the runner's code comes from; no `code` without it
#   JERYU_AUDIT_STATE_DIR     where the last result is kept between runs
#                             (default ${XDG_STATE_HOME:-$HOME/.local/state}/jeryu-jankurai-audit-runner)
#   JERYU_AUDIT_BEAT_EVERY    keepalive seconds while auditing (default 60)
#   JERYU_AUDIT_BEAT_TIMEOUT  curl --max-time per beat (default 5)

AUDIT_BEAT_RECIPE="jankurai audit"
_audit_beat_ready=0
_audit_beat_current="null"
_audit_beat_keepalive=""
_audit_beat_finished=""

# Work out what every beat of this run carries. Never fails.
audit_beat_init() {
  _audit_beat_ready=0
  [ "${JERYU_AUDIT_HEARTBEAT:-1}" = 1 ] || return 0
  local host version digest commit installed=""
  host="$(hostname -s 2>/dev/null)" || return 0
  [[ "${host}" =~ ^[A-Za-z0-9.-]{1,64}$ ]] || return 0
  _audit_beat_host="${host}"
  _audit_beat_state="${JERYU_AUDIT_STATE_DIR:-${XDG_STATE_HOME:-${HOME}/.local/state}/jeryu-jankurai-audit-runner}/last.json"

  version="$("${JERYU_GOVERNED_JANKURAI_BIN:-/nonexistent}" --version 2>/dev/null | head -n 1)" || version=""
  version="${version#jankurai }"
  digest="$(sha256sum "${JERYU_GOVERNED_JANKURAI_BIN:-/nonexistent}" 2>/dev/null | awk '{print $1}')" || digest=""
  [[ "${digest}" =~ ^[0-9a-f]{64}$ ]] || digest=""
  _audit_beat_tools="$(jq -cn --arg version "${version}" --arg digest "${digest}" '
    [{name: "jankurai"}
     + (if $version != "" then {version: $version} else {} end)
     + (if $digest != "" then {sha256: $digest} else {} end)]')" || _audit_beat_tools="[]"

  _audit_beat_code="null"
  commit="$(head -n 1 "${ROOT}/VERSION" 2>/dev/null)" || commit=""
  if [ -n "${JERYU_AUDIT_RUNNER_REPO:-}" ] && [[ "${commit}" =~ ^[0-9a-f]{7,64}$ ]]; then
    installed="$(date -u -r "${ROOT}/VERSION" +%FT%TZ 2>/dev/null)" || installed=""
    _audit_beat_code="$(jq -cn --arg repo "${JERYU_AUDIT_RUNNER_REPO}" --arg commit "${commit}" \
      --arg installed "${installed}" '
      {repo: $repo, commit: $commit} + (if $installed != "" then {installedAt: $installed} else {} end)')" \
      || _audit_beat_code="null"
  fi
  _audit_beat_ready=1
}

# The last result this runner reported (this run's, else the kept one), or null.
_audit_beat_last() {
  [ -z "${_audit_beat_finished}" ] || { printf '%s\n' "${_audit_beat_finished}"; return 0; }
  [ -s "${_audit_beat_state}" ] || { echo null; return 0; }
  jq -ce 'select(type == "object" and (.conclusion | type) == "string")' "${_audit_beat_state}" 2>/dev/null \
    || echo null
}

_audit_beat_post() {
  curl --disable --config "${AUTH_CONFIG}" --silent --max-time "${JERYU_AUDIT_BEAT_TIMEOUT:-5}" \
    -o /dev/null -w '%{http_code}' -X POST -H 'Accept: application/json' \
    -H 'Content-Type: application/json' --data-binary @- \
    "${API}/api/v1/runners/heartbeat" <<<"$1" 2>/dev/null || true
}

# One beat: who this is, what it runs, and its current and last work.
audit_beat() {
  [ "${_audit_beat_ready}" = 1 ] || return 0
  local body status
  body="$(jq -cn --arg host "${_audit_beat_host}" --argjson current "${_audit_beat_current}" \
    --argjson last "$(_audit_beat_last)" --argjson code "${_audit_beat_code}" \
    --argjson tools "${_audit_beat_tools}" '
    {runnerId: ($host + "/jankurai-audit"), host: $host, slot: 0,
     labels: ["jankurai-audit"], intervalSeconds: 30}
    + (if $current != null then {current: $current} else {} end)
    + (if $last != null then {last: $last} else {} end)
    + (if $code != null then {code: $code} else {} end)
    + (if ($tools | length) > 0 then {tools: $tools} else {} end)')" \
    || { echo "[audit-runner] heartbeat: could not build (ignored)" >&2; return 0; }
  status="$(_audit_beat_post "${body}")"
  if [ "${status}" = 422 ] && jq -e 'has("code") or has("tools")' >/dev/null 2>&1 <<<"${body}"; then
    # An older forge refuses fields it does not know; say the rest.
    status="$(_audit_beat_post "$(jq -c 'del(.code, .tools)' <<<"${body}")")"
  fi
  case "${status}" in
    2??) ;;
    *) echo "[audit-runner] heartbeat: not accepted (HTTP ${status:-000}; ignored)" >&2 ;;
  esac
  return 0
}

# An audit of owner/name@sha starts: beat `current`, then keep beating until it ends.
audit_beat_start() {
  [ "${_audit_beat_ready}" = 1 ] || return 0
  _audit_beat_current="$(jq -cn --arg repo "$1" --arg sha "$2" --arg recipe "${AUDIT_BEAT_RECIPE}" \
    --arg at "$(date -u +%FT%TZ)" '{repo: $repo, sha: $sha, recipe: $recipe, startedAt: $at}')" \
    || _audit_beat_current="null"
  audit_beat
  local every="${JERYU_AUDIT_BEAT_EVERY:-60}"
  [[ "${every}" =~ ^[1-9][0-9]*$ ]] || every=60
  # The keepalive must never run the runner's EXIT trap (it removes the work dir).
  ( trap - EXIT; while sleep "${every}"; do audit_beat; done ) >/dev/null 2>&1 &
  _audit_beat_keepalive=$!
}

audit_beat_stop_keepalive() {
  [ -n "${_audit_beat_keepalive}" ] || return 0
  pkill -P "${_audit_beat_keepalive}" 2>/dev/null || true
  kill "${_audit_beat_keepalive}" 2>/dev/null || true
  wait "${_audit_beat_keepalive}" 2>/dev/null || true
  _audit_beat_keepalive=""
}

# The audit ended: audit_beat_finish <owner/name> <sha> <conclusion> <seconds> [reason].
# The result is kept for the next idle run's beat, then beaten as `last`.
audit_beat_finish() {
  audit_beat_stop_keepalive
  [ "${_audit_beat_ready}" = 1 ] || return 0
  _audit_beat_current="null"
  local last
  last="$(jq -cn --arg repo "$1" --arg sha "$2" --arg recipe "${AUDIT_BEAT_RECIPE}" \
    --arg conclusion "$3" --argjson seconds "$4" --arg reason "${5:-}" \
    --arg at "$(date -u +%FT%TZ)" '
    {repo: $repo, sha: $sha, recipe: $recipe, conclusion: $conclusion,
     seconds: $seconds, finishedAt: $at}
    + (if $reason != "" then {reason: $reason} else {} end)')" || return 0
  _audit_beat_finished="${last}"
  if mkdir -p "$(dirname "${_audit_beat_state}")" 2>/dev/null; then
    printf '%s\n' "${last}" >"${_audit_beat_state}.tmp" 2>/dev/null \
      && mv -f "${_audit_beat_state}.tmp" "${_audit_beat_state}" 2>/dev/null || true
  fi
  audit_beat
}
