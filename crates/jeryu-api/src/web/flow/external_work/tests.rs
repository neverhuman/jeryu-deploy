use std::path::Path;

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::web::shift::tests::run_git;

fn request(
    method: Method,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(
            body.map(|value| axum::body::Body::from(value.to_string()))
                .unwrap_or_default(),
        )
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn snapshot() -> Value {
    json!({
        "schema_version":1,"family":"jeryu","source_repo":"jeryu/jeryu-deploy",
        "provider":"GitHub","source_url":"https://external.invalid/acme/project","default_branch":"main",
        "observed_at":"2026-10-30T12:00:00Z","task_observed_at":"2026-10-30T11:00:00Z","branch_observed_at":"2026-10-30T12:00:00Z",
        "branches":[{"name":"feature/topic","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","ahead_by":3,"behind_by":1,
            "comparison":"diverged","state":"review","task_id":"acme-api-01","url":"https://external.invalid/acme/project/tree/feature/topic",
            "pull_request":{"number":1,"title":"Contract","state":"OPEN","url":"https://external.invalid/acme/project/pull/1"}}],
        "phases":[{"id":"W0","label":"Foundations","description":"Contract freeze"}],
        "milestones":[{"id":"M0","label":"Contracts frozen","task_ids":["acme-api-01"]}],
        "tasks":[{"todo_id":"acme-api-01","task_id":"API-01","title":"Contract","phase_id":"W0","phase_source":"index",
            "parent_id":null,"milestone_ids":["M0"],"priority_rank":1,"critical_path":true,"state":"needs_human",
            "reason":"Owner must approve the contract freeze","dependencies":[],"pending_dependencies":[],"merged":false,
            "observed_efforts":1,"source_url":"https://external.invalid/source","pull_requests":[]}],"problems":[]
    })
}

#[test]
fn external_work_validates_explicit_plan_and_origin_evidence() {
    let valid: Snapshot = serde_json::from_value(snapshot()).unwrap();
    valid.validate("jeryu").unwrap();
    assert_eq!(valid.tasks[0].state, "needs_human");
    assert_eq!(valid.phases[0].id, "W0");
    assert_eq!(valid.milestones[0].id, "M0");
    assert_eq!(
        valid.branches[0].pull_request.as_ref().unwrap().state,
        "OPEN"
    );
    for (key, replacement) in [
        ("schema_version", json!(2)),
        ("family", json!("another")),
        ("source_url", json!("javascript:alert(1)")),
        ("observed_at", json!("yesterday")),
    ] {
        let mut data = snapshot();
        data[key] = replacement;
        assert!(
            serde_json::from_value::<Snapshot>(data)
                .unwrap()
                .validate("jeryu")
                .is_err(),
            "{key}"
        );
    }
}

#[test]
fn external_work_rejects_bad_links_or_parent_cycles_instead_of_hiding_them() {
    for mutation in ["url", "dependency", "phase", "cycle", "duplicate", "sha"] {
        let mut data = snapshot();
        match mutation {
            "url" => data["branches"][0]["url"] = json!("https://user:secret@external.invalid/"),
            "dependency" => data["tasks"][0]["dependencies"] = json!(["missing"]),
            "phase" => data["tasks"][0]["phase_id"] = json!("missing"),
            "cycle" => data["tasks"][0]["parent_id"] = json!("acme-api-01"),
            "duplicate" => {
                let task = data["tasks"][0].clone();
                data["tasks"].as_array_mut().unwrap().push(task);
            }
            "sha" => data["branches"][0]["head_sha"] = json!("unverified"),
            _ => unreachable!(),
        }
        assert!(
            serde_json::from_value::<Snapshot>(data)
                .unwrap()
                .validate("jeryu")
                .is_err(),
            "{mutation}"
        );
    }
}

#[test]
fn external_work_parent_graph_and_record_limits_are_bounded() {
    let mut data: Snapshot = serde_json::from_value(snapshot()).unwrap();
    let mut child = data.tasks[0].clone();
    child.todo_id = "acme-api-02".to_string();
    child.parent_id = Some("acme-api-01".to_string());
    data.tasks[0].parent_id = Some(child.todo_id.clone());
    data.tasks.push(child);
    assert!(data.validate("jeryu").unwrap_err().contains("cycle"));
    data.tasks = vec![data.tasks[0].clone(); 5001];
    assert!(data.validate("jeryu").unwrap_err().contains("budget"));
}

fn publish(root: &Path, content: &str) -> String {
    let work = root.join("work-todo");
    std::fs::create_dir_all(work.join("tracking")).unwrap();
    std::fs::write(work.join(SNAPSHOT), content).unwrap();
    run_git(&work, &["add", "tracking"]);
    run_git(&work, &["commit", "-q", "-m", "external snapshot"]);
    let bare = root.join("jeryu/jeryu-todo.git");
    run_git(&work, &["push", "-q", bare.to_str().unwrap(), "queue"]);
    run_git(&bare, &["rev-parse", "queue"])
}

#[tokio::test]
async fn external_work_flow_reads_snapshot_without_native_events_or_source_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let (router, admin) = crate::web::pipeline::tests::shift_forge(dir.path());
    let call = || {
        router.clone().oneshot(request(
            Method::GET,
            "/api/v1/flow?family=jeryu",
            &admin,
            None,
        ))
    };
    let before = body_json(call().await.unwrap()).await;
    assert_eq!(before["external_work"]["snapshots"], json!([]));
    let head = publish(dir.path(), &snapshot().to_string());
    let response = call().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let after = body_json(response).await;
    assert_eq!(
        after["external_work"]["snapshots"][0]["tasks"][0]["state"],
        "needs_human"
    );
    assert_eq!(
        after["external_work"]["snapshots"][0]["branches"][0]["ahead_by"],
        3
    );
    assert_eq!(before["lots"], after["lots"]);
    assert_eq!(before["work_reconciled"], after["work_reconciled"]);
    assert_eq!(
        run_git(
            &dir.path().join("jeryu/jeryu-todo.git"),
            &["rev-parse", "queue"]
        ),
        head
    );
    publish(dir.path(), "{bad json");
    let error = body_json(call().await.unwrap()).await;
    assert_eq!(error["external_work"]["snapshots"], json!([]));
    assert!(
        error["external_work"]["problems"][0]
            .as_str()
            .unwrap()
            .contains("JSON")
    );
}

#[test]
#[ignore = "requires an operator-supplied passive external-work snapshot"]
fn external_work_real_snapshot_preserves_tasks_phases_and_origin_branches() {
    let path = std::env::var_os("JERYU_EXTERNAL_WORK_FIXTURE").unwrap();
    let data: Snapshot = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    data.validate(&data.family).unwrap();
    assert!(!data.tasks.is_empty());
    assert!(!data.branches.is_empty());
    assert!(data.phases.iter().any(|phase| phase.id == "W0"));
    assert!(data.milestones.iter().any(|milestone| milestone.id == "M0"));
    println!(
        "validated {} tasks, {} branches, {} phases, {} milestones",
        data.tasks.len(),
        data.branches.len(),
        data.phases.len(),
        data.milestones.len()
    );
}
