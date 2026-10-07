#!/usr/bin/env bash
# Leak oracle (lane A). A canary must be absent from the scanned bytes.
#
# The canary is read from a file and passed to grep with -f, never as an
# argument. This script does not prove a real provider token is absent.
# It proves only that this canary is absent from the paths it was given.
set -euo pipefail
LC_ALL=C
umask 077

CANARY_FILE=
PROBE_FILES=()
SCAN_PATHS=()
SELF_TEST=0

usage() {
  cat <<'EOF'
usage: leak-oracle.sh --canary-file FILE --scan PATH [PATH ...]
       leak-oracle.sh --canary-file FILE --require-probe FILE [--scan PATH ...]
       leak-oracle.sh --self-test

Exit 0 when every scan path lacks the canary and every probe file contains
LEAK_ORACLE_PROBE=absent and no LEAK_ORACLE_PROBE=present.
Exit 1 when the canary or a present probe is found. The canary value is not printed.
Exit 2 on usage errors. The canary must be one line of at least 24 bytes.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --canary-file)
      [[ $# -ge 2 ]] || { echo "leak-oracle: --canary-file needs a file" >&2; exit 2; }
      CANARY_FILE=$2; shift 2 ;;
    --scan)
      [[ $# -ge 2 ]] || { echo "leak-oracle: --scan needs a path" >&2; exit 2; }
      SCAN_PATHS+=("$2"); shift 2 ;;
    --require-probe)
      [[ $# -ge 2 ]] || { echo "leak-oracle: --require-probe needs a file" >&2; exit 2; }
      PROBE_FILES+=("$2"); shift 2 ;;
    --self-test) SELF_TEST=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "leak-oracle: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ $SELF_TEST -eq 1 ]]; then
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  canary='jeryu-canary-selftest-0123456789abcdef'
  printf '%s\n' "$canary" > "$tmp/canary"
  mkdir -p "$tmp/clean" "$tmp/dirty" "$tmp/frames"
  printf 'hello\n' > "$tmp/clean/log.txt"
  printf 'prefix %s suffix\n' "$canary" > "$tmp/dirty/log.txt"
  printf 'LEAK_ORACLE_PROBE=absent\n' > "$tmp/frames/ok.txt"
  printf 'LEAK_ORACLE_PROBE=present\n' > "$tmp/frames/bad.txt"
  printf 'LEAK_ORACLE_PROBE=absent\n%s\n' "$canary" > "$tmp/frames/leaked.txt"
  fail=0
  check() {
    local want=$1 label=$2
    shift 2
    set +e
    out=$("$@" 2>&1)
    status=$?
    set -e
    if [[ $status -ne $want ]]; then
      echo "self-test failed: $label exit $status want $want" >&2
      echo "$out" >&2
      fail=1
    fi
    if grep -F -q -- "$canary" <<<"$out"; then
      echo "self-test failed: $label printed the canary" >&2
      fail=1
    fi
    printf '%s' "$out"
  }
  out=$(check 0 "clean scan" bash "$0" --canary-file "$tmp/canary" --scan "$tmp/clean")
  grep -F -q "leak-oracle: absent" <<<"$out" || { echo "self-test failed: clean scan text" >&2; fail=1; }
  out=$(check 1 "dirty scan" bash "$0" --canary-file "$tmp/canary" --scan "$tmp/dirty")
  grep -F -q "leak-oracle: present" <<<"$out" || { echo "self-test failed: dirty scan text" >&2; fail=1; }
  check 0 "probe absent" bash "$0" --canary-file "$tmp/canary" --require-probe "$tmp/frames/ok.txt" >/dev/null
  check 1 "probe present" bash "$0" --canary-file "$tmp/canary" --require-probe "$tmp/frames/bad.txt" >/dev/null
  check 1 "probe leaked canary" bash "$0" --canary-file "$tmp/canary" --require-probe "$tmp/frames/leaked.txt" >/dev/null
  printf 'short\n' > "$tmp/short"
  set +e
  bash "$0" --canary-file "$tmp/short" --scan "$tmp/clean" >/dev/null 2>&1
  status=$?
  set -e
  if [[ $status -ne 2 ]]; then
    echo "self-test failed: short canary exit $status" >&2
    fail=1
  fi
  if [[ $fail -ne 0 ]]; then
    echo "leak-oracle self-test failed" >&2
    exit 1
  fi
  echo "leak-oracle self-test passed"
  exit 0
fi

if [[ -z $CANARY_FILE || ( ${#SCAN_PATHS[@]} -eq 0 && ${#PROBE_FILES[@]} -eq 0 ) ]]; then
  usage >&2
  exit 2
fi
if [[ ! -f $CANARY_FILE ]]; then
  echo "leak-oracle: canary file missing" >&2
  exit 2
fi
if [[ $(tr -cd '\0' < "$CANARY_FILE" | wc -c) -ne 0 ]]; then
  echo "leak-oracle: canary file contains NUL" >&2
  exit 2
fi
canary=$(<"$CANARY_FILE")
canary=${canary%$'\n'}
if [[ $canary == *$'\n'* || ${#canary} -lt 24 ]]; then
  echo "leak-oracle: canary must be one line of at least 24 bytes" >&2
  exit 2
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
printf '%s' "$canary" > "$work/pattern"
canary_dev_ino=$(stat -c '%d:%i' "$CANARY_FILE")
found=0

scan_file() {
  local file=$1
  [[ -f $file ]] || { echo "leak-oracle: not a file: $file" >&2; exit 2; }
  local id
  id=$(stat -c '%d:%i' "$file")
  if [[ $id == "$canary_dev_ino" ]]; then
    return 0
  fi
  if grep -a -F -q -f "$work/pattern" -- "$file"; then
    echo "leak-oracle: present $file"
    found=1
  fi
}

scan_path() {
  local path=$1
  if [[ -f $path ]]; then
    scan_file "$path"
    return 0
  fi
  if [[ ! -d $path ]]; then
    echo "leak-oracle: scan path missing: $path" >&2
    exit 2
  fi
  local file
  while IFS= read -r -d '' file; do
    scan_file "$file"
  done < <(find -P "$path" -type f -print0)
}

for path in "${SCAN_PATHS[@]}"; do
  scan_path "$path"
done

for probe in "${PROBE_FILES[@]}"; do
  [[ -f $probe ]] || { echo "leak-oracle: probe file missing: $probe" >&2; exit 2; }
  scan_file "$probe"
  present=$(grep -c -E '^LEAK_ORACLE_PROBE=present$' "$probe" || true)
  absent=$(grep -c -E '^LEAK_ORACLE_PROBE=absent$' "$probe" || true)
  if [[ $present -ne 0 || $absent -lt 1 ]]; then
    echo "leak-oracle: probe failed $probe"
    found=1
  fi
done

if [[ $found -ne 0 ]]; then
  exit 1
fi
echo "leak-oracle: absent"
exit 0
