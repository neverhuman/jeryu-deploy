//! Release actions: a person asks the forge for a release step, and the release
//! host performs it.
//!
//! The attention inbox's "a release is staged and waiting for a deploy" used to
//! end in a shell line to copy to the release host, where it had to be run from
//! the right checkout with the site settings loaded. The step itself lives on
//! the release host (it holds the build-host ssh access and the deploy token),
//! so the forge cannot run it. What the forge can do is record the ask, the way
//! [`super::regate`] records a re-gate for the gate runner:
//!
//! * `POST /api/v1/release-actions` records one ask: `deploy` (a staged release
//!   by id), `auto_pin` (start the auto-pin timer's service now) or
//!   `auto_pin_retry` (forget auto-pin's failure count for one head, then start
//!   it). The attention row's button sends it.
//! * `GET /api/v1/release-actions?state=pending` is what the release host's
//!   `release-action.sh poll` reads every half minute
//!   (`scripts/release/release-action.sh`).
//! * `POST /api/v1/release-actions/poller` advertises a repository-specific poller,
//!   with the same deployer authentication as execution.
//! * `POST /api/v1/release-actions/:id/cancel` cancels a pending ask atomically.
//! * `POST /api/v1/release-actions/:id/claim` takes one ask, once: a second
//!   claim is refused, so two pollers never run the same step.
//! * `POST /api/v1/release-actions/:id/result` says how it ended, with the log's
//!   redacted tail.
//!
//! The forge never sends a command. An ask names one of three actions and its
//! validated argument, and the host maps each to a fixed script of its own; a
//! forge that was compromised could at most ask for a staged release to be
//! deployed. Every route needs a deployer (an admin named in `JERYU_DEPLOYERS`,
//! [`super::auth::can_record_deployments`]), the same bar as recording a
//! deployment: asking for one is the same claim about production.
//!
//! The store is in memory, like [`super::regate`]. A deploy restarts the forge,
//! so the result of the deploy that restarted it is lost with it; the
//! deployment record `deploy-release.sh` writes is the durable answer, and a
//! restarted forge replays no ask from before it started.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use super::*;

/// Asks kept at once, oldest finished forgotten first.
const MAX_ACTIONS: usize = 256;
/// Characters of a result's log kept; the full log stays on the release host.
const MAX_LOG_TAIL: usize = 8_000;
const PENDING_TTL_MINUTES: i64 = 15;
const CLAIMED_TTL_MINUTES: i64 = 90;
const POLLER_TTL_SECONDS: i64 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionKind {
    /// Switch production to a staged release (`deploy-release.sh <release>`).
    Deploy,
    /// Start `jeryu-auto-pin.service` now instead of on its next tick.
    AutoPin,
    /// Forget auto-pin's failures for one jeryu-web head, then start it.
    AutoPinRetry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionState {
    Pending,
    Claimed,
    Succeeded,
    Failed,
    Expired,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReleaseAction {
    pub(crate) id: String,
    pub(crate) action: ActionKind,
    /// `owner/name` of the repository the step is for.
    pub(crate) repo: String,
    /// The staged release a `deploy` switches to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) release: Option<String>,
    /// The 40-hex head an `auto_pin_retry` forgets the failures of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) head: Option<String>,
    pub(crate) state: ActionState,
    pub(crate) requested_by: String,
    pub(crate) requested_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) claimed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) claimed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) finished_at: Option<String>,
    /// One line on how it ended, from the release host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) log_tail: Option<String>,
}

impl ReleaseAction {
    pub(crate) fn open(&self) -> bool {
        matches!(self.state, ActionState::Pending | ActionState::Claimed)
    }

    /// The same step: one open ask per step, so a second click is the first.
    fn same_step(&self, other: &NewAction) -> bool {
        self.action == other.action
            && self.repo == other.repo
            && self.release == other.release
            && self.head == other.head
    }
}

#[path = "release_actions/store.rs"]
mod store;
pub(crate) use store::ReleaseActionStore;

/// What the attention inbox needs to know about release actions: whether a
/// release host is polling for them now (no poller, no button: an ask nobody
/// reads would only wait), and which asks are still open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReleaseQueue {
    pub(crate) poller_repos: Vec<String>,
    pub(crate) open: Vec<ReleaseAction>,
}

