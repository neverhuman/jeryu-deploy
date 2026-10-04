//! Re-gate requests: asking the gate runner to gate a head it already gated.
//!
//! The runner reuses a terminal result whose inputs are identical and holds a
//! failure until an input changes, which is what keeps a green head from being
//! re-proved on every tick. The cost was that a head which failed for a reason
//! outside its own sources -- a dependency fetch that timed out, a gate host
//! that lost its compile cache -- had no way back to the gate: the attention
//! inbox's `pr_checks_failing` said "fix or re-run it" while nothing in the web
//! or this API re-ran anything, so the only re-gate was a new push or a shell
//! on the gate host.
//!
//! `POST /api/v1/repos/:id/pulls/:number/regate` records the ask against the
//! pull request's current head. `GET /api/v1/gate-regate?state=pending` is what
//! the runner reads each tick (`ops/pr-gate/bin/pr-gate-runner.sh` in
//! jeryu-ci-runner): a request still naming an open pull request's current head
//! makes the runner gate that head again although a result for it exists. The
//! runner records which `requested_at` it has honoured, so one request buys one
//! re-gate and asking again records a newer one. A request whose head has since
//! moved is no longer pending: the new head is gated because it is new.
//!
//! The store is in memory, like [`super::merge_attempts`]: a request lives for
//! one runner tick (a minute), and a forge that restarts in between should be
//! asked again rather than replay an ask from before the restart.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::*;

/// Requests kept at once, oldest forgotten first. A gate host runs a handful of
/// slots, so this is far above anything a queue of re-gates can need.
const MAX_REQUESTS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegateRequest {
    /// `owner/name`.
    pub(crate) repo: String,
    pub(crate) number: u64,
    /// The head the re-gate was asked for. The runner honours the request only
    /// for this commit.
    pub(crate) head_sha: String,
    pub(crate) requested_at: String,
    pub(crate) requested_by: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RegateStore {
    requests: Arc<Mutex<BTreeMap<(String, u64), RegateRequest>>>,
}

impl RegateStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, u64), RegateRequest>> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records the newest request for a pull request, replacing any earlier one:
    /// a second ask is the same ask, with a newer time for the runner to tell
    /// apart from the one it has already honoured.
    pub(crate) fn record(&self, request: RegateRequest) {
        let mut requests = self.lock();
        let key = (request.repo.clone(), request.number);
        if !requests.contains_key(&key) && requests.len() >= MAX_REQUESTS {
            // Forget the oldest ask rather than refuse to learn a new one.
            if let Some(oldest) = requests
                .iter()
                .min_by(|a, b| a.1.requested_at.cmp(&b.1.requested_at))
                .map(|(key, _)| key.clone())
            {
                requests.remove(&oldest);
            }
        }
        requests.insert(key, request);
    }

    pub(super) fn all(&self) -> Vec<RegateRequest> {
        self.lock().values().cloned().collect()
    }
}

fn can_write(state: &WebState, account: &AccountSummary, owner: &str, repo: &str) -> bool {
    account.role == UserRole::Admin || state.core.user_can_write_repo(&account.login, owner, repo)
}

/// `POST /api/v1/repos/:id/pulls/:number/regate`: gate this head again.
///
/// Idempotent in effect, not in record: every call records the current time, so
/// a second call after the runner honoured the first asks for another re-gate.
pub(super) async fn request(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
) -> AxumResponse {
    let Some(repo) = super::repositories::find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    if !can_write(&state, &account, &repo.owner, &repo.name) {
        return api_error(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "re-gating needs write access to the repository",
        );
    }
    let Ok(pr) = state.core.get_pull_request(&repo.owner, &repo.name, number) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "pull request not found");
    };
    if !super::merge_queue::is_open_pull(&pr) {
        return api_error(
            StatusCode::CONFLICT,
            "not_open",
            "only an open pull request has a head to gate again",
        );
    }
    let request = RegateRequest {
        repo: format!("{}/{}", repo.owner, repo.name),
        number,
        head_sha: pr.head.sha.clone(),
        requested_at: server_time(),
        requested_by: account.login.clone(),
    };
    state.regate_requests.record(request.clone());
    (StatusCode::ACCEPTED, Json(request)).into_response()
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct RegateListQuery {
    /// `pending` (the default) or `all`.
    state: Option<String>,
}

/// `GET /api/v1/gate-regate`: what the gate runner reads each tick.
///
/// `pending` keeps only a request the runner can still act on: the pull request
/// is open and its head is the one the request names. `all` keeps every request
/// the forge remembers, for a person asking what was requested.
pub(super) async fn list(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<RegateListQuery>,
) -> AxumResponse {
    let pending = query.state.as_deref().unwrap_or("pending") != "all";
    let requests: Vec<RegateRequest> = state
        .regate_requests
        .all()
        .into_iter()
        .filter(|request| {
            let Some((owner, name)) = request.repo.split_once('/') else {
                return false;
            };
            if account.role != UserRole::Admin
                && !state.core.user_can_read_repo(&account.login, owner, name)
            {
                return false;
            }
            if !pending {
                return true;
            }
            state
                .core
                .get_pull_request(owner, name, request.number)
                .is_ok_and(|pr| {
                    super::merge_queue::is_open_pull(&pr) && pr.head.sha == request.head_sha
                })
        })
        .collect();
    Json(json!({ "requests": requests })).into_response()
}
