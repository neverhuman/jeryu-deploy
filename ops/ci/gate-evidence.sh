#!/usr/bin/env bash
# Diagnostic evidence only: never a protected result or permission to merge.
# Source from an errexit-enabled gate; gate_stage must be a simple command.
gate_evidence_init() {
  : "${repo_root:?caller must set repo_root}"
  command -v jq >/dev/null || { echo 'missing prerequisite jq; install jq using your system package manager' >&2; exit 1; }
  mkdir -p "$repo_root/target/evidence/required"
  gate_run="$(mktemp -d "$repo_root/target/evidence/required/run.XXXXXXXX")"
  gate_steps="$gate_run/stages.jsonl"
  : > "$gate_steps"
  gate_failure=0
  gate_complete=false
  gate_active=false
  gate_started="$(date +%s)"
  gate_head="$(git rev-parse HEAD)"
  git status --porcelain=v1 --untracked-files=normal > "$gate_run/source-status.txt"
  gate_dirty=false
  [ ! -s "$gate_run/source-status.txt" ] || gate_dirty=true
  gate_definition="$(cat ops/ci/pr-ci.sh ops/ci/gate-evidence.sh ops/ci/gate-preflight.sh | sha256sum | cut -d' ' -f1)"
  # Bind all tracked recipe inputs, including dirty edits, without retaining secrets.
  gate_inputs="$(git ls-files -z ops/ci tools scripts agent Cargo.lock Cargo.toml | xargs -0 sha256sum | sha256sum | cut -d' ' -f1)"
  trap 'gate_evidence_finish $?' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  gate_receipt running null
  echo "[pr-ci] retained evidence: $gate_run" >&2
}

gate_receipt() {
  local conclusion="$1" code="$2"
  jq -n --arg head "$gate_head" --arg definition "$gate_definition" \
    --arg inputs "$gate_inputs" --arg run "$gate_run" --arg conclusion "$conclusion" \
    --argjson dirty "$gate_dirty" --argjson started "$gate_started" \
    --argjson active "$gate_active" --arg stage "${gate_stage_name:-}" \
    --arg command "${gate_command_id:-}" --arg log "${gate_stage_log:-}" \
    --argjson ended "$(date +%s)" --argjson code "$code" --slurpfile stages "$gate_steps" \
    '{schema:"jeryu.required-diagnostic/v1", merge_authority:false, source_head:$head,
      source_dirty:$dirty, source_status:($run+"/source-status.txt"),
      gate_definition_sha256:$definition, recipe_inputs_sha256:$inputs,
      run:$run, started_at:$started, updated_at:$ended, conclusion:$conclusion,
      exit_code:$code, stages:$stages,
      active_stage:(if $active then {name:$stage, command_id:$command, log:$log} else null end)}' > "$gate_run/receipt.json.next"
  mv "$gate_run/receipt.json.next" "$gate_run/receipt.json"
}

gate_evidence_finish() {
  local code="$1"
  trap - EXIT
  if (( gate_failure != 0 )); then code="$gate_failure"; fi
  if [[ "$gate_active" == true ]]; then
    (( code != 0 )) || code=1
    gate_stage_record "$code"
  fi
  if (( gate_failure != 0 )); then
    gate_receipt failure "$code"
    exit "$code"
  fi
  if (( code == 0 )) && [[ "$gate_complete" == true ]]; then
    gate_receipt success 0
  else
    (( code != 0 )) || code=1
    gate_receipt incomplete "$code"
  fi
  exit "$code"
}

gate_stage() {
  local name="$1"; shift
  gate_stage_name="$name"
  local code
  gate_active=true
  gate_stage_start="$(date +%s)"
  gate_stage_log="$gate_run/$(printf '%03d' "$(($(wc -l < "$gate_steps") + 1))")-$name.log"
  printf -v gate_command_id '%q ' "$@"
  echo "[pr-ci] stage=$name command=$gate_command_id log=$gate_stage_log" >&2
  gate_receipt running null
  # Run outside an if/! context: Bash otherwise disables errexit in functions.
  set +e
  (set -e; "$@") > "$gate_stage_log" 2>&1
  code=$?
  set -e
  # Read the regular file after the child exits. A tee pipeline would wait for
  # stray descendants holding stdout open, changing existing fixture behavior.
  if (( code != 0 )); then gate_failure="$code"; fi
  cat "$gate_stage_log"
  gate_stage_record "$code"
  gate_active=false
  if (( code != 0 )); then
    gate_failure="$code"
    gate_receipt failure "$code"
    echo "[pr-ci] FAILED stage=$name exit=$code log=$gate_stage_log" >&2
    exit "$code"
  fi
  gate_receipt running null
}

gate_stage_record() {
  local code="$1"
  jq -nc --arg name "$gate_stage_name" --arg command "$gate_command_id" --arg log "$gate_stage_log" \
    --argjson code "$code" --argjson start "$gate_stage_start" --argjson end "$(date +%s)" \
    '{name:$name, command_id:$command, exit_code:$code, duration_seconds:($end-$start), log:$log}' >> "$gate_steps"
}
