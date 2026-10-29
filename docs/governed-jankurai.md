# Governed Jankurai

The protected `jeryu-tool/tool-manifest.toml` is the one authority for the
Jankurai release accepted by this forge. Changes to that manifest land through
review and required checks in the Jeryu forge. The release repository named by
the manifest is an artifact distribution location; it does not approve its own
identity or replace protected Jeryu source and review authority.

The current schema-1 pin is `jankurai 1.7.5` (`v1.7.5`), matching the
rendered forge constants and sandbox recipe. This describes the protected
manifest; it is not an independent pin.

## Installation and trust

The protected tool installer downloads only the manifest-pinned public release
asset, its signature bundle, and signed provenance. It verifies both signatures
offline with the pinned release key and pinned cosign binary, plus the asset,
binary and provenance digests. The public key is committed in the tool
repository and pinned by digest independently of the artifact download.
Transparency-log verification is not part of this key-based trust policy.

Build provenance belongs to the release pipeline. This policy supersedes the
former Jeryu-local hermetic build: consumers must not claim that a downloaded
release was built inside their own network-isolated source-build sandbox.
The manifest records the source commit/tree, lockfile identities, target and
compiler identities from the accepted signed provenance. It does not turn a
consumer's unexecuted build into source-reproduction evidence.

The installer records the verified release, physical binary path and protected
manifest identity in a content-addressed installation receipt and authority
stamp. The rendered shell verifier checks the installed binary and that
receipt against the protected trust anchors; it does not download, install or
execute a signature-verification network request. Missing, tampered or
unexportable identity fails closed. The shell verifier still accepts source-build
v2 receipts during the host/image migration to release-key-signed v3 receipts;
the authority stamp alone does not retire v2. That transition does not let forge ingestion accept a binary outside
its compiled governed identity.

## Forge ingestion

The generated `GOVERNED_JANKURAI_*` constants in `ci_bridge.rs` derive from the
same manifest. Audit ingestion requires the reported version and binary digest
to match that governed identity and validates the report's source and proof
provenance. A locally successful score from a different tool is insufficient.
An approved manifest change, rendered consumer source, installed gate tool,
and deployed forge identity must be reconciled as separate rollout steps.

## Agent sandbox

The auditor release stage and runtime both use a libc-compatible Debian base.
The image verifies the pinned artifact before copying it into the runtime,
checks its digest and version there, and runs the same offline receipt guard.
The normal agent runtime is network-isolated; artifact download happens during
the image build. A source test is not an image-build or real-guest receipt.

## Rotation and rollback

[ADR 0012](adr/0012-authenticate-auditor-anchor-rotation-and-bootstrap.md)
supersedes ADR 0011's single-anchor rotation example. Its schema-2 multi-anchor
protocol remains proposed: this schema-1 1.7.5 pin does not implement durable
snapshot-bound jobs, multi-slot installation or manifest-only fleet activation.
The record requires add, activate, retire through protected manifest changes,
with each new anchor authenticated by an already trusted key and pinned cosign.
Clients must verify the installation-bound, ordered transition chain and
atomically persist its checkpoint; manifest membership or a new key's own
signature is insufficient. Exactly one anchor may be active for new proof
jobs; historical anchors and verification material remain available.

The schema-2 bootstrap must demonstrate full/diff parity, the exact no-base
full invocation, and host/image readiness. 1.7.4 is excluded by its diff
regression; 1.7.5's current pin does not by itself prove that bootstrap ready.
Until that migration lands, use the existing rollout below.

Change the tool manifest with `ops/pin-jankurai-release.sh` in `jeryu-tool`,
review and land it, then use its canonical renderer for every claimed consumer.
Never hand-edit generated identity blocks. Run the governed shell verifier,
`cargo test --locked -p jeryu-api --features web --test jankurai_governance`,
the full required gate, and the sandbox build/smoke lane in an admitted build
allocation. Require fresh independent review on the final rendered head.

Before retiring a producer, observe the replacement publish a valid report for
an exact head accepted by the deployed forge. Preserve the predecessor binary
and receipts for controlled rollback. Rollback changes authority through the
same reviewed manifest and renderer process; it does not enable an environment
escape hatch or weaken score floors and branch protection.

## Where an authoritative score is produced

Scoring runs on the gate runners, never on the forge host. The forge serves git
for the whole family, and an audit is a clone plus a full walk; one audit per
pushed head once wedged it during a bulk import of ~244 branches.

The flow below describes the pr-gate 2.1.x host contract used by this forge.
It does not claim ADR 0012's proposed snapshot/job bindings are deployed.

The flow, per head:

1. **The forge records work.** A push writes at most one audit job — owner,
   repository, branch, head sha, base sha, audit mode — into the in-process
   audit queue
   (`crates/jeryu-api/src/ci_bridge/audit_queue.rs`) and publishes
   `jankurai/proof` as *pending*. It runs no clone, no checkout and no auditor,
   so the push acknowledgement stays immediate. A newer tip of the same branch
   supersedes an unclaimed job; the same head is never queued twice.
