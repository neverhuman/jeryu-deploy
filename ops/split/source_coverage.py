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


def run_git_tree(command: list[str], source_sha: str, reader: str) -> list[str]:
    try:
        result = subprocess.run(
            [*command, "ls-tree", "-r", "--name-only", source_sha],
            check=True,
            text=True,
            capture_output=True,
        )
    except subprocess.CalledProcessError as exc:
        detail = exc.stderr.strip() or exc.stdout.strip() or str(exc)
        raise SystemExit(f"failed to read source tree from {reader}: {detail}") from exc
    return [line for line in result.stdout.splitlines() if line.strip()]


def git_files(
    source_root: Path, source_sha: str, source_git_dir: Path | None
) -> tuple[list[str], str]:
    if source_root.exists():
        return (
            run_git_tree(["git", "-C", str(source_root)], source_sha, str(source_root)),
            str(source_root),
        )
    if source_git_dir is not None:
        return (
            run_git_tree(
                ["git", "--git-dir", str(source_git_dir)],
                source_sha,
                str(source_git_dir),
            ),
            str(source_git_dir),
        )
    return (
        run_git_tree(
            ["git", "-C", str(source_root)],
            source_sha,
            f"{source_root} (missing)",
        ),
        str(source_root),
    )


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
    source_git_dir = (
        Path(str(data["source_git_dir"])) if data.get("source_git_dir") else None
    )

    patterns: list[str] = [str(item) for item in data.get("shared_source_paths", [])]
    for repo in data.get("repo", []):
        patterns.extend(str(item) for item in repo.get("source_paths", []))

    files, source_reader = git_files(source_root, source_sha, source_git_dir)
    missing = [path for path in files if not covered(path, patterns)]
    report = {
        "schema_version": "jeryu.split.source-coverage/v1",
        "source_root": str(source_root),
        "source_git_dir": str(source_git_dir) if source_git_dir else None,
        "source_reader": source_reader,
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
