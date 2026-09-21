//! The paged collections honour `limit` / `per_page` / `page`, say what they
//! applied, and refuse an out-of-range value instead of clamping it.

use std::path::Path;

use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::{CreatePullRequestRequest, CreateRepositoryRequest, ForgeCore, UserRole};
use serde_json::Value;
use tower::ServiceExt;

use super::{WebState, app};

async fn get(router: &axum::Router, uri: &str, token: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::GET)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(axum::body::Body::empty())
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
async fn big_collections_page_and_refuse_out_of_range_values() {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    let mut repo_id = String::new();
    for name in ["one", "two", "three"] {
        let repo = core
            .create_repository(
                "alice",
                CreateRepositoryRequest {
                    name: name.to_string(),
                    private: false,
                    description: None,
                    default_branch: Some("main".to_string()),
                },
            )
            .unwrap();
        repo_id = repo.id.to_string();
    }
    for n in 0..3 {
        core.create_pull_request(
            "alice",
            "three",
            "alice",
            CreatePullRequestRequest {
                title: format!("change {n}"),
                head: format!("feature-{n}"),
                base: "main".to_string(),
                head_sha: Some(format!("{n:040}")),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    }
    let token = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new(core).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );

    // Repositories: the rows are cut, `total` still counts every match.
    let (status, repos) = get(&router, "/api/v1/repos?limit=2", &token).await;
    assert_eq!(status, StatusCode::OK, "{repos}");
    assert_eq!(repos["repositories"].as_array().unwrap().len(), 2);
    assert_eq!(repos["total"], 3);
    assert_eq!(repos["page"]["limit"], 2);
    assert_eq!(repos["page"]["has_more"], true);
    let (_, rest) = get(&router, "/api/v1/repos?per_page=2&page=2", &token).await;
    assert_eq!(rest["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(rest["page"]["has_more"], false);
    let (_, all) = get(&router, "/api/v1/repos", &token).await;
    assert_eq!(all["page"]["limit"], 100);

    // Pull requests: paged, and `state` filters or is refused.
    let pulls = format!("/api/v1/repos/{repo_id}/pulls");
    let (status, page) = get(&router, &format!("{pulls}?limit=1&page=2"), &token).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["number"], 2);
    assert_eq!(page["total"], 3);
    assert_eq!(page["page"]["limit"], 1);
    let (_, merged) = get(&router, &format!("{pulls}?state=merged"), &token).await;
    assert_eq!(merged["total"], 0);
    let (_, open) = get(&router, &format!("{pulls}?state=open"), &token).await;
    assert_eq!(open["total"], 3);
    let (status, bad) = get(&router, &format!("{pulls}?state=bogus"), &token).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(bad["code"], "invalid_query");

    // The control-plane snapshot pages each collection it carries.
    let (status, snapshot) = get(&router, "/api/v1/control-plane/status?limit=1", &token).await;
    assert_eq!(status, StatusCode::OK, "{snapshot}");
    assert_eq!(snapshot["repos"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["page"]["limit"], 1);
    assert_eq!(snapshot["page"]["collections"]["repos"]["total"], 3);
    assert_eq!(snapshot["page"]["collections"]["repos"]["has_more"], true);
    assert!(snapshot["summary"].is_object());

    // Events echo the limit they applied and refuse one out of range.
    let (status, events) = get(&router, "/api/v1/events?limit=7", &token).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(events["limit"], 7);
    let (_, events) = get(&router, "/api/v1/events", &token).await;
    assert_eq!(events["limit"], 100);
    for uri in ["/api/v1/events?limit=0", "/api/v1/events?limit=501"] {
        let (status, body) = get(&router, uri, &token).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
        assert_eq!(body["code"], "events_invalid_query", "{uri}");
    }

    for uri in [
        "/api/v1/repos?limit=0",
        "/api/v1/repos?limit=501",
        "/api/v1/repos?page=0",
        "/api/v1/repos?per_page=x",
        "/api/v1/control-plane/status?limit=1000",
        &format!("{pulls}?limit=-1"),
    ] {
        let (status, body) = get(&router, uri, &token).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
        assert_eq!(body["code"], "invalid_page_parameter", "{uri}");
    }
}
