//! `POST /api/v1/waitlist` and `GET /api/v1/admin/waitlist` through the router.
//!
//! HLT-022 negative proof: a signed-out caller and a non-admin never receive
//! the waitlist. Joining stores an email and does not open an account or set
//! a session cookie.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::{ForgeCore, ForgeError, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{WebState, app};

async fn post_anonymous(router: &axum::Router, body: &Value) -> (StatusCode, HeaderMap, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/waitlist")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn router_for(core: ForgeCore) -> axum::Router {
    app(
        WebState::new(core).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    )
}

#[tokio::test]
async fn join_creates_then_counts_a_repeat_without_a_session_or_account() {
    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).unwrap();
    core.create_account("admin", "admin-password", UserRole::Admin)
        .unwrap();
    core.create_account("reader", "reader-password", UserRole::User)
        .unwrap();
    let admin = core
        .create_personal_access_token("admin", "t", None)
        .unwrap()
        .secret;
    let reader = core
        .create_personal_access_token("reader", "t", None)
        .unwrap()
        .secret;
    let audit_core = core.clone();
    let router = router_for(core);

    let (status, headers, body) = post_anonymous(
        &router,
        &json!({
            "email": "  Ada@Example.COM ",
            "name": " Ada ",
            "note": " agents "
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(headers.get(header::SET_COOKIE).is_none(), "{headers:?}");
    assert_eq!(body, json!({ "result": "received" }), "{body}");

    let (status, headers, again) = post_anonymous(
        &router,
        &json!({
            "email": "ADA@example.com",
            "name": "someone else",
            "note": "a different note"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{again}");
    assert!(headers.get(header::SET_COOKIE).is_none(), "{headers:?}");
    assert_eq!(again, json!({ "result": "received" }), "{again}");

    let (status, _, invalid) = post_anonymous(&router, &json!({ "email": "not-an-email" })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
    assert_eq!(invalid["code"], "invalid_input", "{invalid}");

    let users = router
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
    assert_eq!(users.status(), StatusCode::OK);
    let users = axum::body::to_bytes(users.into_body(), usize::MAX)
        .await
        .unwrap();
    let users: Value = serde_json::from_slice(&users).unwrap();
    let logins: Vec<&str> = users
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["login"].as_str())
        .collect();
    assert_eq!(logins, ["admin", "reader"], "{users}");

    let signed_out = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/admin/waitlist")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(signed_out.status(), StatusCode::UNAUTHORIZED);

    let forbidden = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/admin/waitlist")
                .header(header::AUTHORIZATION, format!("Bearer {reader}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let listed = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/admin/waitlist")
                .header(header::AUTHORIZATION, format!("Bearer {admin}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed = axum::body::to_bytes(listed.into_body(), usize::MAX)
        .await
        .unwrap();
    let listed: Value = serde_json::from_slice(&listed).unwrap();
    assert_eq!(listed["total"], 1, "{listed}");
    assert_eq!(listed["limit"], 100, "{listed}");
    assert_eq!(listed["offset"], 0, "{listed}");
    assert_eq!(listed["signups"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed["signups"][0]["email"], "ada@example.com", "{listed}");
    assert_eq!(listed["signups"][0]["name"], "Ada", "{listed}");
    assert_eq!(listed["signups"][0]["note"], "agents", "{listed}");
    assert_eq!(listed["signups"][0]["status"], "listed", "{listed}");
    assert_eq!(listed["signups"][0]["source"], "landing", "{listed}");
    // A repeat join keeps the stored row and does not write again.
    assert_eq!(listed["signups"][0]["request_count"], 1, "{listed}");
    let audit = audit_core.list_audit("waitlist").unwrap();
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0].actor, "admin");
    assert_eq!(audit[0].action, "waitlist.list");
    assert_eq!(audit[0].phase, "completed");
}

#[tokio::test]
async fn one_address_posting_eleven_different_emails_is_limited() {
    let router = router_for(ForgeCore::new());
    let mut statuses = Vec::new();
    for index in 0..11 {
        let (status, headers, _) = post_anonymous(
            &router,
            &json!({ "email": format!("person{index}@example.com") }),
        )
        .await;
        assert!(headers.get(header::SET_COOKIE).is_none(), "{headers:?}");
        statuses.push(status);
    }
    assert_eq!(
        &statuses[..5],
        &[StatusCode::ACCEPTED; 5],
        "the first five joins from one address are accepted: {statuses:?}"
    );
    assert!(
        statuses[5..]
            .iter()
            .all(|status| *status == StatusCode::TOO_MANY_REQUESTS),
        "rotating emails does not grant another budget: {statuses:?}"
    );
}

#[tokio::test]
async fn waitlist_route_bucket_limits_across_addresses() {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use axum::extract::ConnectInfo;

    let router = router_for(ForgeCore::new());
    let mut accepted = 0;
    for host in 1..=7 {
        for index in 0..5 {
            let mut request = Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/waitlist")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    json!({ "email": format!("h{host}-{index}@example.com") }).to_string(),
                ))
                .unwrap();
            request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(203, 0, 113, host)),
                9,
            )));
            let response = router.clone().oneshot(request).await.unwrap();
            if response.status() == StatusCode::ACCEPTED {
                accepted += 1;
            } else {
                assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            }
        }
    }
    assert_eq!(accepted, 30, "the route accepts thirty joins a minute");
}

/// Once the route window is closed, further addresses must not create keys.
/// The hour-long per-address bucket is what would otherwise grow without bound.
#[tokio::test]
async fn route_refusal_adds_no_per_address_key() {
    let state = WebState::new(ForgeCore::new()).with_auth(true, false, false);
    let limits = Arc::clone(&state.auth_rate_limits);
    let router = app(state, Path::new("/tmp/jeryu-no-spa"));

    for host in 1..=6 {
        for index in 0..5 {
            let (status, _, body) = post_from(
                &router,
                Ipv4Addr::new(203, 0, 113, host),
                &format!("fill-{host}-{index}@example.com"),
            )
            .await;
            assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        }
    }
    let before = waitlist_keys(&limits);
    assert!(
        before.iter().any(|key| key == "waitlist:route"),
        "the route bucket is the one being closed: {before:?}"
    );
    assert_eq!(
        before.len(),
        7,
        "six addresses plus the route key: {before:?}"
    );

    for host in 1..=40 {
        let (status, headers, body) = post_from(
            &router,
            Ipv4Addr::new(198, 51, 100, host),
            &format!("flood-{host}@example.com"),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        if host == 1 {
            assert_waitlist_refusal(&headers, &body);
        }
    }
    let after = waitlist_keys(&limits);
    assert_eq!(
        after, before,
        "a closed route window minted per-address keys: before {before:?} after {after:?}"
    );
}

/// The sixth join from one address is the public 429. The header and the
/// message are the contract a client waits on.
#[tokio::test]
async fn waitlist_refusal_states_the_wait_and_the_reason() {
    let router = router_for(ForgeCore::new());
    for index in 0..5 {
        let (status, _, body) = post_from(
            &router,
            Ipv4Addr::new(203, 0, 113, 8),
            &format!("ada{index}@example.com"),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    }
    let (status, headers, body) =
        post_from(&router, Ipv4Addr::new(203, 0, 113, 8), "ada5@example.com").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_waitlist_refusal(&headers, &body);
}

fn assert_waitlist_refusal(headers: &HeaderMap, body: &Value) {
    assert_eq!(
        headers
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("60"),
        "{headers:?} {body}"
    );
    assert_eq!(body["code"], "rate_limited", "{body}");
    assert_eq!(
        body["message"], "too many authentication attempts",
        "{body}"
    );
}

async fn post_from(
    router: &axum::Router,
    ip: Ipv4Addr,
    email: &str,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(HttpMethod::POST)
        .uri("/api/v1/waitlist")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            json!({ "email": email }).to_string(),
        ))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(IpAddr::V4(ip), 9)));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn waitlist_keys(
    limits: &Arc<
        std::sync::Mutex<std::collections::BTreeMap<String, super::super::auth::RateLimitBucket>>,
    >,
) -> Vec<String> {
    limits
        .lock()
        .expect("auth rate-limit mutex poisoned")
        .keys()
        .filter(|key| key.starts_with("waitlist:"))
        .cloned()
        .collect()
}

