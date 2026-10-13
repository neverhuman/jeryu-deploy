# Pin-bump verification

The standalone `scripts/release/verify-pin-bump.sh [--json] BASE HEAD` verifier checks a mechanical jeryu-web lock bump: exactly the pin lines, ancestry on web main, its required status, and a reproducible bundle from the base recipe. It is a diagnostic, default off unless `JERYU_PIN_FAST_LANE=1` is set for that command.

`ops/ci/pr-ci.sh` always runs the full required gate, including formatting, clippy, workspace tests, the web bundle, Jankurai and security. No environment flag or verifier result skips those lanes. The proposed early exit was withdrawn after review: trusting a caller-selected base to choose executable verifier code and accepting an empty verifier stream were insufficient boundaries for a required check.

The verifier never approves, merges, writes forge state or supplies release authority. `BASE` must be chosen by the operator from protected main; an eligibility result alone is no permission to omit required checks or review. Automated short gates and mechanical approvals require a separate reviewed design.

The release-script test suite checks default-off behavior, exact lock shape, ancestry, status, reproducibility and credential destination/custody refusals.