impl ReleaseQueue {
    pub(crate) fn snapshot(state: &WebState, now: chrono::DateTime<chrono::Utc>) -> Self {
        let poller_repos = state.release_actions.poller_repos(now);
        let open = state
            .release_actions
            .all_at(&now.to_rfc3339())
            .into_iter()
            .filter(ReleaseAction::open)
            .collect();
        Self { poller_repos, open }
    }

    pub(crate) fn polls(&self, repo: &str) -> bool {
        self.poller_repos.iter().any(|polled| polled == repo)
    }

    /// The open ask to deploy this staged release, if one is waiting or running.
    pub(crate) fn open_deploy(&self, repo: &str, release: &str) -> Option<&ReleaseAction> {
        self.open.iter().find(|a| {
            a.action == ActionKind::Deploy
                && a.repo == repo
                && a.release.as_deref() == Some(release)
        })
    }

    /// The open auto-pin ask of either kind, if one is waiting or running.
    pub(crate) fn open_auto_pin(&self, repo: &str) -> Option<&ReleaseAction> {
        self.open
            .iter()
            .find(|a| a.action != ActionKind::Deploy && a.repo == repo)
    }
}

impl ReleaseAction {
    /// "asked by operator at 18:02Z, waiting for the release host" — for a
    /// reason line, so a reader knows the click was taken.
    pub(crate) fn progress(&self) -> String {
        let when = self.requested_at.get(11..16).unwrap_or(&self.requested_at);
        match self.state {
            ActionState::Claimed => format!(
                "Asked by {} at {when}Z; the release host is running it now.",
                self.requested_by
            ),
            _ => format!(
                "Asked by {} at {when}Z; waiting for the release host to pick it up (it checks \
                 every 30 seconds).",
                self.requested_by
            ),
        }
    }
}

/// What a person asks for. Exactly the fields an action takes are allowed.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NewAction {
    action: ActionKind,
    repo: String,
    #[serde(default)]
    release: Option<String>,
    #[serde(default)]
    head: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ActionResult {
    ok: bool,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    log_tail: Option<String>,
}

fn not_deployer() -> AxumResponse {
    api_error(
        StatusCode::FORBIDDEN,
        "permission_denied",
        "release actions need a deployer: an admin named in JERYU_DEPLOYERS",
    )
}

fn invalid(message: &str) -> AxumResponse {
    api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", message)
}

pub(crate) fn is_repo_slug(repo: &str) -> bool {
    repo.split_once('/').is_some_and(|(owner, name)| {
        let part = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        };
        part(owner) && part(name)
    })
}

