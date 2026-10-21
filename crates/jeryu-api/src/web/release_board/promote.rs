//! A stage's Promote control runs the deploy. The collector names the staged
//! release a stage would take (`promote.deploy`); this file turns that into a
//! release action the repository's release poller executes, and tells the
//! board what the control may do right now.
//!
//! * `GET /api/v1/release-board/{family}` carries `promote.run` on every
//!   stage that names a deploy: whether Promote is available (and if not,
//!   why), the confirm text, and the state of the newest ask for that release.
//! * `POST /api/v1/release-board/{family}/promote` `{lane, stage}` queues the
//!   deploy. The release and repository come from the stored snapshot, never
//!   from the request, and the ask is the same one the attention inbox's
//!   Deploy button makes, so both share one open ask per release.
//! * `GET /api/v1/release-board/{family}/promote/{id}` is the ask with its
//!   redacted log tail, which the board cell links to.
//!
//! Every route needs a deployer, the bar of the release-actions routes: Promote
//! is a claim about production. A press is written to the event log as
//! `release.promote_requested`, naming who pressed it, before anything is queued.

use super::*;
use crate::web::release_actions::{self, ActionState, ReleaseAction, ReleaseQueue};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromoteRequest {
    lane: String,
    stage: String,
}

fn not_deployer() -> Response {
    refusal(
        StatusCode::FORBIDDEN,
        "permission_denied",
        "Promote needs a deployer: an admin named in JERYU_DEPLOYERS",
    )
}

/// Why Promote cannot run for `deploy` now, or `None` when it can.
fn disabled_reason(
    state: &WebState,
    queue: &ReleaseQueue,
    stage: &Stage,
    deploy: &PromoteDeploy,
) -> Option<String> {
    if let Some(open) = queue.open_deploy(&deploy.repo, &deploy.release) {
        return Some(open.progress());
    }
    if release_actions::latest_deploy(state, &deploy.repo, &deploy.release)
        .is_some_and(|a| a.state == ActionState::Succeeded)
    {
        return Some("The release host already deployed this release.".to_string());
    }
    if stage.ships.as_ref().is_some_and(Vec::is_empty) {
        return Some("Nothing to ship: this stage already runs what would be promoted.".into());
    }
    if !release_actions::was_staged(state, &deploy.repo, &deploy.release) {
        return Some(format!(
            "The forge has no record of {} being staged for {}.",
            deploy.release, deploy.repo
        ));
    }
    if !queue.polls(&deploy.repo) {
        return Some(format!(
            "No release poller is running for {}: start the release-actions poller on its deploy host.",
            deploy.repo
        ));
    }
    None
}

fn confirm_text(lane: &Lane, stage: &Stage, deploy: &PromoteDeploy) -> String {
    let targets: Vec<&str> = stage.targets.iter().map(|t| t.name.as_str()).collect();
    let on = if targets.is_empty() {
        String::new()
    } else {
        format!(" ({})", targets.join(", "))
    };
    format!(
        "Deploy {} to {} {}{on}? The release host for {} runs deploy-release.sh for it now.",
        deploy.release, lane.name, stage.name, deploy.repo
    )
}

fn action_json(family: &str, action: &ReleaseAction) -> Value {
    json!({
        "id": action.id,
        "state": action.state,
        "requested_by": action.requested_by,
        "requested_at": action.requested_at,
        "finished_at": action.finished_at,
        "message": action.message,
        "log_href": format!("/api/v1/release-board/{family}/promote/{}", action.id),
    })
}

fn run_json(
    state: &WebState,
    queue: &ReleaseQueue,
    family: &str,
    lane: &Lane,
    stage: &Stage,
    deploy: &PromoteDeploy,
) -> Value {
    let reason = disabled_reason(state, queue, stage, deploy);
    let latest = release_actions::latest_deploy(state, &deploy.repo, &deploy.release);
    json!({
        "available": reason.is_none(),
        "reason": reason,
        "confirm": confirm_text(lane, stage, deploy),
        "action": latest.as_ref().map(|a| action_json(family, a)),
    })
}

/// The board as JSON with `promote.run` filled in on every stage that names a deploy.
pub(super) fn with_runs(state: &WebState, board: ReleaseBoard) -> Value {
    let queue = ReleaseQueue::snapshot(state, Utc::now());
    let mut value = serde_json::to_value(&board).unwrap_or(Value::Null);
    for (l, lane) in board.lanes.iter().enumerate() {
        for (s, stage) in lane.stages.iter().enumerate() {
            let Some(deploy) = stage.promote.as_ref().and_then(|p| p.deploy.as_ref()) else {
                continue;
            };
            let run = run_json(state, &queue, &board.family, lane, stage, deploy);
            if let Some(promote) = value
                .pointer_mut(&format!("/lanes/{l}/stages/{s}/promote"))
                .and_then(Value::as_object_mut)
            {
                promote.insert("run".to_string(), run);
            }
        }
    }
    value
}

