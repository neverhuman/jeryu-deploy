#!/usr/bin/env bash
# test-jankurai-audit-git-auth.sh — the audit runner's clone credential: written 0600, applied by git
# only to the forge's git base (not another host a redirect could reach), never in argv, and a
# malformed token or base is refused. Local git only: no service, network or real credential.
# -h|--help prints this header and exits, before anything else runs.
case "${1:-}" in -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"; exit 0 ;; esac
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=ops/ci/jankurai-audit-git-auth.sh
source "${here}/jankurai-audit-git-auth.sh"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
pass=0
ok() { pass=$((pass + 1)); echo "ok $pass - $1"; }
fail() { echo "not ok - $1" >&2; exit 1; }
base=https://forge.invalid/git
printf 'test-token-123\n' >"$T/token"

audit_git_auth_config "$T/auth.conf" "$base" "$T/token"
[ "$(stat -c %a "$T/auth.conf")" = 600 ] || fail "auth config is not 0600"
ok "the clone credential is written 0600"

got="$(git -c include.path="$T/auth.conf" config --get-urlmatch http.extraHeader "$base/acme/private.git")"
[ "$got" = "Authorization: Bearer test-token-123" ] || fail "git does not apply the header to the forge: $got"
ok "git applies the bearer header to a repository under the forge's git base"

if git -c include.path="$T/auth.conf" config --get-urlmatch http.extraHeader https://elsewhere.invalid/x.git >/dev/null; then
  fail "the header leaks to another host"
fi
ok "another host never receives the header"

mkdir -p "$T/bin"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >>"%s/argv"\n' "$T" >"$T/bin/git"
chmod +x "$T/bin/git"
PATH="$T/bin:$PATH" git -c include.path="$T/auth.conf" clone -q "$base/acme/private.git" "$T/src"
! grep -q test-token-123 "$T/argv" || fail "the token appears in git's argv"
grep -Fq "clone -q $base/acme/private.git" "$T/argv" || fail "the stub did not see the clone"
ok "the token never appears in argv"

printf 'two words\n' >"$T/bad-token"
! audit_git_auth_config "$T/x.conf" "$base" "$T/bad-token" 2>/dev/null || fail "a malformed token was accepted"
! audit_git_auth_config "$T/y.conf" 'ftp://forge.invalid' "$T/token" 2>/dev/null || fail "a non-http base was accepted"
[ ! -e "$T/x.conf" ] && [ ! -e "$T/y.conf" ] || fail "a refused call wrote a file"
ok "a malformed token or base is refused and writes nothing"

# shellcheck disable=SC2016 # the runner's literal text
grep -Fq 'git -c include.path="${GIT_AUTH_CONFIG}" clone' "${here}/jankurai-audit-runner.sh" || fail "the runner does not clone with the credential"
grep -Fq 'ops/ci/jankurai-audit-git-auth.sh' "${here}/install-jankurai-audit-runner.sh" || fail "the installer does not ship the helper"
ok "the runner clones with it, and the installer ships it"
echo "1..$pass"
