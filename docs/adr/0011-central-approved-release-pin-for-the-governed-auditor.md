# 0011. Pin the governed Jankurai auditor centrally by approved release

Status: Proposed
Date: 2026-10-14
Supersedes: none
Superseded-by: none

## Context

The governed Jankurai auditor is a key-signed public release named in jeryu-tool's `tool-manifest.toml`. That file is meant to be the single source of truth, but every release value (version, tag, asset, binary and provenance digests) is copied out of it into consumer source. A patch bump today means a jeryu-tool pin PR, a render PR in every claimed consumer, a forge rebuild (the accepted identity is a set of Rust constants plus an `include_str!` of the sandbox receipt), a sandbox image rebuild, and hand edits in repositories the renderer does not reach. The 1.7.4 render alone touches 13 files in jeryu-deploy; the appendix lists every copy.

Four failures follow from copying:

- **Bump deadlock.** A pin PR must pass gates that verify the gate host's installed auditor, and hosts install a pin only after it reaches protected main. jeryu-tool#25 kept per-release values out of the consumer template for this reason and pinned only the trust anchors (release repository, signer key digest, cosign digest, target), which do not change between releases.
- **Fleet-wide red between steps.** The forge refuses (422) any report whose version or digest differs from its compiled constants, so between host install, consumer render and forge deploy some set of repositories is always red.
- **Local runs depend on the host.** A local `required` run uses whatever single auditor the machine's installer last placed, so a developer and CI can run different releases with nothing saying so.
- **Qualification was a single full audit.** A release that passed a full audit regressed `diff-audit` and blocked gated repositories. No step ran the candidate under both modes before it governed real heads.

The owner approved the direction on 2026-10-14: a compatible auditor patch needs one manifest PR in jeryu-tool plus automated rollout, with no consumer source edits and no forge rebuild. Scoring rules, floors, caps and review obligations are not changed by this record.

## Decision

### Decisions

1. **Scope.** Each Jeryu installation has one centrally approved Jankurai release manifest, and jeryu-tool's `tool-manifest.toml` on protected main is its authority. Every enrolled repository's CI audit uses it. Repositories keep rules, exceptions and capability or schema requirements, never patch pins.
2. **Membership authorizes, signatures do not.** The manifest carries an `approved` table. A release can produce governed evidence only while it is in that table; a valid signature from the pinned key is necessary but never sufficient. Each entry is `candidate`, `active` or `retired`, and exactly one entry is `active`.
3. **The forge reads the table as configuration.** It loads an immutable snapshot from jeryu-tool protected main through its own readback, keyed by manifest commit and digest. It never takes authority from a PR head, a runner, a request body or a compiled constant. An invalid snapshot fails closed: the last valid snapshot stays in force and the failure is visible; with no valid snapshot, jobs wait in `waiting-for-authority` and the ingest refuses every report.
4. **Jobs carry their auditor.** At dispatch each job captures head, base, policy digest, the manifest revision and digest, and the exact auditor identity. Reports are validated against that binding. In-flight jobs finish on their snapshot, stored evidence is never reinterpreted, and the artifacts needed to reproduce or roll back are retained.
5. **Runners advertise what they can run.** The gate-runner session's proposal is adopted: claim by installed digest, verify before checkout, refuse on mismatch, hold no release constants.
6. **Local runs resolve the same way, and never guess.** The local client resolves the installation, fetches and verifies its manifest, and runs the exact assigned auditor from a verified cache. It never falls back to a binary on `PATH` or to the latest upstream release.
7. **A local pass is developer feedback.** Only trusted CI evidence (a runner identity submitting against a forge job) governs merge.
8. **Jankurai selection is separate from Jeryu compatibility.** The manifest pins Jankurai, not Jeryu. A stable Jeryu client negotiates the manifest schema and capabilities with the forge and needs no upgrade per Jankurai patch.
9. **Policy names compatibility; images and hosts follow the manifest.** `required_tool_version` becomes a compatibility range, the renderer stops writing per-release values, hosts install side by side by digest, and sandbox images rebuild from the manifest.
10. **Qualification precedes activation.** A candidate runs full and diff parity, then host and image readiness, then an activation PR flips `active` for new jobs. The activation PR is audited by the outgoing release, which removes the bump deadlock.

### What exists today and what is proposed

