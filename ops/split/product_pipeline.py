#!/usr/bin/env python3
from __future__ import annotations
import subprocess

steps = [
    ["./ops/split/manifest.sh", "--check-paths"],
    ["./ops/split/source_coverage.py"],
    ["./ops/split/verify-lock.py"],
]
for step in steps:
    print("+", " ".join(step))
    subprocess.run(step, check=True)
print("product pipeline bootstrap ok")
