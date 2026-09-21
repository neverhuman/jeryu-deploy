use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use jeryu_core::{AccountSummary, UserRole};
use serde_json::json;

use std::collections::BTreeMap;

use serde::Serialize;

use crate::web::WebState;
use crate::web::paging::{Page, PageInfo, PageParams, PageRejection};

use super::*;

/// `GET /api/v1/control-plane/status`: the snapshot with every collection in
/// it cut to the requested page. `page.collections` gives each one's total
/// and whether more rows remain; the `summary` counts stay whole.
pub(crate) async fn status(
    State(state): State<Arc<WebState>>,
    Query(paging): Query<PageParams>,
) -> Result<Json<ControlPlaneStatusPage>, PageRejection> {
    let page = paging.page()?;
    Ok(Json(paged_status(snapshot(&state), page)))
}

#[derive(Debug, Serialize)]
pub(crate) struct ControlPlaneStatusPage {
    #[serde(flatten)]
    pub snapshot: ControlPlaneSnapshot,
    pub page: ControlPlanePage,
}

#[derive(Debug, Serialize)]
pub(crate) struct ControlPlanePage {
    pub limit: usize,
    pub page: usize,
    pub collections: BTreeMap<&'static str, PageInfo>,
}

fn cut<T>(
    collections: &mut BTreeMap<&'static str, PageInfo>,
    name: &'static str,
    page: Page,
    items: &mut Vec<T>,
) {
    let (rows, info) = page.apply(std::mem::take(items));
    *items = rows;
    collections.insert(name, info);
}

pub(crate) fn paged_status(
    mut snapshot: ControlPlaneSnapshot,
    page: Page,
) -> ControlPlaneStatusPage {
    let mut collections = BTreeMap::new();
    let c = &mut collections;
    cut(c, "repos", page, &mut snapshot.repos);
    cut(c, "pull_requests", page, &mut snapshot.pull_requests);
    cut(c, "check_runs", page, &mut snapshot.check_runs);
    cut(c, "workflows", page, &mut snapshot.workflows);
    cut(c, "agent_runs", page, &mut snapshot.agent_runs);
    cut(c, "priorities", page, &mut snapshot.priorities);
    cut(c, "repo_graph.nodes", page, &mut snapshot.repo_graph.nodes);
    cut(c, "repo_graph.edges", page, &mut snapshot.repo_graph.edges);
    cut(
        c,
        "repo_graph.clusters",
        page,
        &mut snapshot.repo_graph.clusters,
    );
    cut(
        c,
        "repo_graph.insights",
        page,
        &mut snapshot.repo_graph.insights,
    );
    if let serde_json::Value::Array(workcells) = &mut snapshot.workcells {
        cut(c, "workcells", page, workcells);
    }
    ControlPlaneStatusPage {
        snapshot,
        page: ControlPlanePage {
            limit: page.limit,
            page: page.page,
            collections,
        },
    }
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

/// `POST /api/v1/runners/heartbeat`: a gate runner slot, the reviewer or a
/// background timer reports what it is doing. Open to the named reporters and
/// to forge admins (see [`GateRunnerStore::may_report`]).
pub(crate) async fn runner_heartbeat(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Json(heartbeat): Json<GateRunnerHeartbeat>,
) -> Response {
    let admin = account.role == UserRole::Admin;
    if !state.gate_runners.may_report(&account.login, admin) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "code": "permission_denied",
                "message": "this account may not report runner heartbeats: report as a forge admin or a JERYU_RUNNER_REPORTERS login",
            })),
        )
            .into_response();
    }
    let previous = state.gate_runners.previous(&heartbeat.runner_id);
    match state
        .gate_runners
        .record(heartbeat.clone(), &account.login, Utc::now())
    {
        Ok(accepted) => {
            crate::web::pipeline::emit::runner_heartbeat(&state, previous.as_ref(), &heartbeat);
            Json(accepted).into_response()
        }
        Err(reason) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "code": "invalid_input", "message": reason })),
        )
            .into_response(),
    }
}