/// What a release id looks like (`stage-release.sh`):
/// `prod-<8 digits>T<6 digits>Z-<7..40 lowercase hex>-unsigned`. The release
/// host checks it again before it runs anything.
pub(crate) fn is_release_id(release: &str) -> bool {
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    let Some(rest) = release.strip_prefix("prod-") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix("-unsigned") else {
        return false;
    };
    let Some((stamp, sha)) = rest.split_once('-') else {
        return false;
    };
    let Some((date, time)) = stamp.split_once('T') else {
        return false;
    };
    digits(date, 8)
        && time.strip_suffix('Z').is_some_and(|t| digits(t, 6))
        && (7..=40).contains(&sha.len())
        && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_full_sha(head: &str) -> bool {
    head.len() == 40
        && head
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// The fields an action takes, and nothing else: a check here is the first of
/// two (the release host repeats it), not the only one.
fn validate(new: &NewAction) -> Result<(), &'static str> {
    if !is_repo_slug(&new.repo) {
        return Err("repo must be owner/name");
    }
    match new.action {
        ActionKind::Deploy => {
            if new.head.is_some() {
                return Err("deploy takes a release, not a head");
            }
            match new.release.as_deref() {
                Some(release) if is_release_id(release) => Ok(()),
                _ => Err("deploy needs the staged release id (prod-<UTC stamp>-<sha>-unsigned)"),
            }
        }
        ActionKind::AutoPin => match (&new.release, &new.head) {
            (None, None) => Ok(()),
            _ => Err("auto_pin takes neither a release nor a head"),
        },
        ActionKind::AutoPinRetry => {
            if new.release.is_some() {
                return Err("auto_pin_retry takes a head, not a release");
            }
            match new.head.as_deref() {
                Some(head) if is_full_sha(head) => Ok(()),
                _ => Err("auto_pin_retry needs the 40-hex head auto-pin gave up on"),
            }
        }
    }
}

/// Whether the release host said it staged this release for this repository:
/// a button can only deploy what auto-stage reported, not any well-formed id.
pub(crate) fn was_staged(state: &WebState, repo: &str, release: &str) -> bool {
    state
        .events
        .release_precondition_reporters(repo, "release.staged", Some(release), None, false)
        .unwrap_or_default()
        .iter()
        .any(|reporter| {
            state
                .core
                .get_account(reporter)
                .is_ok_and(|account| super::auth::can_record_deployments(&account))
        })
}

fn retry_was_requested(state: &WebState, new: &NewAction) -> bool {
    state
        .events
        .release_precondition_reporters(
            &new.repo,
            "pin.bump_failed",
            None,
            new.head.as_deref(),
            true,
        )
        .unwrap_or_default()
        .iter()
        .any(|reporter| {
            state
                .core
                .get_account(reporter)
                .is_ok_and(|account| super::auth::can_record_deployments(&account))
        })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PollerCheckIn {
    repo: String,
}

/// Only a deployer can advertise a live release poller, for its own repository.
pub(super) async fn check_in(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Json(mut check_in): Json<PollerCheckIn>,
) -> AxumResponse {
    if !super::auth::can_record_deployments(&account) {
        return not_deployer();
    }
    if !is_repo_slug(&check_in.repo) {
        return invalid("repo must be owner/name");
    }
    let Some(repo) = super::repositories::find_repo(&state, &check_in.repo) else {
        return invalid("repo must name a hosted repository");
    };
    check_in.repo = repo.full_name;
    match state
        .release_actions
        .check_in(&check_in.repo, &account.login, chrono::Utc::now())
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(message) => api_error(StatusCode::TOO_MANY_REQUESTS, "release_queue_full", message),
    }
}

/// Cancelling only a pending ask cannot race an already claimed release step.
pub(super) async fn cancel(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    if !super::auth::can_record_deployments(&account) {
        return not_deployer();
    }
    match state.release_actions.cancel(&id, server_time()) {
        Ok(action) => Json(action).into_response(),
        Err(None) => api_error(StatusCode::NOT_FOUND, "not_found", "no such release action"),
        Err(Some(_)) => api_error(
            StatusCode::CONFLICT,
            "already_claimed",
            "only a pending ask can be cancelled; cancellation cannot stop a release step",
        ),
    }
}

/// `POST /api/v1/release-actions`: ask the release host for one step.
///
/// 201 with the new ask, or 200 with the open ask for the same step: a second
/// click while the first is waiting or running asks nothing new.
pub(super) async fn request(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Json(mut new): Json<NewAction>,
) -> AxumResponse {
    if !super::auth::can_record_deployments(&account) {
        return not_deployer();
    }
    if let Err(message) = validate(&new) {
        return invalid(message);
    }
    let Some(repo) = super::repositories::find_repo(&state, &new.repo) else {
        return invalid("repo must name a hosted repository");
    };
    new.repo = repo.full_name;
    if let Some(release) = new.release.as_deref()
        && !was_staged(&state, &new.repo, release)
    {
        return api_error(
            StatusCode::CONFLICT,
            "not_staged",
            "the forge has no record of this release being staged for this repository",
        );
    }
    if new.action == ActionKind::AutoPinRetry && !retry_was_requested(&state, &new) {
        return api_error(
            StatusCode::CONFLICT,
            "not_retryable",
            "auto-pin has not requested a human retry for this repository and head",
        );
    }
    let (action, created) = match state
        .release_actions
        .record(new, &account.login, server_time())
    {
        Ok(record) => record,
        Err(message) => {
            return api_error(StatusCode::TOO_MANY_REQUESTS, "release_queue_full", message);
        }
    };
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    (status, Json(action)).into_response()
}

/// The ask a board's Promote makes: deploy one staged release of `repo`.
pub(crate) fn deploy_ask(repo: &str, release: &str) -> NewAction {
    NewAction {
        action: ActionKind::Deploy,
        repo: repo.to_string(),
        release: Some(release.to_string()),
        head: None,
    }
}

/// Records `new` for a person already checked by the caller; see
/// [`ReleaseActionStore::record`].
pub(crate) fn record_ask(
    state: &WebState,
    new: NewAction,
    by: &str,
) -> Result<(ReleaseAction, bool), &'static str> {
    state.release_actions.record(new, by, server_time())
}

