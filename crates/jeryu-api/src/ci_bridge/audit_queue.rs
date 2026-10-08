//! The audit work a push records — and never performs.
//!
//! The forge serves git for the whole family; a jankurai audit is a clone plus
//! a full-tree walk, and running one per pushed head wedged the host during a
//! bulk import. So a push only writes a ticket here: which head needs an audit,
//! against which base. The gate runners claim tickets over
//! `POST /api/v1/jankurai-audits/claim`, run the governed auditor on their own
//! machines, and hand the report back through the authenticated ingest, which
//! is the only path that records a score and completes `jankurai/proof`.
//!
//! A ticket is the forge's own statement of what may be scored: the ingest
//! accepts a runner's report only when it matches one exactly (branch, head,
//! base). That is what keeps a score unforgeable now that the audit itself runs
//! off-host, and it is also what makes a head audited once — `<repo>/required`
//! submits against the same ticket, so nothing else is left to claim.

use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};
use jeryu_core::{ForgeCore, PullRequestState};
use serde::Serialize;

use super::RefUpdate;

/// Tickets the queue holds before it starts evicting the oldest unclaimed one.
/// A bulk push of hundreds of branches must not grow the forge without bound.
pub(crate) const AUDIT_QUEUE_CAPACITY: usize = 512;
/// A claimed ticket whose runner never reported returns to the queue after
/// this, so one lost runner does not strand a head as permanently pending.
pub(crate) const CLAIM_LEASE_SECONDS: i64 = 1_800;
/// Most tickets one claim call may take, so one runner cannot drain the queue.
pub(crate) const MAX_CLAIM_BATCH: usize = 8;

/// What a ticket records as the base of a head that has none: git's empty
/// tree, "nothing to diff against" spelled as an object id.
///
/// It is a marker, never an argument to `diff-audit`. A tree cannot be a diff
/// base — git refuses `base...head` with `object 4b825dc6... is a tree, not a
/// commit`, and the auditor reads that as an empty change set, says `nothing to
/// audit`, exits 0 and writes a report with no verdict in it. Nor can a
/// parentless commit of that tree: a three-dot diff needs a merge base, and an
/// unrelated root has none. So a head with no commit base is audited whole
/// ([`AUDIT_MODE_FULL`]) rather than diffed at all.
pub(crate) const NO_COMMIT_BASE_OID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// A head audited against its base: `jankurai diff-audit`.
pub(crate) const AUDIT_MODE_DIFF: &str = "diff";
/// A head audited whole: `jankurai audit`, which needs no base. The
/// `<repo>/required` gate produces this too, so it already satisfies a ticket.
pub(crate) const AUDIT_MODE_FULL: &str = "full";

/// Branch classes that carry no review and no gate, so they get no audit:
/// bulk imports, preserved history, archives, and the forge's own bot branches.
const UNAUDITED_BRANCH_PREFIXES: &[&str] = &[
    "import/",
    "preserve/",
    "archive/",
    "archives/",
    "bot/",
    "auto/",
];
/// The same classes spelled as a whole branch name.
const UNAUDITED_BRANCH_NAMES: &[&str] = &["import", "preserve", "archive", "archives"];

/// The repository's own CI declaration, read for the `[jankurai] audit` switch.
/// Schema 2 — the same file jankurai reads for the CI-present credit.
pub(crate) const CI_DECLARATION_PATH: &str = ".jeryu/ci.toml";
/// Most bytes of `.jeryu/ci.toml` the push path will read. A control file is a
/// few lines; anything larger is not one, and a push must not read a blob of
/// unbounded size on the host that serves git for the whole family.
pub(crate) const MAX_CI_DECLARATION_BYTES: u64 = 64 * 1024;
/// What a repository that turned audits off is told on the push path.
pub(crate) const AUDIT_OPT_OUT_REASON: &str =
    "this repository turned jankurai audits off in .jeryu/ci.toml";

/// One head waiting to be audited, and by whom if a runner holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuditTicket {
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) branch: String,
    pub(crate) head_sha: String,
    /// The commit the audit diffs against, or [`NO_COMMIT_BASE_OID`] when the
    /// head has none and is audited whole instead.
    pub(crate) base_sha: String,
    /// [`AUDIT_MODE_DIFF`] or [`AUDIT_MODE_FULL`]: which audit this head needs.
    /// A full audit may stand in for a diff one (it is strictly more work), a
    /// diff audit never for a full one — there is nothing for it to diff.
    pub(crate) audit_mode: String,
    pub(crate) enqueued_at: DateTime<Utc>,
    pub(crate) claimed_by: Option<String>,
    pub(crate) claimed_at: Option<DateTime<Utc>>,
}

