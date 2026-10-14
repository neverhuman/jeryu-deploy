# 0011. Pin the governed Jankurai auditor centrally by approved release

Status: Proposed
Date: 2026-10-14
Supersedes: none
Superseded-by: none

## Context

The governed Jankurai auditor is a key-signed public release named in jeryu-tool's `tool-manifest.toml`. That file is meant to be the single source of truth, but every release value (version, tag, asset, binary and provenance digests) is copied out of it into consumer source. A patch bump today means a jeryu-tool pin PR, a render PR in every claimed consumer, a forge rebuild (the accepted identity is a set of Rust constants plus an `include_str!` of the sandbox receipt), a sandbox image rebuild, and hand edits in repositories the renderer does not reach. The 1.7.4 render alone touches 13 files in jeryu-deploy; the appendix lists every copy.

Three failures follow from copying:

- **Bump deadlock.** A pin PR must pass gates that verify the gate host's installed auditor, and hosts install a pin only after it reaches protected main. jeryu-tool#25 kept per-release values out of the consumer template for this reason and pinned only the trust anchors (release repository, signer key digest, cosign digest, target), which do not change between releases.
- **Fleet-wide red between steps.** The forge refuses (422) any report whose version or digest differs from its compiled constants, so between host install, consumer render and forge deploy some set of repositories is always red.
- **Qualification was a single full audit.** A release that passed a full audit regressed `diff-audit` and blocked gated repositories. No step ran the candidate under both modes before it governed real heads.

The owner approved the direction on 2026-10-14: a compatible auditor patch needs one manifest PR in jeryu-tool plus automated rollout, with no consumer source edits and no forge rebuild. Scoring rules, floors, caps and review obligations are not changed by this record.

## Decision

### Decisions

1. **Membership authorizes, signatures do not.** `tool-manifest.toml` on jeryu-tool protected main carries an `approved` table of releases. A release can produce governed evidence only while it is in that table; a valid signature from the pinned key is necessary but never sufficient. Each entry is `candidate`, `active` or `retired`, and exactly one entry is `active`.
2. **The forge reads the table as configuration.** The forge loads a verified, immutable snapshot of the approved table from jeryu-tool protected main through its own protected-main readback, keyed by manifest commit and manifest digest. It never takes authority from a PR head, a runner, a request body or a compiled constant. An invalid snapshot fails closed: the last valid snapshot stays in force and the failure is visible; with no valid snapshot, jobs queue in `waiting-for-authority` and the ingest refuses every report.
3. **Jobs carry their auditor.** Each audit job binds at enqueue to one snapshot entry plus its source head, base and policy digest. A report is validated against the job's binding, never against whatever is active when it arrives. In-flight jobs finish on the snapshot they were bound to, and stored evidence is never reinterpreted.
4. **Runners advertise what they can run.** A runner claims with the digests it has installed and only receives jobs it can run. It carries no release constants; a stable, pin-free verifier checks the installed binary and receipt against the job's assigned identity before checkout. A mismatch refuses the job and is never scored. The gate-runner session owns this side and its proposal is adopted as written below.
5. **Hosts install side by side.** Hosts stage every `active` and `candidate` entry in digest-keyed directories, so a candidate runs before activation and rollback needs no download.
6. **Repository policy names compatibility, not a patch.** Policy keeps rules, floors, exceptions and required capabilities. `required_tool_version` is replaced by a compatibility range. The renderer stops writing per-release values anywhere.
7. **Images follow the manifest.** The sandbox image rebuilds automatically from the active (and candidate) entries. Exact digests live in built artifacts and receipts, not in consumer source.
8. **Qualification precedes activation.** A candidate runs full-audit and diff-audit parity on a fixed corpus, then host and image readiness are confirmed, then a separate activation PR flips `active` for new jobs. The activation PR is itself audited by the outgoing active release, which removes the bump deadlock.

### Contract: manifest (jeryu-tool `tool-manifest.toml`, schema 2)

```toml
schema_version = "2"

[jankurai.trust]            # changes only on key, cosign, repo or target rotation
release_repo          = "<public release repository>"
release_signer_sha256 = "<64 hex>"
cosign_version        = "v3.1.3"
cosign_sha256         = "<64 hex>"
target_triple         = "x86_64-unknown-linux-gnu"

[[jankurai.approved]]
version           = "1.7.4"         # `jankurai --version` prints "jankurai 1.7.4"
tag               = "v1.7.4"
state             = "active"        # candidate | active | retired
asset             = "jankurai-1.7.4-x86_64-unknown-linux-gnu.tar.gz"
asset_sha256      = "<64 hex>"
binary_sha256     = "<64 hex>"
provenance_sha256 = "<64 hex>"
source_commit     = "<40 hex>"
signing_identity  = "<release_signer_sha256 of the key that signed it>"
compat            = { report_schema = "<report schema id>", policy_schema = "<policy schema id>", modes = ["full", "diff"] }
qualification     = "<qualification record id; required before state = active>"
```