/// The newest ask to deploy this release, open or finished.
pub(crate) fn latest_deploy(state: &WebState, repo: &str, release: &str) -> Option<ReleaseAction> {
    state
        .release_actions
        .all_at(&server_time())
        .into_iter()
        .filter(|a| {
            a.action == ActionKind::Deploy
                && a.repo == repo
                && a.release.as_deref() == Some(release)
        })
        .max_by(|a, b| a.requested_at.cmp(&b.requested_at).then(a.id.cmp(&b.id)))
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ListQuery {
    /// `pending` (the default) or `all`.
    state: Option<String>,
}

impl super::strict_query::StrictFields for ListQuery {
    const KEYS: &'static [&'static str] = &["state"];
    fn check_values(&self) -> Result<(), String> {
        super::strict_query::one_of(
            "state",
            self.state.as_deref().unwrap_or("pending"),
            &["pending", "all"],
        )
    }
}

/// `GET /api/v1/release-actions`: what the release host reads each tick.
pub(super) async fn list(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    query: Result<super::strict_query::StrictQuery<ListQuery>, super::strict_query::QueryRefused>,
) -> AxumResponse {
    if !super::auth::can_record_deployments(&account) {
        return not_deployer();
    }
    let query = match query {
        Ok(super::strict_query::StrictQuery(query)) => query,
        Err(refused) => return refused.into_response(),
    };
    let pending = query.state.as_deref().unwrap_or("pending") != "all";
    let repos = state
        .release_actions
        .poller_repos_for(&account.login, chrono::Utc::now());
    let actions: Vec<ReleaseAction> = state
        .release_actions
        .all_at(&server_time())
        .into_iter()
        .filter(|a| !pending || (a.state == ActionState::Pending && repos.contains(&a.repo)))
        .collect();
    Json(json!({ "actions": actions })).into_response()
}

/// `POST /api/v1/release-actions/:id/claim`: take one pending ask, once.
pub(super) async fn claim(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    if !super::auth::can_record_deployments(&account) {
        return not_deployer();
    }
    let Some(action) = state.release_actions.all().into_iter().find(|a| a.id == id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "no such release action");
    };
    if !state
        .release_actions
        .poller_repos_for(&account.login, chrono::Utc::now())
        .contains(&action.repo)
    {
        return api_error(
            StatusCode::CONFLICT,
            "not_claimed",
            "register this authenticated poller for the action's repository before claiming it",
        );
    }
    match state
        .release_actions
        .claim(&id, &account.login, server_time())
    {
        Ok(action) => Json(action).into_response(),
        Err(ActionState::Failed) if !state.release_actions.all().iter().any(|a| a.id == id) => {
            api_error(StatusCode::NOT_FOUND, "not_found", "no such release action")
        }
        Err(_) => api_error(
            StatusCode::CONFLICT,
            "already_claimed",
            "this release action is no longer pending",
        ),
    }
}

/// `POST /api/v1/release-actions/:id/result`: how a claimed ask ended.
pub(super) async fn result(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
    Json(result): Json<ActionResult>,
) -> AxumResponse {
    if !super::auth::can_record_deployments(&account) {
        return not_deployer();
    }
    match state
        .release_actions
        .finish(&id, &account.login, result, server_time())
    {
        Ok(action) => Json(action).into_response(),
        Err(None) => api_error(StatusCode::NOT_FOUND, "not_found", "no such release action"),
        Err(Some(_)) => api_error(
            StatusCode::CONFLICT,
            "not_claimed",
            "only the deployer that claimed this release action may report its result",
        ),
    }
}

#[cfg(test)]
#[path = "release_actions/tests.rs"]
mod tests;