2. **What is audited.** The protected `main` and pull request heads. Branches
   under `import/`, `preserve/`, `archive/`, `archives/`, `bot/` and `auto/`
   (and those names on their own) create no job at all — no clone is made for
   work that carries no review and no gate.

   **A head with no commit base** — a first `main` (including one created by a
   PR merge), an orphan branch, a branch unrelated to `main` — is audited
   **whole**: the job carries audit mode `full` and the base
   `4b825dc642cb6eb9a060e54bf8d69288fbee4904` (git's empty tree) purely as the
   marker for *no base*, which is never handed to the auditor. The runner runs
   a whole-tree audit from the clean checkout:

   ```sh
   jankurai audit . --full --mode advisory --policy agent/audit-policy.toml \
     --json "$out" --no-score-history
   ```

   The policy path must be in-tree for Jankurai 1.7.x; the runner installs
   trusted policy bytes there before invoking it. The empty-tree marker is
   never passed to `diff-audit`: a three-dot diff requires a commit base with
   common ancestry. A parentless commit wrapping the empty tree cannot supply
   that ancestry either.

   The ingest keeps the two apart: a `full` job accepts only a `full` report
   (a diff report for it could only be a diff against nothing), while a `diff`
   job still accepts a `full` one.
   A report that comes back with nothing to audit and no verdict fields at all
   is recorded under its own decision, `no-base-diff` — proof title *no base to
   diff against* — never as a pass and no longer hidden inside a generic
   `tool-failed`.
   The one report with nothing to audit that does pass is the exact
   no-changes report Jankurai 1.7.5 writes when the head equals its base
   (`result: "no-changes"`, `score: 100`, and `changed_count`, `caps_applied`
   and `hard_findings` all `0`, no `decision`): it is scored 100 with the proof
   title *no changes to score*. `ops/ci/jankurai-gate.sh` renders it the same.

3. **The gate host does the work.** `pr-gate-audit.sh` (jeryu-ci-runner
   `ops/pr-gate`, installed and kept current with the gate itself) claims jobs
   (`POST /api/v1/jankurai-audits/claim`) as the gate identity, for the
   repositories that host serves, and admits only reviewed code: a `main` head
   the clone's own `main` reaches, or the head of an open pull request marked
   `origin:internal`. A contribution is never checked out on the gate host, so it
   produces no score there; its pending job lapses with its lease. It clones the
   head into a throwaway directory and gives it `agent/` and `.jeryu/` (policy,
   generated zones, boundaries, maps, CI declaration) only from a commit `main`
   reaches: the job's base for a diff when it is on `main`, otherwise the
   admitted head for a `main` job, or `main`'s tip for a topic. Missing trusted
   policy uses the runner's default policy. A topic never chooses the metadata
   that scores it; the runner hides its substitutions from the diff. It runs
   the host's governed auditor --
   `diff-audit` against the job's base, or a whole-tree `audit` for a job with
   no commit base; niced, idle I/O and time-boxed -- and submits the report.
   The auditor is the one the host's tool installer keeps on the pin of record:
   `pr-gate-ensure-jankurai.sh`, installed beside the runner, checks the
   binary, its installation receipt and the host's authority stamp. A host whose
   auditor does not verify audits nothing. Passes start 30 seconds after the previous pass
   finishes and never overlap. Every pass also posts a runner heartbeat
   (`jankurai-audit` label,
   `<host>/jankurai-audit`), so `/runners` shows the auditor, what it is
   auditing, how its last audit went, the gate's installed code and the digest
   of the jankurai binary it really invokes (`tools`). The same identity sends
   it, so it must also be allowed to report heartbeats (a forge admin or a
   `JERYU_RUNNER_REPORTERS` login). See docs/pipeline-events.md, "The jankurai
   audit runner".
4. **Only the auditor's own report counts.** A `<repo>/required` recipe that
   enforces a score floor audits its head too, but that report is written by
   the code under test, so it is never submitted: every recorded score comes
   from the gate host's auditor running on a clean clone.
5. **The ingest is the authority.**
   `POST /api/v1/repos/{id}/jankurai-scores` accepts a report only when

   * the caller is a runner identity allowed to score
     (`JERYU_JANKURAI_SCORERS`) or a global admin,
   * an open audit job exists for exactly this head, and the report names that
     job's branch and base sha, and
   * the reported auditor version and sha256 are the governed ones, checked
     against the installation receipt pinned into the server.

   Anything else is refused with no score recorded and no check published — a
   forged, mismatched or self-scored report never becomes a pass. The verdict
   itself is the forge's own reading of the report JSON (score against the
   effective floor, no hard findings, no caps), so a submission carries
   evidence, never a conclusion. The runner, base, auditor digest and receipt
   digest are kept with the score as `jeryu_audit_provenance`.
6. **No runner, no green.** With nothing claiming jobs, `jankurai/proof` stays
   pending and says it is waiting for a gate runner. There is no fallback to
   auditing on the forge host, and the check never passes by default. A claim
   whose runner never reports expires after 30 minutes and the head becomes
   claimable again.

A global admin may still ingest a raw score for a maintenance backfill
(`ops/jankurai/backfill-repo-scores.sh`); that path records the score and
publishes no check.
The gate this identity feeds — the local pre-approval command and the per-repo
rollout of `jankurai/proof` as a required context — is described in
[`docs/jankurai-gate.md`](jankurai-gate.md).
