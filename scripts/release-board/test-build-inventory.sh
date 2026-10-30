#!/usr/bin/env bash
# A networkless check of the optional build-report collector boundary.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
mkdir -p "$repo/target/build-inventory-fixtures"
fixture="$(mktemp -d "$repo/target/build-inventory-fixtures/run.XXXXXXXX")"
export JERYU_RELEASE_BOARD_STATE="$fixture/state"
# shellcheck source=scripts/release-board/lib.sh
source "$here/lib.sh"
problems_file="$fixture/problems.jsonl"
inventory_file="$fixture/inventory.json"
: >"$problems_file"
printf '%s\n' '{"observed_at":"2026-10-30T12:00:00Z","builds":[]}' >"$fixture/report.json"
build_inventory "$fixture/report.json"
cmp "$fixture/report.json" "$inventory_file"
printf '%s\n' 'ok - normalized build report is attached byte for byte'
printf '%s\n' '{"builds":"broken"}' >"$fixture/malformed.json"
if build_inventory "$fixture/malformed.json"; then exit 1; fi
cmp "$fixture/report.json" "$inventory_file"
printf '%s\n' 'ok - malformed report is refused without replacing the accepted report'
ln -s "$fixture/report.json" "$fixture/linked.json"
if build_inventory "$fixture/linked.json"; then exit 1; fi
if build_inventory "$fixture/missing.json"; then exit 1; fi
[[ "$(jq -s length "$problems_file")" == 3 ]]
printf '%s\n' 'ok - linked or missing reports become source problems'
printf 'retained evidence: %s\n' "$fixture"
