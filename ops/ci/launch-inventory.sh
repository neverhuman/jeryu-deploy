#!/usr/bin/env bash
# Managed-launch inventory (lane A). Revision: launch-inventory-1
#
# Report-only unless --enforce. Milestone M6 makes --enforce the required gate.
# This script does not prove a launch runs in Firecracker. It reports source
# patterns that M6 must remove or that an owner must classify as not an agent launch.
set -euo pipefail
LC_ALL=C

REVISION=launch-inventory-1
ENFORCE=0
ROOT=
ALLOW=
SELF_TEST=0

usage() {
  cat <<'EOF'
usage: launch-inventory.sh [--enforce] [--root DIR] [--allow FILE] [--self-test]

Default is report-only: print findings and exit 0.
--enforce exits 1 on an unsuppressed finding or a stale allowlist entry.
A debt entry (expires=M6) suppresses a finding only while report-only.
A not-agent entry (expires=permanent) suppresses a finding in both modes.
A missing or malformed allowlist exits 2 in both modes.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --enforce) ENFORCE=1; shift ;;
    --root)
      [[ $# -ge 2 ]] || { echo "launch-inventory: --root needs a directory" >&2; exit 2; }
      ROOT=$2; shift 2 ;;
    --allow)
      [[ $# -ge 2 ]] || { echo "launch-inventory: --allow needs a file" >&2; exit 2; }
      ALLOW=$2; shift 2 ;;
    --self-test) SELF_TEST=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "launch-inventory: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ $SELF_TEST -eq 1 ]]; then
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  fail=0
  assert_eq() {
    local got=$1 want=$2 label=$3
    if [[ $got -ne $want ]]; then
      echo "self-test failed: $label exit $got want $want" >&2
      fail=1
    fi
  }
  assert_out() {
    local label=$1 needle=$2
    if ! grep -F -q -- "$needle" <<<"$3"; then
      echo "self-test failed: $label missing [$needle]" >&2
      fail=1
    fi
  }
  assert_absent() {
    local label=$1 needle=$2
    if grep -F -q -- "$needle" <<<"$3"; then
      echo "self-test failed: $label unexpectedly contained [$needle]" >&2
      fail=1
    fi
  }
  repo=$tmp/repo
  mkdir -p "$repo/ops/ci" "$repo/src" "$repo/ops/pr-redteam" "$repo/contracts/connection-v1"
  git -C "$repo" init -q
  printf 'PtyBackend::Native\n' > "$repo/ops/ci/launch-inventory.sh"
  printf 'PtyBackend::DockerHost\n' > "$repo/contracts/connection-v1/note.md"
  cat > "$repo/src/agent.rs" <<'EOF'
backend: PtyBackend::Native,
other: PtyBackend::DockerHost,
spawn_command_on_pty(&spec);
import_from_host(&options);
seed_agent_auth_from_home(&home);
args: &["--dangerously-skip-permissions"],
codex: &["--yolo"],
let cmd = "docker run --rm agent";
fn run_agent() {}
EOF
  printf 'command -v claude\n' > "$repo/ops/pr-redteam/install.sh"
  printf 'docker run --rm debian:stable\n' > "$repo/src/release.sh"
  printf 'fn run_agent_json() {}\npodman running containers\n' > "$repo/src/not_launch.rs"
  printf 'ExecutionPurpose::ManagedAgent\nconnection_id\n' > "$repo/src/bound.rs"
  git -C "$repo" add .
  allow=$tmp/allow
  cat > "$allow" <<'EOF'
# fixture allowlist
src/agent.rs	pty_backend_native	debt	lane-C	M6	host PTY retired at M6
src/release.sh	docker_or_podman_run	not-agent	lane-C	permanent	image build, not an agent launch
EOF
  out=$(bash "$0" --root "$repo" --allow "$allow" || true)
  status=${PIPESTATUS[0]}
  # pipe status is lost because of assignment; re-run for the code
  set +e
  out=$(bash "$0" --root "$repo" --allow "$allow" 2>&1)
  status=$?
  set -e
  assert_eq "$status" 0 "report-only with debt"
  assert_out "report lists debt as allowed" "allowed debt pty_backend_native src/agent.rs" "$out"
  assert_out "report lists unlisted rule" "violation pty_backend_docker_host src/agent.rs" "$out"
  assert_out "report lists yolo" "violation yolo src/agent.rs" "$out"
  assert_out "report lists skip-permissions" "violation dangerously_skip_permissions src/agent.rs" "$out"
  assert_out "report lists spawn" "violation spawn_command_on_pty src/agent.rs" "$out"
  assert_out "report lists import" "violation import_from_host src/agent.rs" "$out"
  assert_out "report lists seed" "violation host_home_seed src/agent.rs" "$out"
  assert_out "report lists docker agent" "violation docker_or_podman_run src/agent.rs" "$out"
  assert_out "report lists missing connection" "violation managed_launch_without_connection_id src/agent.rs" "$out"
  assert_out "report lists pr-redteam" "violation managed_launch_without_connection_id ops/pr-redteam/install.sh" "$out"
  assert_absent "script text is excluded" "ops/ci/launch-inventory.sh" "$out"
  assert_absent "contract text is excluded" "contracts/connection-v1/note.md" "$out"
  assert_absent "bound file is not a violation" "violation managed_launch_without_connection_id src/bound.rs" "$out"
  assert_absent "helper name is not a launch" "src/not_launch.rs" "$out"
  assert_out "release docker is allowed" "allowed not-agent docker_or_podman_run src/release.sh" "$out"

  set +e
  out=$(bash "$0" --enforce --root "$repo" --allow "$allow" 2>&1)
  status=$?
  set -e
  assert_eq "$status" 1 "enforce ignores M6 debt"
  assert_out "enforce still allows not-agent" "allowed not-agent docker_or_podman_run src/release.sh" "$out"
  assert_out "enforce resurfaces debt" "violation pty_backend_native src/agent.rs" "$out"

  printf 'src/missing.rs\tdocker_or_podman_run\tnot-agent\tlane-C\tpermanent\tstale row must fail enforce\n' >> "$allow"
  set +e
  out=$(bash "$0" --root "$repo" --allow "$allow" 2>&1)
  status=$?
  set -e
  assert_eq "$status" 0 "report-only tolerates a stale row"
  assert_out "stale row is visible" "stale not-agent docker_or_podman_run src/missing.rs" "$out"
  set +e
  out=$(bash "$0" --enforce --root "$repo" --allow "$allow" 2>&1)
  status=$?
  set -e
  assert_eq "$status" 1 "enforce rejects a stale row"

  printf 'not-enough-fields\n' > "$tmp/bad.allow"
  set +e
  bash "$0" --root "$repo" --allow "$tmp/bad.allow" >/dev/null 2>&1
  status=$?
  set -e
  assert_eq "$status" 2 "malformed allowlist"
  if [[ $fail -ne 0 ]]; then
    echo "launch-inventory self-test failed" >&2
    exit 1
  fi
  echo "launch-inventory self-test passed ($REVISION)"
  exit 0
fi

if [[ -z $ROOT ]]; then
  ROOT=$(cd "$(dirname "$0")/../.." && pwd)
fi
if [[ -z $ALLOW ]]; then
  ALLOW=$ROOT/ops/ci/launch-inventory.allow
fi
if [[ ! -d $ROOT/.git && ! -f $ROOT/.git ]]; then
  echo "launch-inventory: $ROOT is not a git checkout" >&2
  exit 2
fi
if [[ ! -f $ALLOW ]]; then
  echo "launch-inventory: allowlist missing: $ALLOW" >&2
  exit 2
fi

declare -A FIND_LINE=()
declare -A FIND_COUNT=()
declare -A LAUNCH_FILE=()
declare -A ALLOW_CLASS=()
declare -A ALLOW_OWNER=()
declare -A ALLOW_EXPIRES=()
declare -A ALLOW_REASON=()

add_finding() {
  local rule=$1 path=$2 line=$3
  local key=$rule$'\t'$path
  if [[ -z ${FIND_LINE[$key]+x} ]]; then
    FIND_LINE[$key]=$line
    FIND_COUNT[$key]=1
  else
    FIND_COUNT[$key]=$((FIND_COUNT[$key] + 1))
  fi
}

note_launch() {
  local path=$1
  LAUNCH_FILE[$path]=1
}

classify_line() {
  local path=$1 line_no=$2 text=$3
  [[ $text == *'PtyBackend::Native'* ]] && add_finding pty_backend_native "$path" "$line_no"
  [[ $text == *'PtyBackend::DockerHost'* ]] && add_finding pty_backend_docker_host "$path" "$line_no"
  if [[ $text == *'enum PtyBackend'* ]]; then
    add_finding pty_backend_native "$path" "$line_no"
    add_finding pty_backend_docker_host "$path" "$line_no"
  fi
  [[ $text == *'spawn_command_on_pty'* ]] && add_finding spawn_command_on_pty "$path" "$line_no"
  [[ $text == *'import_from_host'* ]] && add_finding import_from_host "$path" "$line_no"
  if [[ $text == *'seed_agent_auth'* || $text == *'JERYU_AUTH_HOME'* || $text == *'host_rel: ".codex'* || $text == *'host_rel: ".claude'* || $text == *'host_rel: ".gemini'* || $text == *'host_rel: ".jekko'* ]]; then
    add_finding host_home_seed "$path" "$line_no"
  fi
  [[ $text == *'--dangerously-skip-permissions'* ]] && add_finding dangerously_skip_permissions "$path" "$line_no"
  if [[ $text =~ (^|[^A-Za-z0-9_])--yolo([^A-Za-z0-9_]|$) ]]; then
    add_finding yolo "$path" "$line_no"
  fi
  if [[ $text =~ (^|[^A-Za-z0-9_])(docker|podman)[[:space:]]+run([^A-Za-z0-9_]|$) ]]; then
    add_finding docker_or_podman_run "$path" "$line_no"
  fi
  if [[ $text == *'dispatch_managed'* || $text == *'SessionService::start'* || $text == *'seed_agent_auth'* || $text == *'ExecutionPurpose::ManagedAgent'* || $text == *'fn run_agent('* || $text == *'PtyBackend::Native'* || $text == *'PtyBackend::DockerHost'* || $text == *'spawn_command_on_pty'* || $text == *'import_from_host'* || $text == *'enum PtyBackend'* ]]; then
    note_launch "$path"
  fi
}

scan() {
  local excludes=(
    ':(exclude)ops/ci/launch-inventory.sh'
    ':(exclude)ops/ci/launch-inventory.allow'
    ':(exclude)ops/ci/leak-oracle.sh'
    ':(exclude)contracts/connection-v1'
  )
  local pattern='PtyBackend::(Native|DockerHost)|enum PtyBackend|spawn_command_on_pty|import_from_host|seed_agent_auth|JERYU_AUTH_HOME|host_rel: "\.(codex|claude|gemini|jekko)|--dangerously-skip-permissions|--yolo|(docker|podman)[[:space:]]+run|dispatch_managed|SessionService::start|ExecutionPurpose::ManagedAgent|fn run_agent\('
  local hit path line_no text status
  set +e
  hit=$(git -C "$ROOT" grep -n -I -E -e "$pattern" -- . "${excludes[@]}" 2>/dev/null)
  status=$?
  set -e
  if [[ $status -gt 1 ]]; then
    echo "launch-inventory: git grep failed in $ROOT" >&2
    exit 2
  fi
  while IFS= read -r row; do
    [[ -z $row ]] && continue
    path=${row%%:*}
    row=${row#*:}
    line_no=${row%%:*}
    text=${row#*:}
    [[ $line_no =~ ^[0-9]+$ ]] || continue
    classify_line "$path" "$line_no" "$text"
  done <<<"$hit"

  set +e
  hit=$(git -C "$ROOT" grep -n -I -E -e '(^|[^A-Za-z0-9_])(claude|codex|grok)([^A-Za-z0-9_]|$)' -- ops/pr-redteam 2>/dev/null)
  status=$?
  set -e
  if [[ $status -gt 1 ]]; then
    echo "launch-inventory: git grep failed for pr-redteam in $ROOT" >&2
    exit 2
  fi
  if [[ $status -eq 0 ]]; then
    while IFS= read -r row; do
      [[ -z $row ]] && continue
      path=${row%%:*}
      [[ $path == ops/pr-redteam/*.sh || $path == ops/pr-redteam/*/*.sh ]] && note_launch "$path"
    done <<<"$hit"
  fi

  local launch
  for launch in "${!LAUNCH_FILE[@]}"; do
    set +e
    git -C "$ROOT" grep -q -I -F -e connection_id -- "$launch"
    status=$?
    set -e
    if [[ $status -eq 1 ]]; then
      add_finding managed_launch_without_connection_id "$launch" "${FIND_LINE[pty_backend_native$'\t'$launch]:-1}"
    elif [[ $status -gt 1 ]]; then
      echo "launch-inventory: git grep failed for connection_id in $launch" >&2
      exit 2
    fi
  done
}

load_allow() {
  local line_no=0 tabs path rule class owner expires reason key
  while IFS= read -r line || [[ -n $line ]]; do
    line_no=$((line_no + 1))
    [[ $line =~ ^[[:space:]]*# ]] && continue
    [[ -z ${line//[[:space:]]/} ]] && continue
    tabs=$(awk -F '\t' '{print NF}' <<<"$line")
    if [[ $tabs -ne 6 ]]; then
      echo "launch-inventory: $ALLOW:$line_no must have 6 tab-separated fields" >&2
      exit 2
    fi
    IFS=$'\t' read -r path rule class owner expires reason <<<"$line"
    case "$rule" in
      pty_backend_native|pty_backend_docker_host|spawn_command_on_pty|import_from_host|host_home_seed|dangerously_skip_permissions|yolo|docker_or_podman_run|managed_launch_without_connection_id) ;;
      *) echo "launch-inventory: $ALLOW:$line_no unknown rule $rule" >&2; exit 2 ;;
    esac
    case "$class" in
      debt|not-agent) ;;
      *) echo "launch-inventory: $ALLOW:$line_no class must be debt or not-agent" >&2; exit 2 ;;
    esac
    if [[ $class == debt && $expires != M6 ]]; then
      echo "launch-inventory: $ALLOW:$line_no debt expires at M6" >&2
      exit 2
    fi
    if [[ $class == not-agent && $expires != permanent ]]; then
      echo "launch-inventory: $ALLOW:$line_no not-agent expires permanent" >&2
      exit 2
    fi
    if [[ ! $owner =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]]; then
      echo "launch-inventory: $ALLOW:$line_no owner is missing" >&2
      exit 2
    fi
    if [[ ${#reason} -lt 12 || $path == /* || $path == *..* || $path == *$'\t'* ]]; then
      echo "launch-inventory: $ALLOW:$line_no path or reason is invalid" >&2
      exit 2
    fi
    key=$rule$'\t'$path
    if [[ -n ${ALLOW_CLASS[$key]+x} ]]; then
      echo "launch-inventory: $ALLOW:$line_no duplicates $rule $path" >&2
      exit 2
    fi
    ALLOW_CLASS[$key]=$class
    ALLOW_OWNER[$key]=$owner
    ALLOW_EXPIRES[$key]=$expires
    ALLOW_REASON[$key]=$reason
  done < "$ALLOW"
}

suppressed() {
  local key=$1 class expires
  [[ -n ${ALLOW_CLASS[$key]+x} ]] || return 1
  class=${ALLOW_CLASS[$key]}
  expires=${ALLOW_EXPIRES[$key]}
  if [[ $class == not-agent && $expires == permanent ]]; then
    return 0
  fi
  if [[ $ENFORCE -eq 0 && $class == debt && $expires == M6 ]]; then
    return 0
  fi
  return 1
}

scan
load_allow

mode=report-only
[[ $ENFORCE -eq 1 ]] && mode=enforce
echo "launch-inventory: revision=$REVISION mode=$mode root=$ROOT"

violations=0
allowed=0
stale=0
while IFS= read -r key; do
  [[ -z $key ]] && continue
  rule=${key%%$'\t'*}
  path=${key#*$'\t'}
  if suppressed "$key"; then
    echo "allowed ${ALLOW_CLASS[$key]} $rule $path:${FIND_LINE[$key]} owner=${ALLOW_OWNER[$key]} expires=${ALLOW_EXPIRES[$key]} count=${FIND_COUNT[$key]}"
    allowed=$((allowed + 1))
  else
    echo "violation $rule $path:${FIND_LINE[$key]} count=${FIND_COUNT[$key]}"
    violations=$((violations + 1))
  fi
done < <(printf '%s\n' "${!FIND_LINE[@]}" | sort)

while IFS= read -r key; do
  [[ -z $key ]] && continue
  if [[ -z ${FIND_LINE[$key]+x} ]]; then
    rule=${key%%$'\t'*}
    path=${key#*$'\t'}
    echo "stale ${ALLOW_CLASS[$key]} $rule $path owner=${ALLOW_OWNER[$key]}"
    stale=$((stale + 1))
  fi
done < <(printf '%s\n' "${!ALLOW_CLASS[@]}" | sort)

echo "launch-inventory: violations=$violations allowed=$allowed stale=$stale"
if [[ $ENFORCE -eq 1 && ( $violations -gt 0 || $stale -gt 0 ) ]]; then
  exit 1
fi
exit 0