impl AuditTicket {
    pub(crate) fn new(
        owner: &str,
        repo: &str,
        branch: &str,
        head_sha: &str,
        base_sha: &str,
    ) -> Self {
        Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            branch: branch.to_string(),
            head_sha: head_sha.to_string(),
            base_sha: base_sha.to_string(),
            audit_mode: AUDIT_MODE_DIFF.to_string(),
            enqueued_at: Utc::now(),
            claimed_by: None,
            claimed_at: None,
        }
    }

    /// Work for a head with no commit base — a first `main`, an orphan or
    /// unrelated branch: the whole tree is audited, because there is no diff.
    pub(crate) fn whole_tree(owner: &str, repo: &str, branch: &str, head_sha: &str) -> Self {
        Self {
            audit_mode: AUDIT_MODE_FULL.to_string(),
            ..Self::new(owner, repo, branch, head_sha, NO_COMMIT_BASE_OID)
        }
    }

    fn is_for(&self, owner: &str, repo: &str, head_sha: &str) -> bool {
        self.owner == owner && self.repo == repo && self.head_sha == head_sha
    }
}

/// What enqueueing a head did, so the caller can say it on the check run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnqueueOutcome {
    Queued,
    /// The same head is already waiting or being audited.
    AlreadyQueued,
    /// An earlier head of the same branch was dropped: only the tip matters.
    Superseded(usize),
    /// The queue is full of claimed tickets; the head stays unaudited and the
    /// proof stays pending rather than passing by default.
    Full,
}

#[derive(Debug, Default)]
pub(crate) struct AuditQueue {
    tickets: Vec<AuditTicket>,
}

impl AuditQueue {
    pub(crate) fn enqueue(&mut self, ticket: AuditTicket) -> EnqueueOutcome {
        self.expire_stale_claims(Utc::now());
        if self
            .tickets
            .iter()
            .any(|held| held.is_for(&ticket.owner, &ticket.repo, &ticket.head_sha))
        {
            return EnqueueOutcome::AlreadyQueued;
        }
        // An unclaimed ticket for an earlier tip of the same branch is work
        // nobody wants any more; a claimed one is already running, so leave it.
        let before = self.tickets.len();
        self.tickets.retain(|held| {
            held.claimed_by.is_some()
                || !(held.owner == ticket.owner
                    && held.repo == ticket.repo
                    && held.branch == ticket.branch)
        });
        let superseded = before - self.tickets.len();
        if self.tickets.len() >= AUDIT_QUEUE_CAPACITY {
            let oldest_unclaimed = self
                .tickets
                .iter()
                .position(|held| held.claimed_by.is_none());
            match oldest_unclaimed {
                Some(index) => {
                    self.tickets.remove(index);
                }
                None => return EnqueueOutcome::Full,
            }
        }
        self.tickets.push(ticket);
        if superseded > 0 {
            EnqueueOutcome::Superseded(superseded)
        } else {
            EnqueueOutcome::Queued
        }
    }

    /// Hand the oldest unclaimed tickets to a runner under a lease.
    pub(crate) fn claim(&mut self, runner_id: &str, max: usize) -> Vec<AuditTicket> {
        let now = Utc::now();
        self.expire_stale_claims(now);
        let mut claimed = Vec::new();
        for ticket in self.tickets.iter_mut() {
            if claimed.len() >= max.min(MAX_CLAIM_BATCH) {
                break;
            }
            if ticket.claimed_by.is_some() {
                continue;
            }
            ticket.claimed_by = Some(runner_id.to_string());
            ticket.claimed_at = Some(now);
            claimed.push(ticket.clone());
        }
        claimed
    }

    /// The ticket that authorizes scoring this head, removed so the next
    /// submission for the same head has nothing to match.
    pub(crate) fn take(&mut self, owner: &str, repo: &str, head_sha: &str) -> Option<AuditTicket> {
        let index = self
            .tickets
            .iter()
            .position(|ticket| ticket.is_for(owner, repo, head_sha))?;
        Some(self.tickets.remove(index))
    }

    pub(crate) fn tickets(&self) -> &[AuditTicket] {
        &self.tickets
    }

    /// Age a lease without waiting half an hour for it.
    #[cfg(test)]
    pub(crate) fn tickets_for_test(&mut self) -> &mut [AuditTicket] {
        &mut self.tickets
    }

    fn expire_stale_claims(&mut self, now: DateTime<Utc>) {
        let lease = Duration::seconds(CLAIM_LEASE_SECONDS);
        for ticket in self.tickets.iter_mut() {
            let expired = ticket
                .claimed_at
                .is_some_and(|claimed_at| now - claimed_at > lease);
            if expired {
                ticket.claimed_by = None;
                ticket.claimed_at = None;
            }
        }
    }
}

