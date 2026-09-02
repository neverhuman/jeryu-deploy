set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

jobs := env_var_or_default("JERYU_CI_JOBS", "40")
test_threads := env_var_or_default("JERYU_CI_TEST_THREADS", "8")

fast:
  ./ops/ci/fast.sh # cargo check

check:
  ./ops/ci/check.sh

# Narrow deterministic loop for Deploy's owned API surface.
check-api:
  source ops/ci/hosted-git-env.sh; cargo check -p jeryu-api --locked --features web --all-targets --jobs {{jobs}}

test-api:
  source ops/ci/hosted-git-env.sh; cargo test -p jeryu-api --locked --features web --jobs {{jobs}} -- --test-threads {{test_threads}}

cache-status:
  sccache --show-stats

score:
  ./ops/ci/score.sh # jankurai audit repo-score

security:
  ./ops/ci/security.sh # gitleaks actionlint env-file locked-cargo-metadata

audit:
  ./ops/ci/audit.sh # canonical Jankurai proof plus cargo-audit/cargo-deny

artifact-support:
  ./ops/ci/artifact_support.sh

profile:
  printf '%s\n' "deploy"

# Canonical end-user binary build: stage the SPA from the sibling jeryu-web
# checkout (or keep the vendored copy), then build the fused `jeryu` binary.
build-release:
  ./scripts/stage-web-dist.sh
  source ops/ci/hosted-git-env.sh; cargo build --locked --release -p jeryu-cli --jobs {{jobs}}