#[tokio::test]
async fn waitlist_body_over_the_cap_is_refused() {
    let router = router_for(ForgeCore::new());
    let response = router
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/waitlist")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(format!(
                    "{{\"email\":\"{}\"}}",
                    "a".repeat(5000)
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

/// The full-list arm matches core's typed `WaitlistFull`, never display text.
#[tokio::test]
async fn waitlist_full_display_maps_to_503() {
    // A storage error that merely mentions the code is not a full list.
    let lookalike = super::super::auth::waitlist_join_failure(ForgeError::Storage(
        "waitlist_full: echoed caller text".to_string(),
    ));
    assert_eq!(lookalike.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let error = ForgeError::WaitlistFull;
    let published = super::super::error_codes::lookup("waitlist_full")
        .expect("waitlist_full is published in error_codes.rs");
    assert_eq!(published.status, 503);

    let response = super::super::auth::waitlist_join_failure(error);
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        status,
        StatusCode::from_u16(published.status).unwrap(),
        "{body}"
    );
    assert_eq!(
        body,
        json!({
            "code": published.code,
            "message": published.summary,
        }),
        "{body}"
    );
    let rendered = body.to_string();
    assert!(
        !rendered.contains("storage failed") && !rendered.contains("echoed caller text"),
        "public body leaked internal detail: {rendered}"
    );
}

/// A sealed database fails the join and the admin read. The public body is the
/// fixed sentence, and the storage error text stays off the wire.
#[tokio::test]
async fn waitlist_storage_errors_hide_internal_detail() {
    let dir = tempfile::tempdir().unwrap();
    let marker = "waitlist-internal-leak-marker";
    let path = dir.path().join(format!("{marker}.sqlite"));
    let core = ForgeCore::open_sqlite(&path).unwrap();
    core.create_account("admin", "admin-password", UserRole::Admin)
        .unwrap();
    let admin = core
        .create_personal_access_token("admin", "t", None)
        .unwrap()
        .secret;

    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let internal = core
        .join_waitlist("ada@example.com", None, None)
        .expect_err("a directory where the database was must fail the join");
    let internal_text = internal.to_string();
    assert!(
        !matches!(internal, ForgeError::Validation(_)),
        "{internal_text}"
    );
    assert!(
        internal_text.contains("storage failed"),
        "expected a storage error, got {internal_text}"
    );

    let router = router_for(core);
    let (status, _, body) = post_anonymous(&router, &json!({ "email": "bea@example.com" })).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(
        body,
        public_storage_failure("could not save the waitlist signup"),
        "{body}"
    );
    assert_no_storage_leak(&body, &internal_text, marker);

    let listed = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/admin/waitlist")
                .header(header::AUTHORIZATION, format!("Bearer {admin}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let listed_status = listed.status();
    let listed_bytes = axum::body::to_bytes(listed.into_body(), usize::MAX)
        .await
        .unwrap();
    let listed: Value = serde_json::from_slice(&listed_bytes).unwrap();
    assert_eq!(listed_status, StatusCode::INTERNAL_SERVER_ERROR, "{listed}");
    assert_eq!(
        listed,
        public_storage_failure("could not record the waitlist read"),
        "{listed}"
    );
    assert_no_storage_leak(&listed, &internal_text, marker);
}

fn public_storage_failure(message: &str) -> Value {
    json!({
        "code": "storage_failed",
        "message": message,
        "reason": message,
        "purpose": "complete a jeryu API request",
        "common_fixes": [
            "look the code up at GET /api/v1/errors",
            "check the request against the route index at GET /api/v1"
        ],
        "repair_hint": "look the code up at GET /api/v1/errors, fix the request it names, and retry",
        "docs_url": "/api/v1/docs/errors.md"
    })
}

fn assert_no_storage_leak(body: &Value, internal: &str, marker: &str) {
    let rendered = body.to_string();
    assert!(
        !rendered.contains(internal)
            && !rendered.contains(marker)
            && !rendered.contains("storage failed"),
        "public body leaked {internal}: {rendered}"
    );
}

/// One row from the middle of at least three. Dropping skip or take changes
/// either the length or which address comes back.
#[tokio::test]
async fn admin_waitlist_honors_limit_and_offset() {
    let core = ForgeCore::new();
    core.create_account("admin", "admin-password", UserRole::Admin)
        .unwrap();
    let admin = core
        .create_personal_access_token("admin", "t", None)
        .unwrap()
        .secret;
    for email in ["ada@example.com", "bea@example.com", "cy@example.com"] {
        core.join_waitlist(email, None, None).unwrap();
    }
    let mut ordered = core.list_waitlist();
    ordered.sort_by(|left, right| {
        right
            .last_requested_at
            .cmp(&left.last_requested_at)
            .then_with(|| left.email.cmp(&right.email))
    });
    assert!(ordered.len() >= 3, "{}", ordered.len());
    let router = router_for(core);

    let listed = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/admin/waitlist?limit=1&offset=1")
                .header(header::AUTHORIZATION, format!("Bearer {admin}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed = axum::body::to_bytes(listed.into_body(), usize::MAX)
        .await
        .unwrap();
    let listed: Value = serde_json::from_slice(&listed).unwrap();
    let signups = listed["signups"].as_array().unwrap();
    assert_eq!(signups.len(), 1, "{listed}");
    assert_eq!(listed["total"], 3, "{listed}");
    assert_eq!(listed["limit"], 1, "{listed}");
    assert_eq!(listed["offset"], 1, "{listed}");
    assert_eq!(signups[0]["email"], ordered[1].email, "{listed}");
}
