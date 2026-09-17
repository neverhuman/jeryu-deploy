use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use jeryu_core::AccountSummary;
use serde_json::json;

use crate::web::WebState;

use super::*;

pub(crate) async fn status(State(state): State<Arc<WebState>>) -> Json<ControlPlaneSnapshot> {
    Json(snapshot(&state))
}

pub(crate) async fn priorities(
    State(state): State<Arc<WebState>>,
    Query(query): Query<PriorityQuery>,
) -> Json<Vec<PriorityInsight>> {
    let mut priorities = snapshot(&state).priorities;
    if let Some(limit) = query.limit {
        priorities.truncate(limit.max(1));
    }
    Json(priorities)
}

pub(crate) async fn repo_graph(
    State(state): State<Arc<WebState>>,
    Query(query): Query<RepoGraphQuery>,
) -> Json<RepoGraphResponse> {
    Json(repo_graph_response(&state, Some(query)))
}

pub(crate) async fn artifacts_latest(
    State(state): State<Arc<WebState>>,
) -> Json<ArtifactLatestResponse> {
    Json(artifacts(&state))
}

pub(crate) async fn runners(State(state): State<Arc<WebState>>) -> Json<RunnerFabricResponse> {
    Json(runner_fabric(&state))
}

/// `POST /api/v1/runners/heartbeat`: a gate runner slot reports what it is doing.
pub(crate) async fn runner_heartbeat(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Json(heartbeat): Json<GateRunnerHeartbeat>,
) -> Response {
    if !state.gate_runners.may_report(&account.login) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "code": "permission_denied",
                "message": "this account may not report runner heartbeats (JERYU_RUNNER_REPORTERS)",
            })),
        )
            .into_response();
    }
    match state
        .gate_runners
        .record(heartbeat, &account.login, Utc::now())
    {
        Ok(accepted) => Json(accepted).into_response(),
        Err(reason) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "code": "invalid_input", "message": reason })),
        )
            .into_response(),
    }
}
