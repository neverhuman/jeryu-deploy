//! "Review wanted": the forge saying a pull request head can be reviewed now,
//! so the reviewer does not wait for its next timer tick to find out.
//!
//! The security/correctness reviewer (`ops/pr-redteam` in jeryu-ci-runner) used
//! to learn about a head only from its own timers: a five-minute review pass and
//! a thirty-second helper that re-listed every open pull request to guess what
//! was new. A review does not depend on the gate's result, so that latency was
//! paid twice -- once before the review started, once before the computed result
//! was published when the gate went green.
//!
//! The forge already knows the moment a reviewable head appears, so it records
//! it here: a pull request opened ([`record_opened`]) and a push that moved an
//! open pull request's head ([`record_push`]).
//! `GET /api/v1/review-wanted?after=<seq>&wait=<secs>` hands those signals to
//! the reviewer, blocking up to `wait` seconds for the next one so a push
//! reaches an idle reviewer slot in about as long as the push took.
//!
//! The signal is a hint, never an instruction, exactly as [`super::gate_wanted`]
//! is for the gate: it carries no decision about whether the head may be
//! reviewed, approved or landed, and the reviewer still applies its own skip,
//! budget, gate and publication rules to every head it picks up. That is what
//! keeps the reviewer's timer a sufficient fallback -- a signal nobody read
//! costs today's latency and nothing else -- and why the store is in memory and
//! bounded: a restart drops the backlog, and the timer covers it.
//!
//! It needs no new credential. The reviewer reads it with the identity it
//! already reviews with, and sees a signal only for a repository it can read.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use super::*;

/// Signals kept at once, oldest forgotten first. A reviewer that fell this far
/// behind is better served by its pass than by a replay.
const MAX_SIGNALS: usize = 512;
/// Longest a reader may hold a long poll open. Below the reviewer's own
/// fallback period, so a waiting reviewer is woken by the signal rather than by
/// its own timeout.
const MAX_WAIT: Duration = Duration::from_secs(60);

/// A pull request was opened at this head.
pub(crate) const KIND_OPENED: &str = "pull.opened";
/// An open pull request's head moved to this commit.
pub(crate) const KIND_SYNCHRONIZED: &str = "pull.synchronized";

/// One "this head can be reviewed now". `seq` is assigned by the store and is
/// what a reader asks for more than.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReviewWanted {
    pub(crate) seq: u64,
    /// `owner/name`.
    pub(crate) repo: String,
    pub(crate) number: u64,
    pub(crate) head_sha: String,
    pub(crate) base_ref: String,
    /// The reviewer skips a draft unless it was told not to; it is carried so a
    /// reader can drop one without a pull request read.
    pub(crate) draft: bool,
    /// [`KIND_OPENED`] or [`KIND_SYNCHRONIZED`].
    pub(crate) kind: String,
    pub(crate) at: String,
}

/// A signal as a recorder describes it, before the store dates and numbers it.
pub(crate) struct NewReviewWanted {
    pub(crate) repo: String,
    pub(crate) number: u64,
    pub(crate) head_sha: String,
    pub(crate) base_ref: String,
    pub(crate) draft: bool,
    pub(crate) kind: &'static str,
}

#[derive(Debug, Default)]
struct Signals {
    /// Oldest first, so a reader walks forward from its `after`.
    kept: VecDeque<ReviewWanted>,
    /// Newest `seq` handed out. Never reused, so a reader that sees it go
    /// backwards knows the forge restarted.
    last_seq: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ReviewWantedStore {
    signals: Arc<Mutex<Signals>>,
    /// Woken on every record, so a long poll returns when a head appears
    /// instead of when its timeout expires.
    arrived: Arc<Notify>,
}

impl ReviewWantedStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, Signals> {
        self.signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records a signal and wakes every reader waiting for one. Best-effort by
    /// construction: it cannot fail, and nothing it is called from waits on it.
    pub(crate) fn record(&self, new: NewReviewWanted) {
        {
            let mut signals = self.lock();
            signals.last_seq += 1;
            let seq = signals.last_seq;
            signals.kept.push_back(ReviewWanted {
                seq,
                repo: new.repo,
                number: new.number,
                head_sha: new.head_sha,
                base_ref: new.base_ref,
                draft: new.draft,
                kind: new.kind.to_string(),
                at: server_time(),
            });
            while signals.kept.len() > MAX_SIGNALS {
                signals.kept.pop_front();
            }
        }
        self.arrived.notify_waiters();
    }

    /// The newest sequence handed out, which a reader with nothing to catch up
    /// on asks for more than next time.
    pub(crate) fn seq(&self) -> u64 {
        self.lock().last_seq
    }