/// `POST /api/v1/release-board/{family}/promote`: queue the deploy a stage's
/// Promote names. 201 with the new ask, 200 with the open or finished ask for
/// the same release (a second press is the first), 409 `promote_disabled`
/// with the reason when it cannot run.
pub(crate) async fn promote(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Path(family): Path<String>,
    Json(request): Json<PromoteRequest>,
) -> Response {
    if !crate::web::auth::can_record_deployments(&account) {
        return not_deployer();
    }
    let Some(board) = state.release_boards.get(&family) else {
        return refusal(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no release board for {family:?}"),
        );
    };
    let found = board
        .lanes
        .iter()
        .find(|lane| lane.id == request.lane)
        .and_then(|lane| {
            lane.stages
                .iter()
                .find(|stage| stage.id == request.stage)
                .map(|stage| (lane, stage))
        });
    let Some((lane, stage)) = found else {
        return refusal(
            StatusCode::NOT_FOUND,
            "not_found",
            "the board has no such lane and stage",
        );
    };
    let Some(deploy) = stage.promote.as_ref().and_then(|p| p.deploy.as_ref()) else {
        return refusal(
            StatusCode::CONFLICT,
            "promote_disabled",
            "this stage names no staged release to deploy; run its promote command by hand",
        );
    };
    let queue = ReleaseQueue::snapshot(&state, Utc::now());
    let latest = release_actions::latest_deploy(&state, &deploy.repo, &deploy.release);
    if let Some(done) = latest
        .as_ref()
        .filter(|a| a.open() || a.state == ActionState::Succeeded)
    {
        return (
            StatusCode::OK,
            Json(json!({ "created": false, "action": action_json(&board.family, done) })),
        )
            .into_response();
    }
    if let Some(reason) = disabled_reason(&state, &queue, stage, deploy) {
        return refusal(StatusCode::CONFLICT, "promote_disabled", reason);
    }
    // Written first: a deploy is never queued without a record of who asked.
    let mut event = crate::web::pipeline::NewEvent::forge(
        "release.promote_requested",
        format!(
            "{} promoted {} {} ({})",
            account.login, lane.name, stage.name, deploy.release
        ),
    );
    event.actor = Some(account.login.clone());
    event.family = Some(board.family.clone());
    event.repo = Some(deploy.repo.clone());
    event.detail = Some(json!({
        "release": deploy.release,
        "lane": lane.id,
        "stage": stage.id,
        "requested_by": account.login,
    }));
    if let Err(error) =
        crate::web::pipeline::record(&state, crate::web::pipeline::trust::FORGE, event)
    {
        return refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            "audit_failed",
            format!("the press could not be recorded, so nothing was queued: {error}"),
        );
    }
    let ask = release_actions::deploy_ask(&deploy.repo, &deploy.release);
    match release_actions::record_ask(&state, ask, &account.login) {
        Ok((action, created)) => (
            if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            Json(json!({ "created": created, "action": action_json(&board.family, &action) })),
        )
            .into_response(),
        Err(message) => refusal(StatusCode::TOO_MANY_REQUESTS, "release_queue_full", message),
    }
}

/// `GET /api/v1/release-board/{family}/promote/{id}`: one deploy ask with its
/// redacted log tail, only for a release this board's stages name.
pub(crate) async fn promote_action(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Path((family, id)): Path<(String, String)>,
) -> Response {
    if !crate::web::auth::can_record_deployments(&account) {
        return not_deployer();
    }
    let Some(board) = state.release_boards.get(&family) else {
        return refusal(StatusCode::NOT_FOUND, "not_found", "no such release board");
    };
    let named = |a: &ReleaseAction| {
        board.lanes.iter().flat_map(|l| &l.stages).any(|s| {
            s.promote
                .as_ref()
                .and_then(|p| p.deploy.as_ref())
                .is_some_and(|d| {
                    d.repo == a.repo && a.release.as_deref() == Some(d.release.as_str())
                })
        })
    };
    let action = state
        .release_actions
        .all()
        .into_iter()
        .find(|a| a.id == id && named(a));
    match action {
        Some(action) => Json(action).into_response(),
        None => refusal(StatusCode::NOT_FOUND, "not_found", "no such promote action"),
    }
}

#[cfg(test)]
#[path = "promote_tests.rs"]
mod tests;