| Piece | State |
|---|---|
| `jeryu` CLI (`crates/jeryu-cli`), server chosen by `--api-url` / `JERYU_API_URL` | implemented; it has no auditor resolution and no version negotiation |
| `GET /api/v1/capabilities` discovery document (`api_version`, routes) | implemented and deployed; the `jankurai_auditors` field below is proposed. `/.jeryu/capabilities` is not served by the deployed forge (404) and is not relied on |
| `GET /api/v1/jankurai/scores/:score_id`, `POST /api/v1/repos/:id/jankurai-scores`, `POST /api/v1/jankurai-audits/claim` | implemented; the binding fields below are proposed additions |
| Consumer wrapper `require_jankurai` with pinned trust anchors (jeryu-tool#25), host installer and authority stamp | implemented; single release slot |
| Approved table, forge snapshot, `GET /api/v1/jankurai/auditors`, job binding (`metadata_commit`, `policy_sha256`, `origin`, `purpose`), capability claim, `POST /api/v1/jankurai-audits/:job_id/release`, local resolution, digest cache, modes | proposed |

No new subcommand name is fixed here. The implementing PR adds one local entry point to the existing `jeryu` CLI and documents it.

### Contract: manifest (jeryu-tool `tool-manifest.toml`, schema 2)

```toml
schema_version = "2"
[jankurai.trust]            # changes only on key, cosign or release-repo rotation
release_repo = "<public release repository>"
release_signer_sha256 = "<64 hex>"
cosign_sha256 = "<64 hex>"

[[jankurai.approved]]
version = "1.7.4"               # `jankurai --version` prints "jankurai 1.7.4"
tag = "v1.7.4"
state = "active"                # candidate | active | retired
source_commit = "<40 hex>"
signing_identity = "<signer sha256>"
compat = { report_schema = "<id>", policy_schema = "<id>", modes = ["full", "diff"] }
qualification = "<qualification record id; required before active>"
artifacts = [
  { target = "x86_64-unknown-linux-gnu", asset = "jankurai-1.7.4-x86_64-unknown-linux-gnu.tar.gz",
    asset_sha256 = "<64 hex>", binary_sha256 = "<64 hex>", provenance_sha256 = "<64 hex>" },
]
```

Rules, checked by jeryu-tool and again by the forge loader: exactly one `active`; every `binary_sha256` unique; entries are retired, never deleted, so every historical binding stays resolvable; `signing_identity` equals the trust signer unless a rotation is recorded; `active` requires a passed qualification for each listed artifact.

### Contract: forge snapshot and job binding

- **Snapshot.** Read at jeryu-tool protected `main` from the forge's own store after the branch-protection readback, on startup, on each push to that ref and periodically. Key `{manifest_revision, manifest_sha256}` (sha256 of the file bytes). Snapshots are immutable and kept forever. Each transition (candidate added, activated, retired, rolled back) and each rejected snapshot is appended to a forge table that is never updated or deleted.
- **Route (proposed).** `GET /api/v1/jankurai/auditors` returns `snapshot`, `active`, `staged`, `history` and `rejected_snapshots`, plus the raw manifest bytes so a client can recompute `manifest_sha256`. `?snapshot=<manifest_sha256>` returns any past snapshot (for reproduce mode).
Everything in this section is proposed unless it names something that exists today.

- **Binding.** A ticket gains the following fields. The forge fixes them at dispatch and the runner never recomputes them:

```json
{"job_id": "…", "purpose": "proof", "origin": "internal", "head_sha": "<40 hex>", "base_sha": "<40 hex>", "audit_mode": "diff",
 "metadata_commit": "<40 hex>", "policy_sha256": "<64 hex>",
 "auditor": {"manifest_revision": "<40 hex>", "manifest_sha256": "<64 hex>", "version": "1.7.4", "target": "x86_64-unknown-linux-gnu",
             "binary_sha256": "<64 hex>", "signing_identity": "<64 hex>"}}
```

- **Metadata.** The forge chooses `metadata_commit` by the runner's existing rule:
  - the base, when main reaches it;
  - a job for main's own head uses that head;
  - otherwise main's tip.

  A head's own `agent/` and `.jeryu/` are never trusted. `policy_sha256` is the sha256 of `agent/audit-policy.toml` at `metadata_commit`. When that commit has no regular policy file, it is the sha256 of the fixed default below, whose bytes carry no auditor version (today's default in pr-gate writes the verified auditor's version and so changes with every release). The runner checks out metadata from exactly `metadata_commit` and verifies the digest. The report-level `jeryu_audit_policy` that pr-gate 2.1.0 adds goes away once the binding carries both fields.

```toml
schema_version = "1.0.0"
workspace = "unconfigured"
minimum_score = 85
hard_findings_allowed = 0
required_tool = "jankurai"
```

- **No base.** A `full` job keeps today's `base_sha`, git's empty tree `4b825dc642cb6eb9a060e54bf8d69288fbee4904`, as the "no base" marker. Runner and ingest both treat it as a marker and never as a diff argument.
- **Submission.** The submission echoes `job_id`, `auditor`, `metadata_commit`, `policy_sha256`, head and base, and adds `receipt_sha256`. Any difference is refused with 422 `auditor_mismatch` and recorded as a rejected publication naming the field. Score rows and `GET /api/v1/jankurai/scores/:score_id` keep and show the binding.
- **Qualification.** A `qualification` job submits to the same ingest with its `job_id`. It is recorded as qualification evidence for the candidate, never as a score or a `jankurai/proof` check. The ingest answers 202 with `{"job_id": "…", "recorded_as": "qualification", "qualification": "<record id>"}`.
- **Runner claim.** This is the gate-runner session's proposal. The claim adds `"capabilities": {"jankurai": ["<binary_sha256>", …], "sandbox": false}`, and the runner receives only jobs whose `binary_sha256` it lists. A `contributed` job goes only to a runner with `sandbox: true`. Jobs no runner can serve are visible as `waiting-for-tool` or `waiting-for-sandbox`. The heartbeat's `tools` field lists every staged digest.
- **Verify and release.** The pin-free verifier checks the binary under `<tools>/jankurai/<binary_sha256>/` and its receipt against `auditor` before checkout, and again right before running. `<tools>` is a host setting, not a repository path. A job the runner will not run is handed back at once through `POST /api/v1/jankurai-audits/:job_id/release {"reason": …}`, never by letting the lease lapse silently. The reasons are:
  - `verifier-mismatch`
  - `staged-digest-missing`
  - `not-served-by-this-host`
  - `not-on-main`
  - `contributed`
  - `tool-gone` (the binary vanished after claim)

  A released job is never scored, and it stays visible with its reason.
- **States.** `/runners` shows `waiting-for-tool`, `waiting-for-sandbox`, `running` and `publishing`, plus the conclusions `released` and `rejected-publication`, each with its reason.

### Contract: CI and local resolution sequence

| Step | CI (gate runner) | Local (`jeryu` CLI, proposed entry point) |
|---|---|---|
| 1. Installation | Fixed by the runner's enrolment | From explicit repo or client config, else the repository's origin remote. Ambiguity (several remotes, a mirror origin, config disagreeing with origin) is an error that asks for an explicit selection. It never guesses. |
| 2. Compatibility | Runner and forge share a release line | Read `/api/v1/capabilities`; pick a common manifest schema or stop (see below) |
| 3. Manifest | The job's `auditor` block from the claim | `GET /api/v1/jankurai/auditors` (normal) or `?snapshot=` (reproduce), authenticated as the user; recompute `manifest_sha256`; check trust anchors |
| 4. Artifact | `binary_sha256` from the job | The entry's artifact for the local target; no artifact for the target is an explicit error |
| 5. Install | Host staging (below) | Verified cache hit, or download from the release repo, verify signature, provenance and digests, then cache |
| 6. Run | Wrapper verifies the assigned identity, runs repo policy | Same wrapper, same policy |
| 7. Result | Submitted with the binding; governs merge | Printed with release, `manifest_sha256`, mode and freshness; never submitted |

Fresh CI and local runs in normal mode select the same `active` entry from the same snapshot. On a different platform the binary digest differs, and the result names it.

### Contract: normal, reproduce and offline modes

- **normal**: the installation's current active entry, read online.
- **reproduce**: the immutable manifest snapshot and auditor bound to a named historical job (from its score), run against that job's head, base and policy digest. A retired entry is allowed here and only here.
- **offline**: only a manifest and artifact already verified into the cache. No network. Output says `freshness: unknown` and names the cached `manifest_sha256`; it never claims to be the current active release. With no verified cache entry it stops.

### Contract: trust and cache

- **Trust roots.** The signer digest and cosign digest come from the repository's rendered wrapper (implemented in jeryu-tool#25) or from the client's recorded installation. A manifest whose trust section disagrees is refused as an unacknowledged rotation. The forge connection is TLS plus the user's existing token. The manifest digest binds what was served, and table membership is the approval.
- **Artifact checks.** Every install verifies the cosign signature offline with the pinned cosign, then asset, provenance and binary digests against the entry. A tampered, unsigned or unlisted artifact is refused and never cached.
- **Cache.** `<cache>/jankurai/<binary_sha256>/{bin/jankurai,receipt.json}` and `<cache>/manifests/<manifest_sha256>.toml`. Entries are written atomically and never overwritten, and the binary digest is rechecked on every use. Nothing on `PATH` and nothing labelled latest upstream is ever consulted.
- **Hosts.** jeryu-tool's installer stages every `active` and `candidate` artifact into the same digest-keyed layout and publishes the ready digests (the runner's capability list). Retired entries stay for a retention window, and the installer never prunes a digest bound to a queued or claimed job.
- **Retention.** The forge keeps every snapshot. Released artifacts of every entry ever active are kept in an installation-owned store keyed by digest, so reproduction and rollback do not depend on the public release repository staying up.