/// The one queue of the running forge. Push handling and the runner APIs live
/// in the same process, so a process-wide queue is the whole coordination.
pub(crate) fn queue() -> &'static Mutex<AuditQueue> {
    static QUEUE: OnceLock<Mutex<AuditQueue>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(AuditQueue::default()))
}

/// Why a pushed head gets no audit ticket, or `None` when it needs one.
pub(crate) fn skip_reason(branch: &str, has_open_pull: bool) -> Option<&'static str> {
    if UNAUDITED_BRANCH_NAMES.contains(&branch)
        || UNAUDITED_BRANCH_PREFIXES
            .iter()
            .any(|prefix| branch.starts_with(prefix))
    {
        return Some("import, preserve, archive and bot branches are not audited");
    }
    if branch == "main" || has_open_pull {
        return None;
    }
    Some("only the protected main and pull request heads are audited")
}

/// The base branch of the pull request this branch is the head of, when it is
/// the head of one still under review. A draft counts: its gate runs, so its
/// head is audited like any other.
///
/// The base is what the caller needs, not just the fact of a pull request: the
/// audit switch and the required checks of a pull request head are both read
/// from the base, never from the head, so a pull request cannot turn off a
/// gate the branch it targets requires.
fn open_pull_base(core: &ForgeCore, owner: &str, repo: &str, branch: &str) -> Option<String> {
    core.list_pull_requests(owner, repo, None)
        .ok()?
        .into_iter()
        .filter(|pull| {
            // The state is an evaluation (mergeable, blocked, queued,
            // ...), so "still under review" is everything that is not
            // finished.
            !matches!(
                pull.state,
                PullRequestState::Merged | PullRequestState::Closed
            )
        })
        .find(|pull| pull.head.ref_name == branch || pull.head.label == branch)
        .map(|pull| pull.base.ref_name)
}

/// What `.jeryu/ci.toml` says about auditing this repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuditPreference {
    /// No opt-out: no file, no `[jankurai]` table, or `audit = true`.
    On,
    /// `[jankurai] audit = false`: the repository does not use the audit.
    Off,
    /// The file exists but could not be read as the switch; the head is
    /// audited as it was before the switch existed, and the proof says why.
    Unreadable(String),
}

/// Read `[jankurai] audit` from `<rev>:.jeryu/ci.toml` in the bare repository.
///
/// `rev` is the base branch's ref for a pull request head and the pushed commit
/// for `main`: a repository states what it uses on the branch it protects, so a
/// pull request cannot opt itself out by adding the key.
pub(crate) fn audit_preference(
    git_bin: &str,
    bare: &std::path::Path,
    rev: &str,
) -> AuditPreference {
    let object = format!("{rev}:{CI_DECLARATION_PATH}");
    // No file (and no such rev) is the common case and says nothing: audit.
    let Some(size) = super::run_git_stdout(git_bin, Some(bare), &["cat-file", "-s", &object])
        .and_then(|size| size.trim().parse::<u64>().ok())
    else {
        return AuditPreference::On;
    };
    if size > MAX_CI_DECLARATION_BYTES {
        return AuditPreference::Unreadable(format!(
            "{CI_DECLARATION_PATH} is {size} bytes, larger than the {MAX_CI_DECLARATION_BYTES} \
             bytes the push path reads"
        ));
    }
    let Some(text) = super::run_git_stdout(git_bin, Some(bare), &["cat-file", "blob", &object])
    else {
        return AuditPreference::Unreadable(format!("{CI_DECLARATION_PATH} could not be read"));
    };
    let declaration: toml::Value = match toml::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            return AuditPreference::Unreadable(format!(
                "{CI_DECLARATION_PATH} is not valid TOML: {}",
                error.message()
            ));
        }
    };
    match declaration
        .get("jankurai")
        .and_then(|table| table.get("audit"))
    {
        None => AuditPreference::On,
        Some(toml::Value::Boolean(true)) => AuditPreference::On,
        Some(toml::Value::Boolean(false)) => AuditPreference::Off,
        Some(other) => AuditPreference::Unreadable(format!(
            "{CI_DECLARATION_PATH} sets [jankurai] audit to {other}, which is not a boolean"
        )),
    }
}

/// Whether the branch a change lands on requires `jankurai/proof` anyway: its
/// protection rule asks for it, or the site enforces the audit gate for this
/// repository. A repository may decline an advisory audit; it may not decline
/// one its own protected branch is gated on.
fn proof_required_for_base(core: &ForgeCore, owner: &str, repo: &str, base: &str) -> bool {
    if crate::web::audit_gate_enforced_for(owner, repo) {
        return true;
    }
    core.get_branch_protection(owner, repo, base)
        .map(|rule| {
            rule.require_jankurai_proof
                || rule
                    .required_status_checks
                    .iter()
                    .any(|name| name == super::JANKURAI_PROOF_CHECK)
        })
        .unwrap_or(false)
}