Manifest rules, enforced by jeryu-tool's own check and again by the forge loader: exactly one `active`; `binary_sha256` unique; an entry once present is never deleted (retire it instead), so every historical binding stays resolvable; `signing_identity` equals `jankurai.trust.release_signer_sha256` unless a rotation entry says otherwise; `active` requires a `qualification` id that the forge has recorded as passed for that `binary_sha256`.

### Contract: forge snapshot

- **Source.** The forge reads `tool-manifest.toml` at the protected `main` of jeryu-tool from its own repository store, after confirming the branch protection readback for that ref. It loads on startup, on every push to that ref and on a periodic recheck.
- **Key.** `{manifest_revision: <commit>, manifest_sha256: <sha256 of the file bytes>}`. A snapshot is immutable; a new commit is a new snapshot.
- **Record.** Each accepted snapshot and each transition it implies (`candidate added`, `activated`, `retired`, `rolled back`) is appended to a forge table that is never updated or deleted, with the observing time and the previous active entry. Rejected snapshots are recorded with the reason.
- **Route.** `GET /api/v1/jankurai/auditors` (any signed-in account; read-only):

```json
{
  "snapshot": {"manifest_revision": "<40 hex>", "manifest_sha256": "<64 hex>", "loaded_at": "2026-10-14T12:00:00Z"},
  "active":  {"version": "1.7.4", "binary_sha256": "<64 hex>", "signing_identity": "<64 hex>", "compat": {"report_schema": "…", "policy_schema": "…"}},
  "staged":  [{"version": "1.7.5", "state": "candidate", "binary_sha256": "<64 hex>", "qualification": {"full": "passed", "diff": "running"}, "ready_hosts": 1, "image": "built"}],
  "history": [{"at": "2026-10-14T12:00:00Z", "event": "activated", "version": "1.7.4", "previous": "1.6.11", "manifest_revision": "<40 hex>"}],
  "rejected_snapshots": [{"manifest_revision": "<40 hex>", "reason": "two entries are active"}]
}
```

### Contract: job binding and submission

A ticket gains an `auditor` block and a `policy_sha256`, fixed at enqueue (claim response shape, snake_case like the existing API):

```json
{"job_id": "…", "owner": "…", "repo": "…", "branch": "…", "head_sha": "<40 hex>", "base_sha": "<40 hex>", "audit_mode": "diff",
 "policy_sha256": "<64 hex>", "purpose": "proof",
 "auditor": {"manifest_revision": "<40 hex>", "manifest_sha256": "<64 hex>", "version": "1.7.4", "binary_sha256": "<64 hex>", "signing_identity": "<64 hex>"}}
```

`purpose` is `proof` for governed heads and `qualification` for candidate parity runs; a qualification job never writes a score or a `jankurai/proof` check. The submission to `POST /api/v1/repos/:id/jankurai-scores` echoes `job_id`, the whole `auditor` block, `policy_sha256`, head and base, and adds `receipt_sha256`. The ingest compares every echoed field with the job's stored binding and refuses on any difference (422 `auditor_mismatch`, recorded as a rejected publication with the field named). Stored score rows keep the binding, so a later activation or rollback never changes what an old row was checked against.

### Contract: runner (gate-runner session's proposal, adopted)

- Claim: `{"runner_id": "…", "max": 1, "capabilities": {"jankurai": ["<binary_sha256>", "…"]}}`. The forge hands out only jobs whose `auditor.binary_sha256` is in the list.
- A job no recently claiming runner can run shows `waiting-for-tool` in the queue and on `/runners`, with the digest it waits for.
- Before checkout, the runner calls the stable verifier with the job's `auditor` block. It checks the binary under `<tools>/jankurai/<binary_sha256>/` (digest, `--version`, no symlink) and its receipt (trust anchors, signing identity, binary digest) against that block, then exports the assigned path to the lanes. On mismatch it releases the job with a reason; nothing is scored.
- `/runners` shows the states `waiting-for-tool`, `running` and `publishing`, and the conclusion `rejected-publication` with the ingest's reason.
- The runner holds no version, tag or digest constants.

### Contract: hosts, policy, wrappers, images