    /// Kept signals newer than `after`, oldest first, with the newest sequence
    /// handed out. A reader whose `after` is ahead of the store (the forge
    /// restarted) is told the smaller sequence and given nothing, so it resyncs
    /// without treating the whole backlog as new.
    pub(super) fn since(&self, after: u64) -> (u64, Vec<ReviewWanted>) {
        let signals = self.lock();
        let fresh = signals
            .kept
            .iter()
            .filter(|signal| signal.seq > after)
            .cloned()
            .collect();
        (signals.last_seq, fresh)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct ReviewWantedQuery {
    /// Only signals above this sequence. Absent means "whatever is kept".
    after: Option<u64>,
    /// Seconds to hold the request open while there is nothing to report,
    /// capped at [`MAX_WAIT`]. Absent or `0` answers at once.
    wait: Option<u64>,
}

/// `GET /api/v1/review-wanted`: the heads that became reviewable, long-polled.
///
/// Answers as soon as there is a signal above `after` the caller may read, and
/// otherwise holds the request for up to `wait` seconds and then answers with
/// an empty list and the current sequence. Reading it changes nothing: two
/// reviewers asking the same question get the same answer, and a reviewer that
/// misses an answer entirely falls back on its pass.
pub(super) async fn list(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<ReviewWantedQuery>,
) -> AxumResponse {
    let after = query.after.unwrap_or(0);
    let wait = Duration::from_secs(query.wait.unwrap_or(0)).min(MAX_WAIT);
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        // Registered before the read: a signal recorded between the two wakes
        // this future rather than being waited out.
        let arrival = state.review_wanted.arrived.notified();
        let (seq, fresh) = state.review_wanted.since(after);
        let visible: Vec<ReviewWanted> = fresh
            .into_iter()
            .filter(|signal| readable(&state, &account, &signal.repo))
            .collect();
        if !visible.is_empty() || seq < after || tokio::time::Instant::now() >= deadline {
            return Json(json!({ "seq": seq, "signals": visible })).into_response();
        }
        if tokio::time::timeout_at(deadline, arrival).await.is_err() {
            // Nothing arrived in time: the current sequence, so the next poll
            // does not re-walk what this one already skipped.
            return Json(json!({ "seq": state.review_wanted.seq(), "signals": [] }))
                .into_response();
        }
    }
}

fn readable(state: &WebState, account: &AccountSummary, repo: &str) -> bool {
    if account.role == UserRole::Admin {
        return true;
    }
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    state.core.user_can_read_repo(&account.login, owner, name)
}

/// A pull request was just opened: its head is reviewable now, whoever opened
/// it and however they did.
pub(crate) fn record_opened(state: &WebState, pr: &jeryu_core::PullRequest) {
    record(state, pr, KIND_OPENED);
}

/// One signal per open pull request whose head a push moved to a commit this
/// push delivered. `repo` may carry the `.git` suffix the git transport was
/// called with; the reviewer compares it with the repository names it
/// discovered, which do not.
///
/// Called after the push->pull-request head refresh, so a moved head already
/// carries the pushed commit. A pull request whose head the push did not move
/// records nothing.
pub(crate) fn record_push(
    state: &WebState,
    owner: &str,
    repo: &str,
    updates: &[crate::ci_bridge::RefUpdate],
) {
    let repo = repo.trim_end_matches(".git");
    let moved: Vec<&str> = updates
        .iter()
        .filter(|update| update.ref_name.starts_with("refs/heads/"))
        .map(|update| update.new_oid.as_str())
        .filter(|oid| !oid.is_empty() && !oid.bytes().all(|byte| byte == b'0'))
        .collect();
    if moved.is_empty() {
        return;
    }
    let Ok(pulls) = state.core.list_pull_requests(owner, repo, None) else {
        return;
    };
    for pr in pulls {
        // Open as core means it: every state but a closed or merged one. A
        // pull request's state moves with its checks and approvals
        // (`Mergeable`, `BlockedByChecks`), and all of those are reviewable.
        if pr.merged
            || matches!(
                pr.state,
                jeryu_core::PullRequestState::Closed | jeryu_core::PullRequestState::Merged
            )
        {
            continue;
        }
        if moved.contains(&pr.head.sha.as_str()) {
            record(state, &pr, KIND_SYNCHRONIZED);
        }
    }
}

fn record(state: &WebState, pr: &jeryu_core::PullRequest, kind: &'static str) {
    state.review_wanted.record(NewReviewWanted {
        repo: format!("{}/{}", pr.owner, pr.repo),
        number: pr.number,
        head_sha: pr.head.sha.clone(),
        base_ref: pr.base.ref_name.clone(),
        draft: pr.draft,
        kind,
    });
}
