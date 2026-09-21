# Governed Jankurai identity

Release and score lanes consume Jankurai only through `ops/ci/lib.sh`. The
release-authoritative source is the local Jeryu tag
`v1.6.11-deadlang-precision-split.3`; the installed binary must report
`jankurai 1.6.11` and match SHA-256
`9e6b8857a26f6004d4c74e510e13b06d880f2e2ae0c89502698889ed690c5d6c`.
The verifier rejects missing files, symlinks, version drift, byte substitution,
and missing or mismatched content-addressed installation receipts. It
deterministically neutralizes an earlier ambient PATH entry by prepending the
governed binary directory and then verifying the resulting resolution; it does
not claim the initial PATH was rejected. The embedded API bridge additionally
rejects multi-link files and validates the complete local source, build, and
protected jeryu-tool manifest authority before publishing a score. Verification
never installs or fetches a tool, and GitHub is neither release authority nor a
dependency of this verification path.

Under `JAIN_RELEASE_CI=1`, the root broker is the only path authority: the
verifier ignores caller binary settings, requires PATH to resolve exactly to
`/opt/jain-ci/authority/release-bin/jankurai`, requires mode `0555` with one
physical link, and rejects caller receipt or test-authority overrides. Ordinary
and image lanes remain content-addressed-receipt bound.

The former 1.6.10 score is preserved byte-identically under
`agent/baselines/historical/` as audit history. The active report and provenance
under `agent/baselines/` were generated from exact hosted protected `main` with
the governed 1.6.11 binary. The proof lane verifies their checksum, source
commit/tree, tool identity, fingerprints, score, hard findings, and caps before
using them. A topic must bind its exact protected base; a protected-main replay
accepts that report only as a strict ancestor so the receipt is not
self-referential. Those
bytes become accepted only through detached exact-head review and protected
merge; candidate output can never replace its own baseline.