- **Hosts.** jeryu-tool's installer stages each `active` and `candidate` entry into `<tools>/jankurai/<binary_sha256>/{bin/jankurai,receipt.json}`, verified offline with the pinned key and cosign, atomically and never over an existing digest directory. Retired entries are kept for a retention window. Each host publishes which digests are ready (the runner's capability list).
- **Policy.** `agent/audit-policy.toml` replaces `required_tool_version = "1.7.4"` with `[tool] name = "jankurai"`, `compat = ">=1.7, <2"` and the required report and policy schema. The policy digest excludes auditor identity. A job whose active entry falls outside the repository's range is not bound; it shows `incompatible-policy`, never a pass.
- **Wrappers.** `ops/ci/lib.sh` `require_jankurai` and `ensure-jankurai.sh` keep the trust anchors from jeryu-tool#25 and verify the identity assigned in the environment by the runner (or, for a local `required` run, the host's active entry). No consumer file names a version or binary digest.
- **Images.** An image pipeline builds the sandbox from the active entry, tags it by binary digest and publishes its receipt as a build artifact. jeryu-deploy no longer commits `images/agent-sandbox/jankurai-installation-receipt.json` or generated Dockerfile `ARG` values.

### Contract: qualification and activation

1. **Candidate PR** (jeryu-tool): adds an entry with `state = "candidate"`. It changes no job binding, so it passes under the current active release.
2. **Staging**: hosts install the candidate; the image pipeline builds a candidate image.
3. **Parity**: the forge enqueues `qualification` jobs bound to both the candidate and the active entry over a fixed corpus: every governed repository's protected `main` as full audit, and a recent set of PR heads as diff audit, including a no-base head. It passes when reports validate against the declared schemas, no head moves to `tool-failed` or `no-base-diff`, and decisions match (score deltas are listed for review). Both modes must pass.
4. **Readiness**: every gate host advertises the candidate digest and the candidate image is built.
5. **Activation PR** (jeryu-tool): sets the candidate `active` and the old entry `retired`. Its own gate runs under the outgoing active entry. After merge, new jobs bind to the new entry; queued and claimed jobs keep theirs.
6. **Rollback**: an activation PR that re-activates the previous entry, which hosts still hold. No download, render or rebuild.

## Consequences

- A compatible patch is two small jeryu-tool PRs (candidate, then activation), with no consumer edits, no forge rebuild and no red window. The 13-file deploy render and the per-consumer render rounds stop.
- The forge gains a configuration dependency on jeryu-tool main. A broken manifest cannot loosen anything, but it can stall new bindings until fixed; the stall is visible on the auditors route and `/runners`.
- Hosts use more disk (one directory per staged release) and the installer becomes multi-release.
- Policy files change shape once in about 40 repositories, and splitctl's exact equality check becomes a range check.
- Queued tickets are in-process today; a ticket lost to a restart is re-enqueued and binds to the then-active entry. That is safe because no evidence existed for it, but persisting tickets is a follow-up.
- Accepted evidence stays verifiable after rollback or retirement because entries are never deleted and rows keep their binding.

## Migration

One coordinated initial migration. Each step stays compatible with the step before it.

| # | Repository | Change | Owner |
|---|---|---|---|
| 0 | jankurai | Confirm whether the auditor itself reads `required_tool_version`; if it does, accept a range first. | jankurai |
| 1 | jeryu-tool | Manifest schema 2 with 1.7.4 as the only `active` entry; manifest check; side-by-side installer; stable verifier. The legacy single install path and authority stamp keep pointing at the active entry. | jeryu-tool |
| 2 | jeryu-deploy | Snapshot loader, append-only record, auditors route, ticket binding, ingest validation against the job. Legacy submissions without a binding are accepted only when they equal the active entry. | deploy |
| 3 | jeryu-ci-runner | Capability claim, verifier call, `waiting-for-tool` and `rejected-publication` on `/runners`; drop release constants. | gate-runner session |
| 4 | jeryu-deploy | Image pipeline from the manifest; remove the compiled constants, the `include_str!` receipt and the Dockerfile auditor `ARG`s. | deploy |
| 5 | jeryu-tool + jeryu consumers | Renderer stops writing per-release values; one last render round de-pins every claimed consumer (policy range, ci-lanes, native-cli manifest, wrappers, docs). | jeryu-tool |
| 6 | veox/jain repos, splitctl | Replace literal version checks with the stable wrapper; splitctl compares compatibility; policy range in about 32 repositories. | jain owners |
| 7 | jeryu-deploy | After every runner sends capabilities, refuse legacy submissions. | deploy |

