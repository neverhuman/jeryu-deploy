#!/usr/bin/env bash
# jankurai-gate — the pre-approval quality gate, run locally, before the PR.
#
#   ops/ci/jankurai-gate.sh                 audit HEAD against origin/main and print the verdict
#   ops/ci/jankurai-gate.sh --base-ref REV  diff against REV instead of the merge-base with origin/main
#   ops/ci/jankurai-gate.sh --report FILE    render the verdict of an audit report already written
#
# Exit 0 when the hosted `jankurai/proof` check would pass on this head, 1 when
# it would fail: score below the effective floor, any cap applied, any hard
# finding, or an audit that produced no score (which fails closed, with the
# reason the audit failed for). The words it prints are the words the hosted
# check-run carries, rendered from the same report by the same rules as
# `jankurai_proof_output` in crates/jeryu-api/src/ci_bridge/jankurai.rs, so the
# local verdict and the hosted one cannot disagree about one sha.
# `cargo test -p jeryu-api --features web jankurai_gate_script` pins that.
#
# Per-repo rollout. A repo is under the gate when `agent/jankurai-gate.toml`
# says `enabled = true`, or when JERYU_JANKURAI_GATE=1 says so for this run;
# JERYU_JANKURAI_GATE=0 turns it off again. A repo that is not under the gate
# still gets the whole verdict printed, and exit 0 — a repo whose main does not
# clear the floor yet is not frozen by a gate nobody enabled for it.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"

base_ref=""
report=""
while (($#)); do
  case "$1" in
    --base-ref) base_ref="${2:?--base-ref needs a revision}"; shift 2 ;;
    --report) report="${2:?--report needs a path}"; shift 2 ;;
    -h|--help) sed -n '2,25p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) printf 'jankurai-gate: unknown argument: %s\n' "$1" >&2; exit 2 ;;
  esac
done

# Whether a failing verdict refuses the PR here, or only reports itself.
gate_enabled() {
  case "${JERYU_JANKURAI_GATE:-}" in
    1|true|yes|on) return 0 ;;
    0|false|no|off) return 1 ;;
  esac
  [[ -s agent/jankurai-gate.toml ]] || return 1
  grep -Eq '^[[:space:]]*enabled[[:space:]]*=[[:space:]]*true[[:space:]]*(#.*)?$' \
    agent/jankurai-gate.toml
}

# The audit the host runs on a push: the exact head against its merge-base with
# the real trunk, --advisory-only so the JSON is always written and this script
# derives the strict verdict from it, exactly as the host does. Sets host_error
# to what went wrong, and tool_exit to the code the hosted score would record.
host_error=""
tool_exit=0
run_audit() {
  local out="$1" base="$2"
  local args=(diff-audit "${repo_root}" --base-ref "${base}"
    --json "${out}" --advisory-only)
  # The host audits an unconfigured repo with its own default policy and, with
  # no owner map to prove lanes against, skips the proof plan. Mirror that.
  [[ -e agent/owner-map.json ]] || args+=(--skip-proof)
  source ops/ci/lib.sh
  if ! require_jankurai; then
    host_error="the governed jankurai identity was rejected"
    tool_exit=-1
    return 0
  fi
  local stderr_file="${out}.stderr" code=0
  command "${JERYU_GOVERNED_JANKURAI_BIN}" "${args[@]}" 2>"${stderr_file}" || code=$?
  tool_exit="${code}"
  ((code == 0)) && return 0
  local tail_line
  tail_line="$(grep -v '^[[:space:]]*$' "${stderr_file}" | tail -1 || true)"
  if [[ -n "${tail_line}" ]]; then
    host_error="the auditor exited ${code}: ${tail_line}"
  else
    host_error="the auditor exited ${code}"
  fi
}

if [[ -z "${report}" ]]; then
  report="target/jankurai/gate/diff-score.json"
  mkdir -p "$(dirname "${report}")"
  rm -f "${report}"
  if [[ -z "${base_ref}" ]]; then
    base_ref="$(git merge-base origin/main HEAD 2>/dev/null || true)"
    if [[ -z "${base_ref}" ]]; then
      printf 'jankurai-gate: no merge-base with origin/main; fetch the trunk first\n' >&2
      exit 2
    fi
  fi
  run_audit "${report}" "${base_ref}"
fi

verdict_status=0
JANKURAI_GATE_REPORT="${report}" JANKURAI_GATE_HOST_ERROR="${host_error}" \
  JANKURAI_GATE_TOOL_EXIT="${tool_exit}" python3 - <<'PY' || verdict_status=$?
import json
import os
import sys

# The host's own floor for every head it scores, whatever a repository's policy
# asks for (ci_bridge::HOST_JANKURAI_MINIMUM_SCORE).
HOST_MINIMUM_SCORE = 85
MAX_REASON_CHARS = 200
PROOF_TEXT_FINDINGS = 5

