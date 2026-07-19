# jeryu-deploy

Integration, end-user binary build, split lock, and release bundle logic.

This repository was seeded from Jeryu source commit `cbecf7caa0e932c76a341b2521e66e911233860d` by
`ops/split/materialize.py`. It is part of the seven-repo Jeryu split family and keeps source
paths stable where practical so ownership remains auditable.

## Agent Navigation

Read `AGENTS.md` before changing this repository; `CLAUDE.md` points to the same
authority. Durable detail is intentionally routed rather than duplicated:

- Architecture and trust boundaries: `docs/architecture.md` and
  `docs/boundaries.md`.
- Tests and required proof: `docs/testing.md`, `agent/test-map.json`, and
  `agent/proof-lanes.toml`.
- Ownership and generated files: `agent/owner-map.json`,
  `agent/generated-zones.toml`, and `docs/generated-zones.md`.
- Audit and release controls: `agent/audit-policy.toml`, `docs/audit-rubric.md`,
  `docs/release.md`, and `docs/release-process.md`.

## Owned Cargo Packages

- `crates/jeryu-api`
- `crates/jeryu-cli`

## Source Coverage

- `crates/jeryu-api/**`
- `crates/jeryu-cli/**`
- `.github/**`
- `ci-fast-push.sh`
- `ops/**`
- `scripts/**`
- `tests/**`
- `examples/**`
- `config/**`
- `configs/**`
- `policies/**`
- `images/**`
- `docs/**`
- `agent/**`
- `Cargo.toml`
- `Cargo.lock`
- `Justfile`

## Local Commands

- `just fast`
- `just check`
- `just score`
- `just security`
- `just artifact-support`

## Quick Start

Prerequisites are Rust 1.95 and the governed Jankurai binary described in
`docs/release.md#governed-jankurai-identity`. From a canonical checkout:

```bash
bash ops/ci/ensure-jankurai.sh
just fast
```

The verifier is read-only and never installs a tool. `just fast` is the
deterministic affected lane; use `just check`, then `just security`, before
requesting protected review. Test ownership and narrower reruns are mapped in
`agent/test-map.json` and `docs/testing.md`.

## Status

Protected local Jeryu checks are release authority. This successor is not a
release: its 1.6.11 score is candidate evidence, and the active ratchet remains
closed until protected-main evidence receives detached review. Current lane
artifacts are written under `.jankurai/` and `target/jankurai/`; neither is a
source-of-truth badge or permission to publish.
