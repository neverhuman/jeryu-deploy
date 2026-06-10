#!/usr/bin/env python3
from __future__ import annotations

import argparse
import fnmatch
import json
import subprocess
from pathlib import Path
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    import tomli as tomllib  # type: ignore[no-redef]


def load(path: Path) -> dict[str, Any]:
    with path.open("rb") as fh:
        return tomllib.load(fh)


def git_files(source_root: Path, source_sha: str) -> list[str]:
    result = subprocess.run(
        ["git", "-C", str(source_root), "ls-tree", "-r", "--name-only", source_sha],
        check=True,
        text=True,
        capture_output=True,
    )
    return [line for line in result.stdout.splitlines() if line.strip()]


def covered(path: str, patterns: list[str]) -> bool:
    for pattern in patterns:
        if fnmatch.fnmatch(path, pattern):
            return True
        if pattern.endswith("/**") and path.startswith(pattern[:-3]):
            return True
        if path == pattern:
            return True
    return False


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", default="repos.manifest.toml")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    manifest_path = Path(args.manifest)
    data = load(manifest_path)
    source_root = Path(str(data["source_root"]))
    source_sha = str(data["source_sha"])

    patterns: list[str] = [str(item) for item in data.get("shared_source_paths", [])]
    for repo in data.get("repo", []):
        patterns.extend(str(item) for item in repo.get("source_paths", []))

    files = git_files(source_root, source_sha)
    missing = [path for path in files if not covered(path, patterns)]
    report = {
        "schema_version": "jeryu.split.source-coverage/v1",
        "source_root": str(source_root),
        "source_sha": source_sha,
        "tracked_files": len(files),
        "patterns": len(patterns),
        "missing_count": len(missing),
        "missing": missing,
        "status": "pass" if not missing else "fail",
    }
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    elif missing:
        print(f"source coverage failed: {len(missing)} tracked files are not assigned")
        for path in missing[:100]:
            print(path)
    else:
        print(
            f"source coverage pass: {len(files)} tracked files covered by {len(patterns)} patterns"
        )
    return 0 if not missing else 1


if __name__ == "__main__":
    raise SystemExit(main())

