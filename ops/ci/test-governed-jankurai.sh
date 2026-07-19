#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GOVERNED="${JERYU_JANKURAI_BIN:-/home/ubuntu/.jeryu/bin/jankurai}"
TEST_ROOT="$(mktemp -d)"

cleanup() {
  rm -rf -- "${TEST_ROOT}"
}
trap cleanup EXIT

verify_at() {
  JERYU_JANKURAI_BIN="$1" bash -c \
    'set -euo pipefail; source "$1/ops/ci/lib.sh"; require_jankurai' _ "${ROOT}"
}

expect_rejected() {
  local label="$1" path="$2"
  if verify_at "${path}" >/dev/null 2>&1; then
    printf 'hostile jankurai fixture was accepted: %s (%s)\n' "${label}" "${path}" >&2
    exit 1
  fi
}

# The governed host binary must also carry a digest-bound installation receipt.
verify_at "${GOVERNED}"

expect_rejected missing "${TEST_ROOT}/missing"

ln -s "${GOVERNED}" "${TEST_ROOT}/symlink"
expect_rejected symlink "${TEST_ROOT}/symlink"

printf '#!/usr/bin/env bash\nprintf "jankurai 1.6.10\\n"\n' > "${TEST_ROOT}/wrong-version"
chmod 0755 "${TEST_ROOT}/wrong-version"
expect_rejected wrong-version "${TEST_ROOT}/wrong-version"

printf '#!/usr/bin/env bash\nprintf "jankurai 1.6.11\\n"\n' > "${TEST_ROOT}/same-version-substitute"
chmod 0755 "${TEST_ROOT}/same-version-substitute"
expect_rejected same-version-wrong-digest "${TEST_ROOT}/same-version-substitute"

# A hostile ambient PATH is neutralized deterministically: the verifier prepends
# the governed binary directory, then proves the resulting resolution and receipt.
mkdir "${TEST_ROOT}/hostile-path"
cp -- "${TEST_ROOT}/same-version-substitute" "${TEST_ROOT}/hostile-path/jankurai"
before="$(PATH="${TEST_ROOT}/hostile-path:${PATH}" command -v jankurai)"
[[ "${before}" == "${TEST_ROOT}/hostile-path/jankurai" ]]
after="$(JERYU_JANKURAI_BIN="${GOVERNED}" PATH="${TEST_ROOT}/hostile-path:${PATH}" \
  bash -c 'set -euo pipefail; source "$1/ops/ci/lib.sh"; require_jankurai; command -v jankurai' \
  _ "${ROOT}")"
[[ "${after}" == "${GOVERNED}" ]]

printf 'governed jankurai hostile identity and PATH neutralization tests ok\n'
