# shellcheck shell=bash
# jankurai-audit-git-auth.sh — sourced by jankurai-audit-runner.sh: let `git clone` read private
# repositories with the runner's own token.
#
# The token goes into a 0600 git config file that is included only for the clone
# (`git -c include.path=<file>`), so it never appears in argv or the environment, and the header is
# scoped to the forge's git base, so a redirect to another host does not receive it.

# audit_git_auth_config FILE GIT_BASE TOKEN_FILE: write FILE; fails on a malformed token or base.
audit_git_auth_config() {
  local file="$1" base="$2" token_file="$3" token
  [[ "${base}" =~ ^https?://[^[:space:]\"]+$ ]] || { echo "git base must be an http(s) URL: ${base}" >&2; return 1; }
  token="$(cat "${token_file}")" || return 1
  [[ "${token}" =~ ^[A-Za-z0-9._~+/-]+=*$ ]] || { echo "token file must contain one nonempty bearer value" >&2; return 1; }
  ( umask 077; printf '[http "%s/"]\n\textraHeader = Authorization: Bearer %s\n' "${base%/}" "${token}" > "${file}" )
}
