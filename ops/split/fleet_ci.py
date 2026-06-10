#!/usr/bin/env python3
from __future__ import annotations
import argparse
import subprocess
from pathlib import Path
try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib

parser = argparse.ArgumentParser()
parser.add_argument("--manifest", default="repos.manifest.toml")
parser.add_argument("--full", action="store_true")
args = parser.parse_args()
data = tomllib.loads(Path(args.manifest).read_text())
cmd = ["just", "check"] if args.full else ["just", "score"]
for repo in data.get("repo", []):
    path = Path(repo["path"])
    print(f"{repo['name']}: {' '.join(cmd)}")
    subprocess.run(cmd, cwd=path, check=True)
