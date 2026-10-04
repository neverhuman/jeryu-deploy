//! `POST /api/v1/admin/users` through the full router: an admin adds an
//! identity with its role and gets a one-time password back, a non-admin is
//! refused, and a login that already exists conflicts.

use std::path::Path;

use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::{ForgeCore, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{WebState, app};

async fn post(router: &axum::Router, uri: &str, token: &str, body: &Value) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
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
async fn admin_creates_a_user_with_a_role_and_duplicates_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).unwrap();
    core.create_account("admin", "admin-password", UserRole::Admin)
        .unwrap();
    core.create_account("reader", "reader-password", UserRole::User)
        .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, reader) = (token("admin"), token("reader"));
    let router = app(
        WebState::new(core).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );

    // A non-admin cannot add identities.
    let (status, body) = post(
        &router,
        "/api/v1/admin/users",
        &reader,
        &json!({ "login": "alton2", "role": "user" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // The admin adds the identity and is handed its one-time password.
    let (status, body) = post(
        &router,
        "/api/v1/admin/users",
        &admin,
        &json!({ "login": "alton2", "role": "admin" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["login"], "alton2", "{body}");
    assert_eq!(body["role"], "admin", "{body}");
    let password = body["password"].as_str().unwrap_or_default().to_string();
    assert!(password.starts_with("jeryu-"), "{body}");

    // It is a live account, served by the list route without a restart, and
    // the one-time password logs in once and must then be changed.
    let (status, users) = {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/admin/users")
                    .header(header::AUTHORIZATION, format!("Bearer {admin}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice::<Value>(&bytes).unwrap())
    };
    assert_eq!(status, StatusCode::OK, "{users}");
    let created = users
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["login"] == "alton2")
        .unwrap_or_else(|| panic!("alton2 missing: {users}"));
    assert_eq!(created["role"], "admin", "{users}");
    assert_eq!(created["must_change_password"], true, "{users}");

    let (status, login) = post(
        &router,
        "/api/v1/auth/login",
        &admin,
        &json!({ "login": "alton2", "password": password }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{login}");

    // The same login a second time is a conflict, not a silent overwrite.
    let (status, body) = post(
        &router,
        "/api/v1/admin/users",
        &admin,
        &json!({ "login": "alton2", "role": "user" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "conflict", "{body}");
}
