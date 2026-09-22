//! A public repository is readable without an account over the web API; a
//! private one, a write, and routes outside the repository page are not.

use std::path::Path;

use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::{CreateRepositoryRequest, ForgeCore, UserRole};
use serde_json::Value;
use tower::ServiceExt;

use super::{WebState, app};

async fn call(
    router: &axum::Router,
    method: HttpMethod,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(axum::body::Body::empty()).unwrap())
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

fn forge() -> (axum::Router, ForgeCore, String, String) {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    let mut ids = Vec::new();
    for (name, private) in [("open", false), ("closed", true)] {
        let repo = core
            .create_repository(
                "alice",
                CreateRepositoryRequest {
                    name: name.to_string(),
                    private,
                    description: None,
                    default_branch: Some("main".to_string()),
                },
            )
            .unwrap();
        ids.push(repo.id.to_string());
    }
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let closed = ids.pop().unwrap();
    let open = ids.pop().unwrap();
    (router, core, open, closed)
}

#[tokio::test]
async fn anonymous_visitor_reads_a_public_repository() {
    let (router, _core, open, _closed) = forge();
    for uri in [
        format!("/api/v1/repos/{open}"),
        format!("/api/v1/repos/{open}/pulls"),
        "/api/v1/repos/alice/open".to_string(),
        "/api/v1/repos/alice%2Fopen/pulls".to_string(),
    ] {
        let (status, _) = call(&router, HttpMethod::GET, &uri, None).await;
        assert_ne!(status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_ne!(status, StatusCode::FORBIDDEN, "{uri}");
    }

    let (status, list) = call(&router, HttpMethod::GET, "/api/v1/repos", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = list["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["id"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["open"]);
}

#[tokio::test]
async fn anonymous_visitor_is_refused_everything_else() {
    let (router, _core, open, closed) = forge();
    for uri in [
        format!("/api/v1/repos/{closed}"),
        format!("/api/v1/repos/{closed}/tree?ref=main"),
        "/api/v1/repos/alice/closed".to_string(),
        format!("/api/v1/repos/{open}/settings"),
        format!("/api/v1/repos/{open}/agent-runs"),
        "/api/v1/repos/alice/missing".to_string(),
        "/api/v1/auth/me".to_string(),
    ] {
        let (status, _) = call(&router, HttpMethod::GET, &uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let (status, _) = call(
        &router,
        HttpMethod::DELETE,
        &format!("/api/v1/repos/{open}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A bad credential is still a bad credential, even on a public repository.
    let (status, _) = call(
        &router,
        HttpMethod::GET,
        &format!("/api/v1/repos/{open}"),
        Some("not-a-token"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn signed_in_user_without_a_grant_reads_public_but_not_private() {
    let (router, core, open, closed) = forge();
    let token = core
        .create_personal_access_token("bob", "t", None)
        .unwrap()
        .secret;
    let (status, _) = call(
        &router,
        HttpMethod::GET,
        &format!("/api/v1/repos/{open}"),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &router,
        HttpMethod::GET,
        &format!("/api/v1/repos/{closed}"),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, list) = call(&router, HttpMethod::GET, "/api/v1/repos", Some(&token)).await;
    assert_eq!(list["repositories"].as_array().unwrap().len(), 1);
}
