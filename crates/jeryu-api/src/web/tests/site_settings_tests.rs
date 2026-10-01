//! `/api/v1/site-settings` and `/api/v1/admin/site-settings` through the full
//! router: only admins change the wiki (another user is forbidden), and a
//! private wiki is reported only to callers who may read it.

use std::path::Path;

use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::{CreateRepositoryRequest, ForgeCore, RepoAccessLevel, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::super::{WebState, app};

async fn call(
    router: &axum::Router,
    method: HttpMethod,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(body.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(request.body(body).unwrap())
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
async fn admins_pick_the_wiki_and_readers_see_it_only_with_access() {
    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).unwrap();
    core.create_account("admin", "admin-password", UserRole::Admin)
        .unwrap();
    for login in ["reader", "outsider"] {
        core.create_account(login, "user-password", UserRole::User)
            .unwrap();
    }
    for (name, private) in [("handbook", true), ("docs", false)] {
        core.create_repository(
            "acme",
            CreateRepositoryRequest {
                name: name.to_string(),
                private,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }
    core.grant_repo_access("admin", "reader", "acme", "handbook", RepoAccessLevel::Read)
        .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, reader, outsider) = (token("admin"), token("reader"), token("outsider"));
    let router = app(
        WebState::new(core).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let read = |token: Option<String>| {
        let router = router.clone();
        async move {
            call(
                &router,
                HttpMethod::GET,
                "/api/v1/site-settings",
                token.as_deref(),
                None,
            )
            .await
        }
    };
    let put = |token: &str, wiki: Value| {
        let router = router.clone();
        let token = token.to_string();
        async move {
            call(
                &router,
                HttpMethod::PUT,
                "/api/v1/admin/site-settings",
                Some(&token),
                Some(json!({ "internal_wiki": wiki })),
            )
            .await
        }
    };

    // Unset: every caller sees no wiki.
    let (status, body) = read(Some(reader.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["internal_wiki"], Value::Null);

    // Only an admin may change it, and only to a repository that exists.
    let (status, _) = put(&reader, json!("acme/handbook")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = put(&admin, json!("acme/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = put(&admin, json!("handbook")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, body) = put(&admin, json!("acme/handbook")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["internal_wiki"]["full_name"], "acme/handbook");
    assert_eq!(body["updated_by"], "admin");
    assert_eq!(body["internal_wiki_missing"], false);

    let (status, body) = call(
        &router,
        HttpMethod::GET,
        "/api/v1/admin/site-settings",
        Some(&reader),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // A private wiki reaches the granted reader, not the outsider or a visitor.
    let (_, body) = read(Some(reader.clone())).await;
    assert_eq!(body["internal_wiki"]["owner"], "acme");
    assert_eq!(body["internal_wiki"]["name"], "handbook");
    assert_eq!(body["internal_wiki"]["default_branch"], "main");
    let (_, body) = read(Some(outsider.clone())).await;
    assert_eq!(body["internal_wiki"], Value::Null, "{body}");
    let (status, body) = read(None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["internal_wiki"], Value::Null, "{body}");

    // A public wiki is shown to everyone, including anonymous visitors.
    put(&admin, json!("acme/docs")).await;
    let (_, body) = read(None).await;
    assert_eq!(body["internal_wiki"]["full_name"], "acme/docs", "{body}");
    let (_, body) = read(Some(outsider.clone())).await;
    assert_eq!(body["internal_wiki"]["full_name"], "acme/docs", "{body}");

    // Clearing removes it.
    let (status, body) = put(&admin, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["internal_wiki"], Value::Null);
    assert_eq!(body["updated_by"], Value::Null);
    let (_, body) = read(Some(admin.clone())).await;
    assert_eq!(body["internal_wiki"], Value::Null);
}
