//! `/api/v1/repos/...` answers to `owner/name` as well as the UUID, on the
//! repo itself and on its sub-resources.

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
async fn repos_answer_to_owner_name_and_uuid_alike() {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "deploy".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    core.create_pull_request(
        "alice",
        "deploy",
        "alice",
        CreatePullRequestRequest {
            title: "change".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some(format!("{:040}", 1)),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    let token = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new(core).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let id = repo.id.to_string();

    let (status, by_uuid) = get(&router, &format!("/api/v1/repos/{id}"), &token).await;
    assert_eq!(status, StatusCode::OK);
    for uri in [
        "/api/v1/repos/alice/deploy",
        "/api/v1/repos/alice%2Fdeploy",
        "/api/v1/repos/Alice/Deploy",
    ] {
        let (status, body) = get(&router, uri, &token).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body, by_uuid, "{uri}");
    }

    let (status, by_uuid) = get(&router, &format!("/api/v1/repos/{id}/pulls"), &token).await;
    assert_eq!(status, StatusCode::OK);
    let (status, by_name) = get(&router, "/api/v1/repos/alice/deploy/pulls", &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(by_name, by_uuid);

    let (status, _) = get(&router, "/api/v1/repos/alice/deploy/pulls?limit=1", &token).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = get(&router, "/api/v1/repos/alice/missing", &token).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
