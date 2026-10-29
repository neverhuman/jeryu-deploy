# jeryu-deploy

[![Required checks: inspect the exact head](https://img.shields.io/badge/required-check%20exact%20head-lightgrey)](https://git.jeryu.ai/repos/jeryu/jeryu/jeryu-deploy/pulls)

This navigation badge links to live forge checks; it does not assert a green
head, a Jankurai score, or release readiness.

Integration, end-user binary build, split lock, and release bundle logic.

This repository was seeded from Jeryu source commit `cbecf7caa0e932c76a341b2521e66e911233860d`.
It is part of the Jeryu split family and keeps source paths stable where practical so ownership
remains auditable.

## Agent Navigation

Read `AGENTS.md` before changing this repository; `CLAUDE.md` points to the same
authority. Durable detail is intentionally routed rather than duplicated:

- Architecture and trust boundaries: `docs/architecture.md` and
  `docs/boundaries.md`.
- Decisions that outlive the work that produced them, with their context and
  consequences: `docs/adr/` (`docs/adr/README.md` explains the shape, and
  cross-cutting family decisions live here rather than in a single repository).
- Proposed Shiftwork development and final review gates:
  [ADR 0014](docs/adr/0014-separate-development-completion-from-final-review.md)
  and the [implementation plan](docs/shiftwork-gates-plan.md).
- Tests and required proof: `docs/testing.md`, `agent/test-map.json`, and
  `agent/proof-lanes.toml`.
- Ownership and generated files: `agent/owner-map.json`,
  `agent/generated-zones.toml`, and `docs/generated-zones.md`.
- Audit and release controls: `agent/audit-policy.toml`, `docs/audit-rubric.md`,
  and `scripts/release/README.md` (the release procedure; run it on the release host only).
- Running a forge: every environment variable it reads, with defaults and
  formats, in `configs/jeryu.env.example`; how to load it, the minimal and
  production sets, and how to check them in `docs/configuration.md`.
- Tool-finder scheduled scan and proposal decisions: `docs/tool-finder.md`.
- Bots, person-owned and team bots (enroll, reach, grants, setup, edge): `docs/bots.md`.
- Organizations, owners and teams: `docs/organizations.md`.
- Page to hand an external agent so it connects itself (MCP, REST, git):
  `docs/connect-an-agent.md`.
- Pipeline event log, attention inbox and their agent guide:
  `docs/pipeline-events.md`.
- Product-wide search (`GET /api/v1/search`), what it looks in and what it
  deliberately does not: `docs/search.md`.
- Pull request create and review-base behavior: `docs/pull-request-bases.md`.
- Paging every list route, on both the `/api/v1` and `/api/v3` edges:
  `docs/pagination.md`.
- Flow, the delivery line as stations (where work sits, waits and loops back,
  and the runners behind each station), design and build order: `docs/flow.md`
  (decision: `docs/adr/0004-flow-shows-where-work-sits-and-backs-up.md`).
- Agent presence, every coding agent working on a repository (reported
  sessions, agent runs, gate runners, reviewers, queue workers):
  `docs/agent-presence.md`
  (decision: `docs/adr/0006-agent-presence-on-the-repo-agents-tab.md`).

Governed Jankurai rotations run the closed projection integration test, the
hostile-identity shell verifier, full score, and protected-base diff audit
listed in `docs/testing.md`. The monorepo-only mapcheck marker lane is not a
standalone Deploy proof.

## Owned Cargo Packages

- `crates/jeryu-api`
- `crates/jeryu-cli`
- `crates/jeryu-split-tool`

## Source Coverage

- `crates/jeryu-api/**`
- `crates/jeryu-cli/**`
- `crates/jeryu-split-tool/**`
- `.github/**`
- `ci-fast-push.sh`
- `ops/**`
- `scripts/**`
- `tools/**`
- `tests/**`
- `examples/**`
- `extensions/**`
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

The repository-owned [gh jeryu extension](extensions/gh-jeryu/README.md) reads
native PR status and waits for a selected head with an explicit target and timeout.

Run the gate locally with one command:

```bash
bash ops/ci/pr-ci.sh
```

This is the same entry point the protected `jeryu-deploy/required` check runs.
`just gate`, `just required` and `scripts/ci-local.sh` are aliases for it.
Set `JERYU_TOOL_RENDER` to the executable `ops/render-tool-manifest.sh` in a
trusted jeryu-tool checkout outside this worktree when the default sibling path
is unavailable. The web status proof also needs a Git read credential or
`JERYU_WEB_PIN_TOKEN_FILE` (a regular mode-0600 file). Prerequisites are checked
before builds. Each run prints its
retained `target/evidence/required/run.*` directory, containing readable stage
logs, `stages.jsonl` and an atomically replaced `receipt.json`. Receipts include
source head/dirtiness, recipe hashes, command identifiers, exit codes and elapsed
seconds. A running or incomplete receipt is never success; these diagnostics
are not merge authority. Each run keeps the newest `JERYU_GATE_EVIDENCE_KEEP`
(20) finalized run directories and discards a run that never finalized once it
is older than `JERYU_GATE_EVIDENCE_STALE_DAYS` (7), so a crashed run's receipt
survives long enough to read. Keep this evidence when cleaning build outputs; the
normal `lanes sweep --apply` archives report/log files under `target/` before
removing a lane. Do not remove that directory by hand before preserving it.

The commands below are narrower loops for iteration, not the gate:

- `just fast`
- `just check`
- `just check-api`
- `just test-api`
- `just agent-runs`
- `just request-id`
- `just cache-status`
- `just score`
- `just security`
- `just security-network`
- `just artifact-support`

To release, follow [`scripts/release/README.md`](scripts/release/README.md) on
the release host: `stage-release.sh` stages, `deploy-release.sh` switches.

Rust-native split-family checks are available through
`cargo run --locked --offline -p jeryu-split-tool --bin jeryu-split --
<manifest|manifest-path|fleet-ci|verify-lock|product-pipeline>`. The family's
membership and release identity live in one authority manifest published by
`jeryu-release-ops`; this repository keeps no copy of it. The commands read it
from `JERYU_FAMILY_MANIFEST`, else from a `jeryu-release-ops` checkout beside
this one, and `manifest-path` prints where it was found. The
`ops/split/manifest.sh` compatibility entrypoint delegates to that binary and
does not invoke a Python runtime.

Repository CI wrappers route their lock-bound release-toolkit checks through
the same local binary with `ci-lanes-check`, `ci-lanes-list`, and
`affected-plan`; they do not discover or execute source from a Cargo cache or
sibling checkout. Retired monorepo-only repository gates are rejected rather
than being applied to this standalone split.

`bash ops/ci/full.sh` and `bash ci-fast-push.sh --full --no-push` retain the
complete workspace test matrix while composing only repository-owned proof,
workflow-parity, release-receipt-contract, score, security, and doctor gates.

## Quick Start

Prerequisites are Rust 1.95 and the governed Jankurai binary described in
`docs/governed-jankurai.md`. From a canonical checkout:

```bash
bash ops/ci/ensure-jankurai.sh
just fast
```

The verifier is read-only and never installs a tool. `just fast` is the
deterministic affected lane; run `bash ops/ci/pr-ci.sh` (the gate) before
requesting protected review. Test ownership and narrower reruns are mapped in
`agent/test-map.json` and `docs/testing.md`.

## Status

The protected repository on `git.neverhuman.org` is source and ref authority;
local checks are developer evidence, not permission to merge. Until the hosted
runner posts the protected required context, a topic remains unmergeable even
when its push-time Jankurai proof is green. This successor is not a release.
Its ratchet baseline was generated from exact hosted protected `main` with the
governed 1.6.11 binary; the baseline and every candidate result still require
detached exact-head review before protected merge. Current lane artifacts are
written under `.jankurai/` and `target/jankurai/`; neither is a source-of-truth
badge or permission to publish.

Historical Cargo source spellings remain unchanged to preserve crate identity.
`.cargo/hosted-gitconfig` maps only the exact dependency repositories to their
hosted Jeryu URLs and uses the dedicated hosted credential helper without
including ambient user configuration. It also carries the one exact host-scoped
smart-HTTP setting required by the currently deployed server. CI sources
`ops/ci/hosted-git-env.sh` before Cargo (Cargo's
own fetch does not consume its `[env]` table). Because the hosted backend
rejects Cargo's tag-only exact-object fetch, `.cargo/hosted-pin-refs.tsv` also
binds every immutable tag target to an advertised hosted preservation ref; the
refs contain no bytes beyond their existing release tags. Finally,
`bash ops/ci/dependency-sources.sh` rejects an unlisted source, mutable pin,
missing mapping/ref, or non-hosted effective destination.

## Active-active PR gate hosts

Gate hosts gate every repository they can reach, at the same time. The forge
holds the claim on each gated head, so only one host ever posts a head's
`<repo>/required`, and a host that dies loses its heads to a live one without
an operator moving anything.

- `POST /api/v1/gate-claims {repo, number, head_sha|queue_sha, runner_id,
  host}` takes an exclusive lease on one head for 180 seconds, answering `201`
  with `ttl_seconds`. The same ask repeated by the holder is the renewal: it
  keeps `granted_at` and moves `expires_at`, and the runner sends it every
  third of the TTL while the gate runs. Another runner asking for a head that
  is leased is refused `409 claim_held`.
- `POST /api/v1/gate-claims/release` gives a head back on publication, so the
  next host does not wait out the TTL. Only the holder may release a live
  lease; releasing one the forge has already forgotten answers `200 absent`,
  which is what a publication retry does. A lease nobody renews expires, and
  the head becomes claimable by any live host.
- Both routes need the credential heartbeats need (a `JERYU_RUNNER_REPORTERS`
  login or a forge admin), so an ordinary account can neither park a lease on
  a head nor clear one. A repeat carrying an `Idempotency-Key` replays the
  first answer, like every other `POST` on `/api/v1`.
- The leases are in memory, like the heartbeats they are matched against: a
  restart loses them and the first host to ask takes the head again.

Capability matching replaces the hand-maintained owner partition. A repository
declares what a gate of it needs in its own `.jeryu/repo.toml`:

```toml
[gate]
capabilities = ["host-ci"]
```

The file is read at the gated head's base branch -- the default branch for a
head with no pull request -- and never at the pull request's own head, so a
pull request cannot change what its own gate requires. `host-ci` is the only
recognized capability; a file that names an unknown or repeated one, or that
cannot be read as a policy, refuses every claim (`422 invalid_repo_config`)
rather than quietly requiring nothing. A host advertises what it has in its
heartbeat's optional `capabilities` array (validated the same way, shown per
node by `/api/v1/control-plane/runners`, and ignorable by an older forge),
and a claim is granted only to a host whose advertised set covers the
repository's required set; otherwise `409 capability_missing` names what is
missing.

needs-you raises `gate_claim_stale` for a head whose lease expired while no
live gate host advertises every capability its repository requires: nothing
will pick that head up, and `gate_runner_down` stays quiet because other hosts
are beating. An expired lease any live host could take is not a row.

`veox/jain`, whose gate needs the host-ci sandbox, is not one of this
repository's files; its `.jeryu/repo.toml` has to be committed there.
