#!/usr/bin/env bash
# Proves ops/ci/ci-env.sh refuses to run a gate under JERYU_CI_MOCK, the flag
# that mocks out ci_bridge::run_job and would make a gate pass without running.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

for value in 1 true 0 ""; do
  if output="$(JERYU_CI_MOCK="$value" bash -c "source '${root}/ops/ci/ci-env.sh'" 2>&1)"; then
    if [ -n "$value" ]; then
      echo "ci-env.sh accepted JERYU_CI_MOCK=${value}" >&2
      exit 1
    fi
  else
    if [ -z "$value" ]; then
      echo "ci-env.sh rejected an empty JERYU_CI_MOCK" >&2
      exit 1
    fi
    case "$output" in
      *JERYU_CI_MOCK*) ;;
      *) echo "refusal did not name the flag: ${output}" >&2; exit 1 ;;
    esac
  fi
done

# The unset case — how every gate runs — must source cleanly and leave the flag unset.
env -u JERYU_CI_MOCK bash -c "source '${root}/ops/ci/ci-env.sh'; [ -z \"\${JERYU_CI_MOCK:-}\" ]"

echo "ci-env ok: JERYU_CI_MOCK cannot be set in a gate"
