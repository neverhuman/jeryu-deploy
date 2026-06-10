#!/usr/bin/env python3
from __future__ import annotations
import argparse
import re
from pathlib import Path
try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib

parser = argparse.ArgumentParser()
parser.add_argument("--lock", default="jeryu-split.lock.toml")
args = parser.parse_args()
data = tomllib.loads(Path(args.lock).read_text())
missing = []
for repo in data.get("repo", []):
    for field in ("name", "github_slug", "local_path", "tag", "commit", "required_check"):
        if not str(repo.get(field, "")).strip():
            missing.append(f"{repo.get('name', '<unknown>')} missing {field}")
    commit = str(repo.get("commit", ""))
    if commit not in {"PENDING", "PENDING_SELF"} and not re.fullmatch(r"[0-9a-f]{40}", commit):
        missing.append(f"{repo.get('name', '<unknown>')} commit is not a sha: {commit}")
if missing:
    raise SystemExit("\n".join(missing))
print(f"lock ok: {len(data.get('repo', []))} repos")
