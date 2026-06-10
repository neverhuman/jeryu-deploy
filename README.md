# jeryu-deploy

Integration, end-user binary build, split lock, and release bundle logic.

This repository was seeded from Jeryu source commit `cbecf7caa0e932c76a341b2521e66e911233860d` by
`ops/split/materialize.py`. It is part of the seven-repo Jeryu split family and keeps source
paths stable where practical so ownership remains auditable.

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