/// What a push decided about a head, for the caller that publishes the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PushAudit {
    /// No ticket and no check: this branch class is not audited at all.
    Skipped(&'static str),
    /// Already scored, or already waiting: exactly one audit per head.
    AlreadyCovered,
    /// The head needs an audit but no ticket could be recorded; the proof stays
    /// pending and says why, and nothing on the forge runs the audit instead.
    Unqueued(&'static str),
    Queued {
        ticket: AuditTicket,
        /// Something a reader of the proof has to be told about how this head
        /// came to be audited: that the repository's own opt-out was ignored
        /// because the base requires the proof, or that its `.jeryu/ci.toml`
        /// could not be read and the head was audited as it was before.
        note: Option<String>,
    },
}

/// Decide what a pushed head needs and record it. This runs on the push path,
/// so it does no clone, no checkout and no audit: at most a `merge-base` and
/// one bounded `.jeryu/ci.toml` blob read in the bare repository the push just
/// wrote.
pub(crate) fn plan_push_audit(
    core: &ForgeCore,
    git_bin: &str,
    bare: &std::path::Path,
    owner: &str,
    repo: &str,
    update: &RefUpdate,
) -> PushAudit {
    let Some(branch) = update.ref_name.strip_prefix("refs/heads/") else {
        return PushAudit::Skipped("only branch heads are audited");
    };
    let pull_base = open_pull_base(core, owner, repo, branch);
    if let Some(reason) = skip_reason(branch, pull_base.is_some()) {
        return PushAudit::Skipped(reason);
    }
    // What the repository says about auditing itself, read from the branch a
    // change lands on: the pushed `main`, or the base a pull request targets.
    // A pull request that adds the key opts out nothing.
    let base_branch = pull_base.unwrap_or_else(|| branch.to_string());
    let read_from = if base_branch == branch {
        update.new_oid.clone()
    } else {
        format!("refs/heads/{base_branch}")
    };
    let note = match audit_preference(git_bin, bare, &read_from) {
        AuditPreference::On => None,
        AuditPreference::Off => {
            if !proof_required_for_base(core, owner, repo, &base_branch) {
                return PushAudit::Skipped(AUDIT_OPT_OUT_REASON);
            }
            Some(format!(
                "`{}` requires `{}`, so the `[jankurai] audit = false` in \
                 {CI_DECLARATION_PATH} was ignored and this head was audited",
                base_branch,
                super::JANKURAI_PROOF_CHECK,
            ))
        }
        AuditPreference::Unreadable(reason) => Some(format!(
            "the audit switch could not be read ({reason}), so this head was audited"
        )),
    };
    // One audit per head, across the forge and `<repo>/required`: a head that
    // already carries a score, or already has a ticket, needs no second job.
    let scored = core
        .list_jankurai_scores(owner, repo, None, Some(&update.new_oid))
        .map(|scores| !scores.is_empty())
        .unwrap_or(false);
    if scored {
        return PushAudit::AlreadyCovered;
    }
    let ticket = match commit_base(git_bin, bare, branch, update) {
        Some(base) => AuditTicket::new(owner, repo, branch, &update.new_oid, &base),
        None => AuditTicket::whole_tree(owner, repo, branch, &update.new_oid),
    };
    let Ok(mut queue) = queue().lock() else {
        return PushAudit::Unqueued("the audit queue is unavailable");
    };
    match queue.enqueue(ticket.clone()) {
        EnqueueOutcome::AlreadyQueued => PushAudit::AlreadyCovered,
        EnqueueOutcome::Full => PushAudit::Unqueued("the audit queue is full"),
        EnqueueOutcome::Queued | EnqueueOutcome::Superseded(_) => {
            PushAudit::Queued { ticket, note }
        }
    }
}

/// The commit a diff audit runs against: the previous tip for main, the
/// merge-base with main for anything else. A first main, an orphan branch and
/// a branch unrelated to main have neither, and get a whole-tree audit instead
/// of a diff against nothing.
fn commit_base(
    git_bin: &str,
    bare: &std::path::Path,
    branch: &str,
    update: &RefUpdate,
) -> Option<String> {
    if branch == "main" {
        return (update.old_oid != super::ZERO_OID).then(|| update.old_oid.clone());
    }
    super::run_git_stdout(
        git_bin,
        Some(bare),
        &["merge-base", "refs/heads/main", &update.new_oid],
    )
    .map(|oid| oid.trim().to_string())
    .filter(|oid| !oid.is_empty())
}