### Contract: Jeryu client/server compatibility

The `jeryu` client and the forge agree on the manifest schema and capabilities, not on a Jankurai version. `/api/v1/capabilities` gains (proposed) `"jankurai_auditors": {"route": "/api/v1/jankurai/auditors", "manifest_schemas": [2], "modes": ["normal", "reproduce", "offline"]}`. The client picks the highest schema both support. If there is none, it stops with an actionable error that names its own version and schemas, the server's schemas, and the minimum client release that supports them. A Jankurai patch never changes the manifest schema. Schema changes are additive first, and the forge serves N and N-1 for a deprecation window, so clients upgrade on Jeryu's cadence, not Jankurai's.

### Contract: policy, wrappers and images

`agent/audit-policy.toml` replaces `required_tool_version` with `[tool] name = "jankurai"`, `compat = ">=1.7, <2"` and the required schemas. The policy digest excludes auditor identity. A job whose active entry falls outside the range is not bound and shows `incompatible-policy`. `require_jankurai` keeps the #25 trust anchors and verifies the assigned identity. Sandbox images are built by a pipeline from the manifest, tagged by binary digest, with the receipt as a build artifact.

### Contract: activation and rollback

1. **Candidate PR** (jeryu-tool) adds a `candidate` entry. It changes no binding, so it passes under the current active release.
2. **Staging**: hosts and the image pipeline pick it up.
3. **Parity**: `qualification` jobs run the candidate and the active release on a fixed corpus: each governed `main` as a full audit, and recent PR heads, including a no-base head, as diff audits, plus a no-base `full` job run with the runner's exact invocation (trusted policy bytes at `./agent/audit-policy.toml`). Both modes must validate against the declared schemas, with no new `tool-failed` or `no-base-diff` and matching decisions; score deltas are listed for review.
4. **Readiness**: every gate host advertises the digest and the image is built.
5. **Activation PR** sets it `active` and the old entry `retired`, gated by the outgoing release. New jobs bind to the new entry; queued and claimed jobs keep theirs.
6. **Rollback** is an activation PR re-activating the previous entry, which hosts and the retention store still hold. It needs no download, render or rebuild, and both transitions appear in the history.

