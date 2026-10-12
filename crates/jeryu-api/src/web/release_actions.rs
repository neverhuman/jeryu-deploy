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
//! * `POST /api/v1/release-actions/:id/claim` takes one ask, once: a second
//!   claim is refused, so two pollers never run the same step.
//! * `POST /api/v1/release-actions/:id/result` says how it ended, with the log's
//!   tail.
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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::*;

/// Asks kept at once, oldest finished forgotten first.
const MAX_ACTIONS: usize = 256;
/// Characters of a result's log kept; the full log stays on the release host.
const MAX_LOG_TAIL: usize = 8_000;

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

#[derive(Debug, Clone, Default)]
pub(crate) struct ReleaseActionStore {
    actions: Arc<Mutex<BTreeMap<String, ReleaseAction>>>,
    next: Arc<std::sync::atomic::AtomicU64>,
}

impl ReleaseActionStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ReleaseAction>> {
        self.actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records an ask, or answers the open ask for the same step. The `bool`
    /// says whether this call recorded it.
    fn record(&self, new: NewAction, by: &str, now: String) -> (ReleaseAction, bool) {
        let mut actions = self.lock();
        if let Some(existing) = actions.values().find(|a| a.open() && a.same_step(&new)) {
            return (existing.clone(), false);
        }
        if actions.len() >= MAX_ACTIONS {
            // Forget the oldest finished ask; an open one is never dropped.
            if let Some(oldest) = actions
                .values()
                .filter(|a| !a.open())
                .min_by(|a, b| a.requested_at.cmp(&b.requested_at))
                .map(|a| a.id.clone())
            {
                actions.remove(&oldest);
            }
        }
        // Time-ordered and unique within this forge's life, which is the
        // store's life.
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let id = format!("ra-{}-{n}", now.replace([':', '-', '.'], ""));
        let action = ReleaseAction {
            id: id.clone(),
            action: new.action,
            repo: new.repo,
            release: new.release,
            head: new.head,
            state: ActionState::Pending,
            requested_by: by.to_string(),
            requested_at: now,
            claimed_by: None,
            claimed_at: None,
            finished_at: None,
            message: None,
            log_tail: None,
        };
        actions.insert(id, action.clone());
        (action, true)
    }

    pub(crate) fn all(&self) -> Vec<ReleaseAction> {
        self.lock().values().cloned().collect()
    }

    fn claim(&self, id: &str, by: &str, now: String) -> Result<ReleaseAction, ActionState> {
        let mut actions = self.lock();
        let Some(action) = actions.get_mut(id) else {
            return Err(ActionState::Failed);
        };
        if action.state != ActionState::Pending {
            return Err(action.state);
        }
        action.state = ActionState::Claimed;
        action.claimed_by = Some(by.to_string());
        action.claimed_at = Some(now);
        Ok(action.clone())
    }

    fn finish(
        &self,
        id: &str,
        result: ActionResult,
        now: String,
    ) -> Result<ReleaseAction, Option<ActionState>> {
        let mut actions = self.lock();
        let Some(action) = actions.get_mut(id) else {
            return Err(None);
        };
        if action.state != ActionState::Claimed {
            return Err(Some(action.state));
        }
        action.state = if result.ok {
            ActionState::Succeeded
        } else {
            ActionState::Failed
        };
        action.finished_at = Some(now);
        action.message = result.message.map(|m| m.chars().take(500).collect());
        action.log_tail = result.log_tail.map(|tail| {
            let skip = tail.chars().count().saturating_sub(MAX_LOG_TAIL);
            tail.chars().skip(skip).collect()
        });
        Ok(action.clone())
    }
}

/// The label a release host's poller heartbeat carries
/// (`release-action.sh poll`, `POST /api/v1/runners/heartbeat`).
pub(crate) const POLLER_LABEL: &str = "release-actions";

/// What the attention inbox needs to know about release actions: whether a
/// release host is polling for them now (no poller, no button: an ask nobody
/// reads would only wait), and which asks are still open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReleaseQueue {
    pub(crate) poller: bool,
    pub(crate) open: Vec<ReleaseAction>,
}

