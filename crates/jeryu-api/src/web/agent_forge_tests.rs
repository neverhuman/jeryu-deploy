//! Exercise the API contracts consumed by scripts/agents/forge.py.
use std::path::Path;

use axum::http::{Request, StatusCode};
use jeryu_core::{CreateRepositoryRequest, ForgeCore, RepoAccessLevel, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{WebState, app};

async fn call(
    router: &axum::Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
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
async fn agent_forge_contracts_attribute_actions_and_refuse_non_writer() {
    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).unwrap();
    for login in ["owner", "agent-forge", "reader"] {
        core.create_account(login, "test-password", UserRole::User)
            .unwrap();
    }
    core.create_repository(
        "owner",
        CreateRepositoryRequest {
            name: "project".into(),
            private: true,
            description: None,
            default_branch: Some("main".into()),
        },
    )
    .unwrap();
    for (login, access) in [
        ("agent-forge", RepoAccessLevel::Write),
        ("reader", RepoAccessLevel::Read),
    ] {
        core.grant_repo_access("owner", login, "owner", "project", access)
            .unwrap();
    }
    let agent = core
        .create_personal_access_token("agent-forge", "agent", None)
        .unwrap()
        .secret;
    let reader = core
        .create_personal_access_token("reader", "reader", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        Path::new("."),
    );
    let create = json!({"title":"Change", "head":"topic", "base":"main", "draft":true,
        "head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "base_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"});
    let root = "/api/v3/repos/owner/project/pulls";
    let (status, _) = call(&router, "POST", root, &reader, create.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, opened) = call(&router, "POST", root, &agent, create.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    let number = opened["number"].as_u64().unwrap();
    let pull = format!("/api/v1/repos/owner/project/pulls/{number}");
    let stored = core.get_pull_request("owner", "project", number).unwrap();
    assert_eq!(stored.author, "agent-forge");
    assert!(stored.draft);
    let comment = json!({"verdict":"comment", "expected_head_sha":stored.head.sha,
        "body_markdown":"Agent comment", "thread_comments":[]});
    let (status, result) = call(&router, "POST", &format!("{pull}/reviews"), &agent, comment).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (status, detail) = call(&router, "GET", &pull, &agent, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(detail["reviews"].as_array().unwrap().iter().any(|review| {
        review["author"] == "agent-forge" && review["body_markdown"] == "Agent comment"
    }));
    for token in [&reader, &agent] {
        let (status, result) =
            call(&router, "POST", &format!("{pull}/ready"), token, json!({})).await;
        assert_eq!(
            status,
            if token == &reader {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::OK
            },
            "{result}"
        );
    }
    let (status, replacement) = call(&router, "POST", root, &agent, create).await;
    assert_eq!(status, StatusCode::CREATED, "{replacement}");
    let replacement = replacement["number"].as_u64().unwrap();
    for token in [&reader, &agent] {
        let (status, result) = call(
            &router,
            "POST",
            &format!("{pull}/close"),
            token,
            json!({"superseded_by": replacement}),
        )
        .await;
        assert_eq!(
            status,
            if token == &reader {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::OK
            },
            "{result}"
        );
    }
    assert!(
        !core
            .list_reviews("owner", "project", number)
            .unwrap()
            .iter()
            .any(|r| { r.state == jeryu_core::ReviewState::Approved })
    );
}
