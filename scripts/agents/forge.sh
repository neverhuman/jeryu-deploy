#!/bin/sh
# Phase-0 agent entrypoint; credentials are read only by the Rust helper.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
exec cargo run --quiet --locked --offline --manifest-path "$root/Cargo.toml" \
    -p jeryu-cli --bin jeryu-agent-forge -- "$@"