path = os.environ["JANKURAI_GATE_REPORT"]
host_error = os.environ["JANKURAI_GATE_HOST_ERROR"].strip() or None
try:
    report = json.loads(open(path, encoding="utf-8").read())
except (OSError, ValueError):
    report = None


def truncate(reason):
    reason = reason.strip()
    return reason if len(reason) <= MAX_REASON_CHARS else reason[:MAX_REASON_CHARS] + "..."


def scored(report):
    """The fields a verdict needs, or None when the report is not a diff score."""
    if not isinstance(report, dict):
        return None
    decision = report.get("decision")
    caps = report.get("caps_applied")
    if not isinstance(decision, dict) or not isinstance(caps, list):
        return None
    score, hard = report.get("score"), decision.get("hard_findings")
    minimum = decision.get("minimum_score")
    values = (score, hard, minimum)
    if any(not isinstance(value, int) or isinstance(value, bool) for value in values):
        return None
    if any(value < 0 for value in values) or score > 100 or minimum > 100:
        return None
    if any(not isinstance(cap, str) for cap in caps):
        return None
    return score, hard, minimum, caps


def top_findings_text(report):
    findings = (report or {}).get("findings")
    if not isinstance(findings, list) or not findings:
        return (
            "The audit recorded no individual findings; the Quality gate page of this "
            "head has the full report."
        )
    lines = ["Top findings:"]
    for finding in findings[:PROOF_TEXT_FINDINGS]:
        finding = finding if isinstance(finding, dict) else {}
        rule = finding.get("rule_id") if isinstance(finding.get("rule_id"), str) else "unknown"
        where = finding.get("path") if isinstance(finding.get("path"), str) else "(no path)"
        line = finding.get("line")
        if isinstance(line, int) and not isinstance(line, bool):
            where = f"{where}:{line}"
        problem = finding.get("problem")
        problem = problem if isinstance(problem, str) else "no description"
        lines.append(f"- {rule} at {where}: {problem}")
    if len(findings) > PROOF_TEXT_FINDINGS:
        lines.append(
            f"- ... and {len(findings) - PROOF_TEXT_FINDINGS} more on the Quality gate "
            "page of this head."
        )
    return "\n".join(lines)


parsed = None if host_error else scored(report)
if parsed is None:
    # Fails closed, naming what went wrong: "tool-failed" alone tells a reader
    # nothing (ci_bridge::jankurai::tool_failure_reason).
    if host_error:
        reason = truncate(host_error)
    elif isinstance(report, dict) and isinstance(report.get("host_error"), str):
        reason = truncate(report["host_error"])
    elif report is not None:
        reason = (
            "the audit report is not a valid diff-score JSON (score, caps_applied, "
            "decision.hard_findings and decision.minimum_score must all be present "
            "and in range)"
        )
    else:
        reason = "the audit wrote no report at all"
    exit_code = os.environ.get("JANKURAI_GATE_TOOL_EXIT", "").strip() or "unknown"
    print(f"jankurai audit produced no score: {reason}")
    print()
    print(
        "The authoritative jankurai audit did not produce a valid report (decision "
        f"`tool-failed`, exit {exit_code}); the proof fails closed.\n\n"
        f"- reason: {reason}\n"
        "- what to do: rerun the audit on this head once the reason above is "
        "addressed; the Quality gate page of this head keeps the audit's own output."
    )
    print()
    print(f"jankurai audit failed: {reason}")
    sys.exit(1)

score, hard, minimum, caps = parsed
floor = max(minimum, HOST_MINIMUM_SCORE)
passed = score >= floor and hard == 0 and not caps
if passed:
    title = f"score {score} >= floor {floor}"
elif score < floor:
    title = f"score {score} < floor {floor}"
elif hard > 0:
    title = f"score {score} >= floor {floor}, {hard} hard finding(s)"
else:
    title = f"score {score} >= floor {floor}, {len(caps)} cap(s) applied"
print(title)
print()
print(
    f"- score: {score}\n- floor: {floor}\n"
    f"- caps applied: {', '.join(caps) if caps else 'none'}\n- hard findings: {hard}"
)
if not passed:
    print()
    print(top_findings_text(report))
sys.exit(0 if passed else 1)
PY

if ((verdict_status == 0)); then
  printf '\njankurai-gate: PASS — the hosted jankurai/proof passes on this head\n'
  exit 0
fi
if gate_enabled; then
  printf '\njankurai-gate: FAIL — the hosted jankurai/proof fails on this head, and this\n'
  printf 'repository is under the gate: fix the above before opening or updating a PR.\n'
  exit 1
fi
printf '\njankurai-gate: the hosted jankurai/proof fails on this head. This repository is\n'
printf 'not under the gate yet (agent/jankurai-gate.toml), so this is a report, not a\n'
printf 'refusal. Enable the gate once main clears the floor.\n'
exit 0
