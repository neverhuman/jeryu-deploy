#!/usr/bin/env bash
# Offline fixtures exercise the same stage runner used by the required gate.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"
mkdir -p target/evidence/required-fixtures
work="$(mktemp -d "$repo_root/target/evidence/required-fixtures/run.XXXXXXXX")"
source ops/ci/gate-evidence.sh
gate_evidence_prune "$repo_root/target/evidence/required-fixtures" "$work"
cat > "$work/fixture.sh" <<'FIXTURE'
set -euo pipefail
cd "$1"
repo_root="$PWD"
source ops/ci/gate-evidence.sh
source ops/ci/gate-preflight.sh
gate_evidence_init
printf '%s\n' "$gate_run" > "$2"
case "$3" in
  failure)
    # Even a successful EXIT cleanup must preserve the failed command's code.
    trap 'code=$?; gate_stage later-success true; gate_evidence_finish "$code"' EXIT
    fail() { echo intentional-failure; return 27; echo unreachable; }
    gate_stage injected-failure fail
    gate_stage later-success true ;;
  missing) JERYU_TOOL_RENDER="$2.absent"; gate_stage preflight gate_preflight ;;
  credential-missing)
    git() { return 1; }
    unset JERYU_WEB_PIN_TOKEN_FILE
    gate_stage credential-preflight gate_web_credential_preflight ;;
  credential-present)
    git() { printf 'password=fixture-read-value\n'; }
    unset JERYU_WEB_PIN_TOKEN_FILE
    gate_stage credential-preflight gate_web_credential_preflight
    gate_complete=true ;;
  missing-tool)
    command() {
      if [[ "$*" == '-v cargo-audit' ]]; then return 1; fi
      builtin command "$@"
    }
    gate_stage preflight gate_preflight ;;
  nonexecutable)
    printf '#!/bin/sh\nexit 0\n' > "$2.renderer"
    chmod 600 "$2.renderer"
    JERYU_TOOL_RENDER="$2.renderer"; gate_stage preflight gate_preflight ;;
  killed) gate_stage killed bash -c 'kill -KILL "$PPID"; sleep 1' ;;
  interruption) gate_stage interrupted bash -c 'kill -TERM "$PPID"; sleep 1' ;;
  incomplete) gate_stage first true ;;
  success) gate_stage first true; gate_stage later-success true; gate_complete=true ;;
esac
FIXTURE
run_case() {
  local case_name="$1" expected="$2" actual=0 run
  bash "$work/fixture.sh" "$repo_root" "$work/$case_name.path" "$case_name" > "$work/$case_name.log" 2>&1 || actual=$?
  [[ "$actual" == "$expected" ]] || { cat "$work/$case_name.log"; echo "$case_name exit=$actual expected=$expected" >&2; exit 1; }
  run="$(cat "$work/$case_name.path")"
  if [[ "$case_name" == killed ]]; then
    jq -e '.conclusion == "running" and .exit_code == null and .active_stage.name == "killed"' "$run/receipt.json" >/dev/null
    return
  fi
  jq -e --argjson code "$expected" '.exit_code == $code and .merge_authority == false and (.source_head|length == 40) and (.gate_definition_sha256|length == 64)' "$run/receipt.json" >/dev/null
  if [[ "$expected" != 0 ]]; then jq -e '.conclusion != "success"' "$run/receipt.json" >/dev/null; fi
  case "$case_name" in
    failure) jq -e '.conclusion == "failure" and (.stages|length == 2) and .stages[1].exit_code == 0 and .stages[0].exit_code == 27 and .stages[0].name == "injected-failure" and (.stages[0].command_id|length > 0) and .stages[0].duration_seconds >= 0' "$run/receipt.json" >/dev/null
      grep -q intentional-failure "$(jq -r '.stages[0].log' "$run/receipt.json")" ;;
    missing) grep -q "renderer: $work/$case_name.path.absent; set JERYU_TOOL_RENDER" "$(jq -r '.stages[0].log' "$run/receipt.json")" ;;
    credential-missing) grep -q 'missing prerequisite: web status read credential; repair: set JERYU_WEB_PIN_TOKEN_FILE' "$(jq -r '.stages[0].log' "$run/receipt.json")" ;;
    credential-present)
      jq -e '.conclusion == "success"' "$run/receipt.json" >/dev/null
      if grep -q fixture-read-value "$(jq -r '.stages[0].log' "$run/receipt.json")"; then exit 1; fi ;;
    missing-tool) grep -q 'missing prerequisite: cargo-audit; repair: cargo install --locked cargo-audit' "$(jq -r '.stages[0].log' "$run/receipt.json")" ;;
    interruption) jq -e '.stages[0].exit_code == 143 and .stages[0].name == "interrupted"' "$run/receipt.json" >/dev/null ;;
    success) jq -e '.conclusion == "success" and (.stages|length == 2)' "$run/receipt.json" >/dev/null ;;
  esac
}
run_case failure 27
run_case missing 1
run_case credential-missing 1
run_case credential-present 0
run_case missing-tool 1
run_case nonexecutable 1
run_case interruption 143
run_case killed 137
run_case incomplete 1
run_case success 0
# Retained evidence must stay bounded: a gate host runs this recipe every push.
prune_case() {
  local root="$work/prune" dir i
  mkdir -p "$root"
  for i in 1 2 3 4 5 6 7; do
    dir="$root/run.final$i"
    mkdir -p "$dir"
    jq -nc --argjson started "$i" '{conclusion:"failure", started_at:$started}' > "$dir/receipt.json"
  done
  mkdir -p "$root/run.livefresh" "$root/run.livestale"
  jq -nc '{conclusion:"running", started_at:1}' > "$root/run.livefresh/receipt.json"
  jq -nc '{conclusion:"running", started_at:1}' > "$root/run.livestale/receipt.json"
  touch -d '30 days ago' "$root/run.livestale"
  mkdir -p "$root/run.current"
  touch -d '30 days ago' "$root/run.current"
  JERYU_GATE_EVIDENCE_KEEP=3 JERYU_GATE_EVIDENCE_STALE_DAYS=7 \
    gate_evidence_prune "$root" "$root/run.current"
  local kept
  kept="$(cd "$root" && printf '%s\n' run.* | sort | tr '\n' ' ')"
  [[ "$kept" == "run.current run.final5 run.final6 run.final7 run.livefresh " ]] || {
    echo "prune kept: $kept" >&2; exit 1; }
  # A tolerated non-numeric cap must not delete anything beyond the stale run.
  JERYU_GATE_EVIDENCE_KEEP=nonsense gate_evidence_prune "$root" "$root/run.current"
  kept="$(cd "$root" && printf '%s\n' run.* | sort | tr '\n' ' ')"
  [[ "$kept" == "run.current run.final5 run.final6 run.final7 run.livefresh " ]] || {
    echo "prune (default cap) kept: $kept" >&2; exit 1; }
}
prune_case
echo "gate evidence fixtures passed; retained at $work"