What stays compatible during the transition: the trust anchors and receipt schema from jeryu-tool#25; the legacy install path and authority stamp; the ingest's existing fields (the new ones are additive until step 7); scores and floors. Rollback of the migration itself: until step 4 the compiled constants remain and are cross-checked against the snapshot, so reverting step 2 restores today's behaviour; after step 4, rollback is a manifest activation PR as above.

### Acceptance tests

- **Patch by manifest only.** Add 1.7.N+1 as candidate, qualify, activate: no file outside `tool-manifest.toml` changes in any repository, the forge binary is the same build, and new heads score under the new release.
- **Refusal.** Reports from an unlisted binary, a candidate on a `proof` job, a retired entry on a new job, a mismatched `manifest_sha256`, `receipt_sha256` or `policy_sha256`, and a runner claiming without the digest are all refused and never scored.
- **In flight.** A job claimed before activation completes and is accepted under its old binding after the switch.
- **History.** A score row from before activation and one from before a rollback both re-verify against their stored binding.
- **Rollback.** Re-activating the previous entry binds new jobs to it with no install, render or rebuild, and the history shows both transitions.
- **Fail closed.** A manifest with two active entries, a deleted entry or a malformed digest is rejected, recorded and does not replace the last valid snapshot.

### Open questions for the owner

1. Emergency rollback: manifest PR only, or also a forge admin override recorded in the append-only history?
2. Signer key rotation: manifest-only like a patch, or does it need a deploy-side acknowledgement?
3. Parity bar: equal decisions with listed score deltas, or a numeric tolerance per release?
4. Compatibility range form: semver range, or report and policy schema ids only?
5. Retention for retired binaries on hosts: fixed days, or until no stored evidence needs re-running?
6. Should audit tickets persist across forge restarts as part of step 2?

## Appendix: inventory of today's copies

| Repository | Path | What it holds |
|---|---|---|
| jeryu-tool | `tool-manifest.toml` `[jankurai]` | the pin of record (single release) |
| jeryu-tool | `crates/jeryu-tool-control/src/render_rules.rs`, `render.rs` | rewrites version, digests and sandbox stage into consumers |
| jeryu-tool | `generated/jankurai-pin.env`, `ops/render-assets/require-jankurai.sh` | generated pin; consumer wrapper template (trust anchors only since #25) |
| jeryu-tool | `ops/install-jankurai.sh`, `ops/auto-install-jankurai.sh` | single-slot install from protected main; authority stamp |
| each consumer | `agent/audit-policy.toml` `required_tool_version` | exact semver |
| each consumer | `agent/ci-lanes.toml` `jankurai_version`, `agent/native-cli-manifest.toml` | exact version and binary digest |
| each consumer | `ops/ci/lib.sh`, `ops/ci/ensure-jankurai.sh`, `ops/ci/test-governed-jankurai.sh`, docs | rendered wrapper and test, version prose |
| jeryu-deploy | `crates/jeryu-api/src/ci_bridge.rs` `GOVERNED_JANKURAI_*` | compiled version, digest, source, manifest commit and digest |
| jeryu-deploy | `crates/jeryu-api/src/ci_bridge/jankurai.rs` `verify_reported_auditor` | equality check against the constants |
| jeryu-deploy | `images/agent-sandbox/jankurai-installation-receipt.json` (`include_str!`) | receipt compiled into the forge |
| jeryu-deploy | `crates/jeryu-api/src/web/jankurai/audits.rs` | 422 on mismatch |
| jeryu-deploy | `images/agent-sandbox/Dockerfile`, `ops/agent-sandbox/smoke.sh`, `images/agent-sandbox/README.md` | auditor stage `ARG`s, pinned version and digest |
| jeryu-deploy | `crates/jeryu-api/tests/jankurai_governance.rs`, `crates/jeryu-api/src/web/tests/jankurai_audit_tests.rs`, `docs/governed-jankurai.md`, `docs/testing.md` | test constants, version fixtures, prose |
| jeryu-intelligence, jeryu-release-ops | `ops/ci/test-governed-jankurai-path.sh` | governed-path test with stamp and receipt layout |
| jeryu-ci-runner | `crates/jeryu-runnerd/tests/hosted_dependency_transport.rs` | governed path, authority stamp, version fixtures |
| jain, jain-catboost, jain-cli, jain-deploy, jain-nexus, jain-tui, jain-web, jair-search | `ops/ci/lib.sh` (jain-nexus also `ops/ci/proofbind-bind.sh`) | literal `jankurai 1.6.11` checks |
| jain-split-ops | `tools/splitctl/src/main.rs`, `agent/audit-policy.toml` in about 32 repos | `required_tool_version` must equal the executable's version |