## Consequences

- A compatible patch is two small jeryu-tool PRs, with no consumer edits, no forge rebuild, no client upgrade and no red window.
- The forge depends on jeryu-tool main as configuration. A broken manifest cannot loosen anything but can stall new bindings, visibly.
- Hosts, caches and the retention store use more disk; the installer becomes multi-release.
- Policy files change shape once in about 40 repositories; splitctl's exact equality becomes a range check.
- Contributed heads need a sandbox-capable runner. Until one exists, merge-gated repositories cannot merge contributions; those jobs sit visibly in `waiting-for-sandbox`.
- Tickets are in-process today; one lost to a restart is re-enqueued under the then-active entry. That is safe because no evidence existed, but persisting them is a follow-up.

## Migration

| # | Repository | Change | Owner |
|---|---|---|---|
| 0 | jankurai | Confirm whether the auditor reads `required_tool_version`; if so, accept a range first | jankurai |
| 1 | jeryu-tool | Manifest schema 2 (1.7.4 sole `active`), check, side-by-side installer, pin-free verifier; legacy path and stamp track `active` | jeryu-tool |
| 2 | jeryu-deploy | Snapshot loader, history, auditors route, capabilities field, binding with `metadata_commit` and `origin`, release route, qualification ingest, validation; legacy unbound reports accepted only if equal to `active` | deploy |
| 3 | jeryu-ci-runner | Capability claim (digests, sandbox), metadata from `metadata_commit`, verifier, release route use, `/runners` states, drop constants and `jeryu_audit_policy` | gate-runner session |
| 4 | jeryu-deploy | `jeryu` CLI local resolution, cache and modes; image pipeline; remove compiled constants and the `include_str!` receipt | deploy |
| 5 | jeryu-tool + consumers | Renderer stops writing per-release values; one last render de-pins every claimed consumer | jeryu-tool |
| 6 | jain repos, splitctl | Literal checks become the wrapper; splitctl compares compatibility | jain owners |
| 7 | jeryu-deploy | Refuse unbound reports once every runner sends capabilities | deploy |

