set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

jobs := env_var_or_default("JERYU_CI_JOBS", "40")

fast:
  ./ops/ci/fast.sh # cargo check

check:
  ./ops/ci/check.sh

score:
  ./ops/ci/score.sh # jankurai audit repo-score

security:
  ./ops/ci/security.sh # gitleaks cargo audit npm audit syft

artifact-support:
  ./ops/ci/artifact_support.sh

profile:
  printf '%s\n' "deploy"

# Canonical end-user binary build: stage the SPA from the sibling jeryu-web
# checkout (or keep the vendored copy), then build the fused `jeryu` binary.
build-release:
  ./scripts/stage-web-dist.sh
  cargo build --release -p jeryu-cli --jobs {{jobs}}
