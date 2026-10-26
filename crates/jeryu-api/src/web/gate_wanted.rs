//! "Gate wanted": the forge saying a head can be gated now, so the runner does
//! not wait for its next timer tick to find out.
//!
//! A gate slot used to learn about work only when its own timer fired, and the
//! wake helper in front of it (`ops/pr-gate/bin/pr-gate-wake.sh` in
//! jeryu-ci-runner) could only guess by re-listing every gated repository's
//! open pull requests, the merge queue and the pending re-gates. Both cost up
//! to a tick of latency before every gate -- twice for a pull request that is
//! queued after its base moved, because the queue commit is a second head.
//!
//! The forge already knows the moment a gateable head appears, so it records it
//! here: a push that moved a branch ([`crate::git_transport`]), a merge-queue
//! commit it just built ([`super::merge_queue`]) and a re-gate someone asked
//! for ([`super::regate`]). `GET /api/v1/gate-wanted?after=<seq>&wait=<secs>`
//! hands those signals to the runner, blocking up to `wait` seconds for the
//! next one so a push reaches an idle slot in about as long as the push took.
//!
//! The signal is a hint, never an instruction: it carries no decision about
//! whether the head is gateable, and the runner still chooses, claims and gates
//! heads exactly as it does from its timer. That is what keeps the timer a
//! sufficient fallback -- a signal nobody read costs today's latency and
//! nothing else -- and why the store is in memory and bounded like
//! [`super::merge_attempts`]: a restart drops the backlog, and the slot timers
//! cover it.
//!
//! It needs no new credential. The runner reads it with the identity it already
//! heartbeats and gates with, and sees a signal only for a repository it can
//! read.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use super::*;

/// Signals kept at once, oldest forgotten first. A runner that fell this far
/// behind is better served by its timer than by a replay, and the count it
/// needs -- "how many heads appeared" -- is capped by its slot count anyway.
const MAX_SIGNALS: usize = 512;
/// Longest a reader may hold a long poll open. Above the slot timer's period,
/// so a waiting runner is woken by the signal rather than by its own timeout,
/// and below the wake unit's `TimeoutStartSec`.
const MAX_WAIT: Duration = Duration::from_secs(60);

/// Why a head could be gated now. The runner logs it and otherwise only counts
/// the signals, so these are names for a person reading the log.
pub(crate) const SOURCE_PUSH: &str = "push";
pub(crate) const SOURCE_QUEUE: &str = "queue";
pub(crate) const SOURCE_REGATE: &str = "regate";
pub(crate) const SOURCE_OK_TO_TEST: &str = "ok_to_test";

/// One "this head can be gated now". `seq` is assigned by the store and is
/// what a reader asks for more than.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct GateWanted {
    pub(crate) seq: u64,
    /// `owner/name`.
    pub(crate) repo: String,
    /// The ref whose tip moved, when the signal came from a push. A queue
    /// commit and a re-gate name a pull request instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ref_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) number: Option<u64>,
    pub(crate) sha: String,
    pub(crate) source: String,
    pub(crate) at: String,
}

/// A signal as a recorder describes it, before the store dates and numbers it.
pub(crate) struct NewGateWanted {
    pub(crate) repo: String,
    pub(crate) ref_name: Option<String>,
    pub(crate) number: Option<u64>,
    pub(crate) sha: String,
    pub(crate) source: &'static str,
}