Compatible throughout: the #25 trust anchors and receipt schema, the legacy install path and stamp, additive ingest fields until step 7, scores and floors. Until step 4 the compiled constants remain and are cross-checked against the snapshot, so reverting step 2 restores today's behaviour.

### Acceptance checks

- **Manifest-only patch.** Candidate, qualify, activate 1.7.N+1. No file outside `tool-manifest.toml` changes in any repository, and the forge and `jeryu` binaries are the same builds.
- **Same release everywhere.** A fresh CI job and a fresh local normal-mode run after activation both report the new `version` and `manifest_sha256`.
- **Reproduce.** A job scored before activation (and one before a rollback) reruns locally on its bound snapshot and binary and matches its stored report.
- **Refusal.** Tampered, unsigned or unlisted artifacts, a candidate on a `proof` job, a retired entry on a new job, any mismatched binding field, and a claim without the digest, a `metadata_commit` or `policy_sha256` mismatch, and a contributed job offered to a non-sandbox runner are refused and never scored or cached. A binary removed after claim produces a `tool-gone` release, not a score.
- **In flight.** A job claimed before activation completes and is accepted under its old binding.
- **Compatibility.** A client without a common manifest schema stops with the actionable error. An ambiguous origin asks for an explicit selection.
- **Offline.** With a verified cache, offline runs and reports `freshness: unknown`. With no cache, or a cache whose digest no longer verifies, it stops. It never uses `PATH`.
- **Fail closed and rollback.** An invalid manifest (two active entries, a deleted entry, a bad digest) is rejected and recorded. Rollback rebinds new jobs with no install, render or rebuild.

### Open questions for the owner

1. Emergency rollback: manifest PR only, or also a forge admin override recorded in history?
2. Signer rotation: manifest-only, or a deploy-side acknowledgement and a client prompt?
3. Parity bar: equal decisions with listed deltas, or a numeric tolerance?
4. Compatibility range form: semver range or schema ids only?
5. Retention store and window for retired artifacts on hosts?
6. Persist audit tickets across forge restarts as part of step 2?

## Appendix: inventory of today's copies

| Repository | Path | What it holds |
|---|---|---|
| jeryu-tool | `tool-manifest.toml` `[jankurai]`; `generated/jankurai-pin.env` | the single pin of record and its generated mirror |
| jeryu-tool | `crates/jeryu-tool-control/src/render_rules.rs`, `render.rs`; `ops/render-assets/require-jankurai.sh` | rewrites version, digests and sandbox stage into consumers; wrapper template |
| jeryu-tool | `ops/install-jankurai.sh`, `ops/auto-install-jankurai.sh` | single-slot install from protected main; authority stamp |
| each consumer | `agent/audit-policy.toml`, `agent/ci-lanes.toml`, `agent/native-cli-manifest.toml` | exact semver, version and binary digest |
| each consumer | `ops/ci/lib.sh`, `ops/ci/ensure-jankurai.sh`, `ops/ci/test-governed-jankurai.sh`, docs | rendered wrapper, test and prose |
| jeryu-deploy | `crates/jeryu-api/src/ci_bridge.rs` `GOVERNED_JANKURAI_*`; `ci_bridge/jankurai.rs` `verify_reported_auditor`; `web/jankurai/audits.rs` | compiled identity, equality check, 422 |
| jeryu-deploy | `images/agent-sandbox/jankurai-installation-receipt.json` (`include_str!`), `Dockerfile`, `README.md`; `ops/agent-sandbox/smoke.sh` | receipt in the forge binary; stage `ARG`s, pinned version and digest |
| jeryu-deploy | `crates/jeryu-api/tests/jankurai_governance.rs`, `src/web/tests/jankurai_audit_tests.rs`, `docs/governed-jankurai.md`, `docs/testing.md` | test constants, fixtures, prose |
| jeryu-intelligence, jeryu-release-ops | `ops/ci/test-governed-jankurai-path.sh` | governed-path test with stamp and receipt layout |
| jeryu-ci-runner | `crates/jeryu-runnerd/tests/hosted_dependency_transport.rs` | governed path, stamp, version fixtures |
| jain, jain-catboost, jain-cli, jain-deploy, jain-nexus, jain-tui, jain-web, jair-search | `ops/ci/lib.sh` (jain-nexus also `ops/ci/proofbind-bind.sh`) | literal `jankurai 1.6.11` checks |
| jain-split-ops | `tools/splitctl/src/main.rs`; `agent/audit-policy.toml` in about 32 repos | policy version must equal the executable's |
