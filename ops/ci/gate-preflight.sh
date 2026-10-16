#!/usr/bin/env bash
# Offline prerequisite checks for the required gate.
gate_preflight() {
  : "${repo_root:?caller must set repo_root}"
  local tool repair
  for tool in bash git cargo rustc jq realpath sha256sum awk sed stat curl gitleaks actionlint cargo-audit cargo-deny; do
    case "$tool" in
      cargo|rustc) repair='install the repository toolchain with rustup toolchain install (see rust-toolchain.toml)' ;;
      cargo-audit|cargo-deny) repair="cargo install --locked $tool" ;;
      gitleaks|actionlint) repair='provision the pinned tool version from tools/security-lane.sh on this gate host' ;;
      realpath|sha256sum|stat) repair='install coreutils using your system package manager' ;;
      *) repair="install $tool using your system package manager" ;;
    esac
    command -v "$tool" >/dev/null || {
      echo "missing prerequisite: $tool; repair: $repair" >&2
      return 1
    }
  done
  if [ -z "${JERYU_TOOL_RENDER+x}" ]; then
    JERYU_TOOL_RENDER="$repo_root/../jeryu-tool/ops/render-tool-manifest.sh"
  fi
  if [ -z "$JERYU_TOOL_RENDER" ] || [ ! -x "$JERYU_TOOL_RENDER" ]; then
    echo "missing/non-executable prerequisite renderer: $JERYU_TOOL_RENDER; set JERYU_TOOL_RENDER to an executable jeryu-tool/ops/render-tool-manifest.sh outside this checkout (chmod +x that path if needed)" >&2
    return 1
  fi
  render_real="$(realpath "$JERYU_TOOL_RENDER")"
  case "$render_real/" in
    "$(realpath "$repo_root")/"*)
      echo "refusing prerequisite renderer inside checkout: $JERYU_TOOL_RENDER; set JERYU_TOOL_RENDER to jeryu-tool/ops/render-tool-manifest.sh outside this checkout" >&2
      return 1 ;;
  esac
  cargo fmt --version || {
    echo 'missing prerequisite rustfmt; repair: rustup component add rustfmt' >&2
    return 1
  }
  cargo clippy --version || {
    echo 'missing prerequisite clippy; repair: rustup component add clippy' >&2
    return 1
  }
  gate_web_credential_preflight
}

gate_auditor_preflight() {
  echo 'checking prerequisite governed Jankurai; repair installation/receipt with ops/install-jankurai.sh from protected jeryu-tool main (see docs/governed-jankurai.md)' >&2
  require_jankurai
}

# Match web-pin-proof.sh without a network request or persisting credentials.
gate_web_credential_preflight() {
  set +x
  local credential token token_file="${JERYU_WEB_PIN_TOKEN_FILE:-}"
  local repair='set JERYU_WEB_PIN_TOKEN_FILE to a regular mode-0600 file containing a forge PAT for reading jeryu-web commit status, or configure a Git credential helper for https://git.neverhuman.org/git/jeryu/jeryu-web.git'
  if [[ -n "$token_file" ]]; then
    if [[ ! -f "$token_file" || -L "$token_file" || "$(stat -c '%a' "$token_file")" != 600 ]]; then
      echo "missing prerequisite: web status credential file must be regular mode-0600; repair: $repair" >&2
      return 1
    fi
    token="$(cat "$token_file")"
  else
    if ! credential="$(printf 'protocol=https\nhost=git.neverhuman.org\npath=git/jeryu/jeryu-web.git\n\n' |
      GIT_TERMINAL_PROMPT=0 git credential fill 2>/dev/null)"; then
      echo "missing prerequisite: web status read credential; repair: $repair" >&2
      return 1
    fi
    token="$(sed -n 's/^password=//p' <<< "$credential")"
  fi
  if [[ ! "$token" =~ ^[A-Za-z0-9._~+/-]+=*$ ]]; then
    echo "missing prerequisite: valid web status read credential; repair: $repair" >&2
    return 1
  fi
}