#[derive(Debug, Default)]
struct Signals {
    /// Oldest first, so a reader walks forward from its `after`.
    kept: VecDeque<GateWanted>,
    /// Newest `seq` handed out. Never reused, so a reader that sees it go
    /// backwards knows the forge restarted.
    last_seq: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct GateWantedStore {
    signals: Arc<Mutex<Signals>>,
    /// Woken on every record, so a long poll returns when a head appears
    /// instead of when its timeout expires.
    arrived: Arc<Notify>,
}

impl GateWantedStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, Signals> {
        self.signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records a signal and wakes every reader waiting for one. Best-effort by
    /// construction: it cannot fail, and nothing it is called from waits on it.
    pub(crate) fn record(&self, new: NewGateWanted) {
        {
            let mut signals = self.lock();
            signals.last_seq += 1;
            let seq = signals.last_seq;
            signals.kept.push_back(GateWanted {
                seq,
                repo: new.repo,
                ref_name: new.ref_name,
                number: new.number,
                sha: new.sha,
                source: new.source.to_string(),
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
    pub(super) fn since(&self, after: u64) -> (u64, Vec<GateWanted>) {
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
pub(super) struct GateWantedQuery {
    /// Only signals above this sequence. Absent means "whatever is kept".
    after: Option<u64>,
    /// Seconds to hold the request open while there is nothing to report,
    /// capped at [`MAX_WAIT`]. Absent or `0` answers at once.
    wait: Option<u64>,
}

/// `GET /api/v1/gate-wanted`: the heads that became gateable, long-polled.
///
/// Answers as soon as there is a signal above `after` the caller may read, and
/// otherwise holds the request for up to `wait` seconds and then answers with
/// an empty list and the current sequence. Reading it changes nothing: two
/// runners asking the same question get the same answer, and a runner that
/// misses an answer entirely falls back on its timer.
pub(super) async fn list(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<GateWantedQuery>,
) -> AxumResponse {
    let after = query.after.unwrap_or(0);
    let wait = Duration::from_secs(query.wait.unwrap_or(0)).min(MAX_WAIT);
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        // Registered before the read: a signal recorded between the two wakes
        // this future rather than being waited out.
        let arrival = state.gate_wanted.arrived.notified();
        let (seq, fresh) = state.gate_wanted.since(after);
        let visible: Vec<GateWanted> = fresh
            .into_iter()
            .filter(|signal| readable(&state, &account, &signal.repo))
            .collect();
        if !visible.is_empty() || seq < after || tokio::time::Instant::now() >= deadline {
            return Json(json!({ "seq": seq, "signals": visible })).into_response();
        }
        if tokio::time::timeout_at(deadline, arrival).await.is_err() {
            // Nothing arrived in time: the current sequence, so the next poll
            // does not re-walk what this one already skipped.
            return Json(json!({ "seq": state.gate_wanted.seq(), "signals": [] })).into_response();
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

/// One signal per branch a push moved. `repo` may carry the `.git` suffix the
/// git transport was called with; the runner compares it with the repository
/// names it discovered, which do not.
pub(crate) fn record_push(
    state: &WebState,
    owner: &str,
    repo: &str,
    updates: &[crate::ci_bridge::RefUpdate],
) {
    let repo = repo.trim_end_matches(".git");
    for update in updates {
        // A contribution's branch is gated only after a maintainer says
        // ok-to-test, which records its own signal.
        if update
            .ref_name
            .strip_prefix("refs/heads/")
            .is_some_and(|branch| crate::github_pull_import::contrib_number(branch).is_some())
        {
            continue;
        }
        state.gate_wanted.record(NewGateWanted {
            repo: format!("{owner}/{repo}"),
            ref_name: Some(update.ref_name.clone()),
            number: None,
            sha: update.new_oid.clone(),
            source: SOURCE_PUSH,
        });
    }
}

/// The merge queue just built `sha` for `repo#number`: a head that exists only
/// because the forge made it, so nothing else would tell the runner about it.
pub(crate) fn record_queue_commit(state: &WebState, repo: &str, number: u64, sha: &str) {
    state.gate_wanted.record(NewGateWanted {
        repo: repo.to_string(),
        ref_name: None,
        number: Some(number),
        sha: sha.to_string(),
        source: SOURCE_QUEUE,
    });
}

/// Someone asked for `repo#number`'s head to be gated again. The ask itself
/// lives in [`super::regate`]; this only says an idle slot has work now.
pub(crate) fn record_regate(state: &WebState, repo: &str, number: u64, sha: &str) {
    state.gate_wanted.record(NewGateWanted {
        repo: repo.to_string(),
        ref_name: None,
        number: Some(number),
        sha: sha.to_string(),
        source: SOURCE_REGATE,
    });
}

/// A maintainer said ok-to-test for `repo#number`'s head: the gate may start.
pub(crate) fn record_ok_to_test(state: &WebState, repo: &str, number: u64, sha: &str) {
    state.gate_wanted.record(NewGateWanted {
        repo: repo.to_string(),
        ref_name: None,
        number: Some(number),
        sha: sha.to_string(),
        source: SOURCE_OK_TO_TEST,
    });
}
