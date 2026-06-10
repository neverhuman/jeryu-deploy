#!/usr/bin/env bash
set -euo pipefail
required=(
  agent/owner-map.json
  agent/test-map.json
  agent/generated-zones.toml
  agent/proof-lanes.toml
  agent/audit-policy.toml
  agent/boundaries.toml
  agent/JANKURAI_STANDARD.md
)
for path in "${required[@]}"; do
  [[ -s "$path" ]] || { printf 'missing split metadata: %s\n' "$path" >&2; exit 1; }
done
mkdir -p target/jankurai
python3 - <<'PY'
import json
from pathlib import Path
report = {
    "schema_version": "jeryu.split.bootstrap-score/v1",
    "status": "pending-real-jankurai-score",
    "score": None,
    "hard_findings": None,
    "reason": "Run the pinned Jankurai lane after split remotes/tags are available.",
}
Path("target/jankurai/bootstrap-score.json").write_text(json.dumps(report, indent=2) + "\n")
PY
printf 'score bootstrap ok; real Jankurai score is pending\n'
