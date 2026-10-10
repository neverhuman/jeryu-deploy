//! `/api/v1/flow/layout`: admin-only station layout from runner heartbeats.

use std::path::Path;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use chrono::Utc;
use jeryu_core::{ForgeCore, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::web::control_plane::{GateRunnerHeartbeat, GateRunnerResult, RunnerCode};

use super::super::{WebState, app};

async fn get(router: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn layout_is_admin_only_and_rejects_unknown_query_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).unwrap();
    core.create_account("admin", "admin-password", UserRole::Admin)
        .unwrap();
    core.create_account("reader", "reader-password", UserRole::User)
        .unwrap();
    let admin = core
        .create_personal_access_token("admin", "test", None)
        .unwrap()
        .secret;
    let reader = core
        .create_personal_access_token("reader", "test", None)
        .unwrap()
        .secret;
    let state = WebState::new(core)
        .with_reporters(&["ci-bot"], "rel-bot")
        .with_auth(true, false, false);
    state
        .gate_runners
        .record(
            GateRunnerHeartbeat {
                runner_id: "build-1/slot0".to_string(),
                host: "build-1".to_string(),
                slot: 0,
                labels: Vec::new(),
                interval_seconds: None,
                current: None,
                last: Some(GateRunnerResult {
                    repo: "jeryu/jeryu-deploy".to_string(),
                    pr: Some(11),
                    sha: "1234567".to_string(),
                    recipe: "just required".to_string(),
                    conclusion: "success".to_string(),
                    target: None,
                    reason: None,
                    seconds: 15,
                    finished_at: Utc::now(),
                }),
                code: Some(RunnerCode {
                    repo: "jeryu/jeryu-deploy".to_string(),
                    commit: "abcdef1".to_string(),
                    version: Some("1.0.0".to_string()),
                    installed_at: None,
                }),
                tools: Vec::new(),
            },
            "ci-bot",
            Utc::now(),
        )
        .unwrap();
    let router = app(state, Path::new("/tmp/jeryu-no-spa"));

    let (status, body) = get(&router, &reader, "/api/v1/flow/layout").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], json!("permission_denied"));

    let (status, body) = get(&router, &admin, "/api/v1/flow/layout?family=jeryu").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], json!("invalid_query"));

    let (status, body) = get(&router, &admin, "/api/v1/flow/layout").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["schemaVersion"], json!("jeryu.flow.layout/v1"));
    let gate = body["stations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|station| station["id"] == "gate")
        .unwrap();
    assert_eq!(gate["tools"][0]["runnerId"], json!("build-1/slot0"));
    assert_eq!(gate["tools"][0]["online"], json!("online"));
    assert_eq!(gate["tools"][0]["code"]["commit"], json!("abcdef1"));
}
