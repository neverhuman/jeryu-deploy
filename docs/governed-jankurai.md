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
not claim the initial PATH was rejected. The forge itself runs no auditor
(see *Where an authoritative score is produced* below); it validates the
complete local source, build, and protected jeryu-tool manifest authority of the
pinned installation receipt, and accepts only a report produced by that exact
binary. Verification
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


## Where an authoritative score is produced

Scoring runs on the gate runners, never on the forge host. The forge serves git
for the whole family, and an audit is a clone plus a full walk; one audit per
pushed head once wedged it during a bulk import of ~244 branches.

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
   `jankurai audit .` — the same whole-tree invocation `<repo>/required` and
   `ops/jankurai/backfill-repo-scores.sh` use — and the head gets a real score
   and a `jankurai/proof` that passes or fails on its findings.

   It is not diffed against the empty tree: `git diff base...head` refuses a
   tree (`object 4b825dc6... is a tree, not a commit`), so jankurai 1.6.11 reads
   an empty change set, prints `nothing to audit`, exits 0 and writes a report
   with no verdict in it. A parentless commit of that tree does not help either
   — a three-dot diff needs a merge base, and an unrelated root has none. That
   empty-tree base is why all 16 bootstrapped `root/jankurai*` mains recorded
   `decision: tool-failed, score: null`.

   The ingest keeps the two apart: a `full` job accepts only a `full` report
   (a diff report for it could only be a diff against nothing), while a `diff`
   job still accepts a `full` one, which is how `<repo>/required` stands in.
   A report that comes back with nothing to audit and no verdict fields at all
   is recorded under its own decision, `no-base-diff` — proof title *no base to
   diff against* — never as a pass and no longer hidden inside a generic
   `tool-failed`.

3. **Runners do the work.** `ops/ci/jankurai-audit-runner.sh` claims jobs
   (`POST /api/v1/jankurai-audits/claim`), clones the head, runs the governed
   auditor — `diff-audit` against the job's base, or a whole-tree `audit` for a
   job with no commit base; niced, ionice'd and time-boxed — and submits the
   report with
   `ops/ci/submit-jankurai-score.sh`.
4. **One audit per head.** When `<repo>/required` (`ops/ci/pr-ci.sh`) already
   audits a head, that run submits its own report (`--audit-mode full`) against
   the same job, and the job is spent: no runner audits the head again.
5. **The ingest is the authority.**
   `POST /api/v1/repos/{id}/jankurai-scores` accepts a report only when

   * the caller is a runner identity allowed to score
     (`JERYU_JANKURAI_SCORERS`, default `gatebot`) or a global admin,
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
