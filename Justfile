set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

jobs := env_var_or_default("JERYU_CI_JOBS", "40")

fast:
  ./ops/ci/fast.sh

check:
  ./ops/ci/check.sh

score:
  ./ops/ci/score.sh

security:
  ./ops/ci/security.sh

artifact-support:
  ./ops/ci/artifact_support.sh

profile:
  printf '%s\n' "deploy"