impl ReleaseQueue {
    pub(crate) fn snapshot(state: &WebState, now: chrono::DateTime<chrono::Utc>) -> Self {
        let poller = state.gate_runners.snapshot().iter().any(|record| {
            record
                .heartbeat
                .labels
                .iter()
                .any(|label| label == POLLER_LABEL)
                && super::control_plane::is_online(record, now)
        });
        let open = state
            .release_actions
            .all()
            .into_iter()
            .filter(ReleaseAction::open)
            .collect();
        Self { poller, open }
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
    pub(crate) fn open_auto_pin(&self) -> Option<&ReleaseAction> {
        self.open.iter().find(|a| a.action != ActionKind::Deploy)
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

fn is_repo_slug(repo: &str) -> bool {
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
fn was_staged(state: &WebState, repo: &str, release: &str) -> bool {
    state
        .events
        .query(&super::pipeline::EventsQuery {
            kind: Some("release.staged".to_string()),
            repo: Some(repo.to_string()),
            limit: Some(200),
            ..Default::default()
        })
        .unwrap_or_default()
        .iter()
        .any(|event| {
            event
                .detail
                .as_ref()
                .and_then(|detail| detail["release"].as_str())
                == Some(release)
        })
}

/// `POST /api/v1/release-actions`: ask the release host for one step.
///
/// 201 with the new ask, or 200 with the open ask for the same step: a second
/// click while the first is waiting or running asks nothing new.
pub(super) async fn request(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Json(new): Json<NewAction>,
) -> AxumResponse {
    if !super::auth::can_record_deployments(&account) {
        return not_deployer();
    }
    if let Err(message) = validate(&new) {
        return invalid(message);
    }
    if let Some(release) = new.release.as_deref()
        && !was_staged(&state, &new.repo, release)
    {
        return api_error(
            StatusCode::CONFLICT,
            "not_staged",
            "the forge has no record of this release being staged for this repository",
        );
    }
    let (action, created) = state
        .release_actions
        .record(new, &account.login, server_time());
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    (status, Json(action)).into_response()
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct ListQuery {
    /// `pending` (the default) or `all`.
    state: Option<String>,
}

/// `GET /api/v1/release-actions`: what the release host reads each tick.
pub(super) async fn list(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<ListQuery>,
) -> AxumResponse {
    if !super::auth::can_record_deployments(&account) {
        return not_deployer();
    }
    let pending = query.state.as_deref().unwrap_or("pending") != "all";
    let actions: Vec<ReleaseAction> = state
        .release_actions
        .all()
        .into_iter()
        .filter(|a| !pending || a.state == ActionState::Pending)
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
    match state.release_actions.finish(&id, result, server_time()) {
        Ok(action) => Json(action).into_response(),
        Err(None) => api_error(StatusCode::NOT_FOUND, "not_found", "no such release action"),
        Err(Some(_)) => api_error(
            StatusCode::CONFLICT,
            "not_claimed",
            "only a claimed release action takes a result",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deploy(release: &str) -> NewAction {
        NewAction {
            action: ActionKind::Deploy,
            repo: "acme/forge-deploy".to_string(),
            release: Some(release.to_string()),
            head: None,
        }
    }

    const REL: &str = "prod-20261012T171717Z-f1b1fb4-unsigned";

    #[test]
    fn a_second_ask_for_the_same_step_is_the_first() {
        let store = ReleaseActionStore::default();
        let (first, created) = store.record(deploy(REL), "operator", "2026-10-12T18:00:00Z".into());
        assert!(created);
        let (again, created) = store.record(deploy(REL), "operator", "2026-10-12T18:00:05Z".into());
        assert!(!created);
        assert_eq!(again.id, first.id);
        assert_eq!(store.all().len(), 1);
    }

    #[test]
    fn an_ask_is_claimed_once_and_finished_once() {
        let store = ReleaseActionStore::default();
        let (ask, _) = store.record(deploy(REL), "operator", "t0".into());
        let claimed = store.claim(&ask.id, "release-host", "t1".into()).unwrap();
        assert_eq!(claimed.state, ActionState::Claimed);
        assert_eq!(
            store.claim(&ask.id, "another-host", "t2".into()),
            Err(ActionState::Claimed)
        );
        let done = store
            .finish(
                &ask.id,
                ActionResult {
                    ok: true,
                    message: Some("live".into()),
                    log_tail: None,
                },
                "t3".into(),
            )
            .unwrap();
        assert_eq!(done.state, ActionState::Succeeded);
        assert!(!store.all().iter().any(ReleaseAction::open));
        // Finished, so a new click is a new ask.
        let (next, created) = store.record(deploy(REL), "operator", "t4".into());
        assert!(created);
        assert_ne!(next.id, ask.id);
    }

    #[test]
    fn only_a_claimed_ask_takes_a_result() {
        let store = ReleaseActionStore::default();
        let (ask, _) = store.record(deploy(REL), "operator", "t0".into());
        let result = ActionResult {
            ok: false,
            message: None,
            log_tail: None,
        };
        assert_eq!(
            store.finish(&ask.id, result.clone(), "t1".into()),
            Err(Some(ActionState::Pending))
        );
        assert_eq!(store.finish("ra-missing", result, "t1".into()), Err(None));
    }

    #[test]
    fn each_action_takes_exactly_its_own_argument() {
        assert!(validate(&deploy(REL)).is_ok());
        assert!(validate(&deploy("prod-1; rm -rf ~")).is_err());
        assert!(validate(&deploy("../../etc/passwd")).is_err());
        let mut bad_repo = deploy(REL);
        bad_repo.repo = "acme/forge deploy".into();
        assert!(validate(&bad_repo).is_err());
        let pin = NewAction {
            action: ActionKind::AutoPin,
            repo: "acme/forge-deploy".into(),
            release: None,
            head: None,
        };
        assert!(validate(&pin).is_ok());
        let retry = |head: &str| NewAction {
            action: ActionKind::AutoPinRetry,
            repo: "acme/forge-deploy".into(),
            release: None,
            head: Some(head.into()),
        };
        assert!(validate(&retry(&"a".repeat(40))).is_ok());
        assert!(validate(&retry("abc123")).is_err());
        assert!(validate(&retry(&"A".repeat(40))).is_err());
    }

    #[test]
    fn a_full_store_forgets_a_finished_ask_never_an_open_one() {
        let store = ReleaseActionStore::default();
        let (done, _) = store.record(deploy(REL), "operator", "2026-01-01T00:00:00Z".into());
        store.claim(&done.id, "host", "t".into()).unwrap();
        store
            .finish(
                &done.id,
                ActionResult {
                    ok: true,
                    message: None,
                    log_tail: None,
                },
                "t".into(),
            )
            .unwrap();
        for i in 1..MAX_ACTIONS {
            let release = format!("prod-20261012T{:06}Z-f1b1fb4-unsigned", i);
            store.record(
                deploy(&release),
                "operator",
                format!("2026-10-12T00:00:{i:05}Z"),
            );
        }
        assert_eq!(store.all().len(), MAX_ACTIONS);
        store.record(
            deploy("prod-20261013T000000Z-f1b1fb4-unsigned"),
            "operator",
            "z".into(),
        );
        assert!(!store.all().iter().any(|a| a.id == done.id));
        assert!(store.all().iter().all(|a| a.open()));
    }
}
