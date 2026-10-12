//! HTTP coverage for owners (family) reach and the free-form bot kind.

use std::path::Path;

use axum::body::Body;
use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::{CreateOrganizationRequest, CreateRepositoryRequest, RepoAccessLevel, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{WebState, app};

async fn send(
    router: &axum::Router,
    method: HttpMethod,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::HOST, "127.0.0.1");
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let body = body.map(|value| value.to_string()).unwrap_or_default();
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
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

fn repo(name: &str) -> CreateRepositoryRequest {
    CreateRepositoryRequest {
        name: name.to_string(),
        private: true,
        description: None,
        default_branch: Some("main".to_string()),
    }
}

/// dee may write acme/widgets and globex/ledger; acme/secret has no grant.
fn fixture() -> (jeryu_core::ForgeCore, String, axum::Router) {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("dee", "dee-password-1", UserRole::User)
        .unwrap();
    for login in ["acme", "globex"] {
        core.create_organization(CreateOrganizationRequest {
            login: login.to_string(),
            display_name: None,
        })
        .unwrap();
    }
    core.create_repository("acme", repo("widgets")).unwrap();
    core.create_repository("acme", repo("secret")).unwrap();
    core.create_repository("globex", repo("ledger")).unwrap();
    for (owner, name) in [("acme", "widgets"), ("globex", "ledger")] {
        core.grant_repo_access("dee", "dee", owner, name, RepoAccessLevel::Write)
            .unwrap();
    }
    let pat = core
        .create_personal_access_token("dee", "owner", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    (core, pat, router)
}

async fn enroll(router: &axum::Router, pat: &str, body: Value) -> (StatusCode, Value) {
    send(router, HttpMethod::POST, "/api/v1/bots", pat, Some(body)).await
}

#[tokio::test]
async fn enroll_accepts_an_owners_reach_and_defaults_the_kind_to_agent() {
    let (_core, pat, router) = fixture();
    let (status, body) = enroll(
        &router,
        &pat,
        json!({
            "slug": "family",
            "display_name": "Family",
            "reach": {"mode": "owners", "owners": [" ACME ", "acme"]},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["bot"]["kind"], "agent");
    assert_eq!(
        body["bot"]["reach"],
        json!({"mode": "owners", "owners": ["acme"]})
    );
    let script = body["setup_script"].as_str().unwrap();
    assert!(script.contains("KIND='agent'"), "kind reaches the script");
    assert!(script.contains("LABEL='agent'"));

    let (status, body) = enroll(
        &router,
        &pat,
        json!({
            "slug": "coder",
            "display_name": "Coder",
            "kind": "claude-code",
            "reach": {"mode": "general"},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["bot"]["kind"], "claude-code");

    let (status, body) = send(&router, HttpMethod::GET, "/api/v1/bots", &pat, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let bots = body["bots"].as_array().unwrap();
    assert_eq!(bots.len(), 2);
    let family = bots.iter().find(|bot| bot["slug"] == "family").unwrap();
    assert_eq!(
        family["reach"],
        json!({"mode": "owners", "owners": ["acme"]})
    );
    assert_eq!(family["kind"], "agent");
}

#[tokio::test]
async fn enroll_rejects_a_bad_kind_or_owners_with_their_own_codes() {
    let (_core, pat, router) = fixture();
    for kind in [
        json!("Grok"),
        json!("9lives"),
        json!(""),
        json!("a".repeat(33)),
        json!(5),
    ] {
        let (status, body) = enroll(
            &router,
            &pat,
            json!({
                "slug": "bad",
                "display_name": "Bad",
                "kind": kind,
                "reach": {"mode": "general"},
            }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{kind}: {body}");
        assert_eq!(body["code"], "invalid_bot_kind", "{kind}: {body}");
    }
    let many: Vec<String> = (0..21).map(|index| format!("org{index}")).collect();
    for owners in [
        json!([]),
        json!(["nobody"]),
        json!(["acme/widgets"]),
        json!(many),
    ] {
        let (status, body) = enroll(
            &router,
            &pat,
            json!({
                "slug": "bad",
                "display_name": "Bad",
                "reach": {"mode": "owners", "owners": owners},
            }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{owners}: {body}");
        assert_eq!(body["code"], "invalid_reach_owners", "{owners}: {body}");
    }
    let (status, body) = send(&router, HttpMethod::GET, "/api/v1/bots", &pat, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["bots"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn an_owners_agent_token_reaches_granted_family_repositories_only() {
    let (core, pat, router) = fixture();
    let (status, body) = enroll(
        &router,
        &pat,
        json!({
            "slug": "family",
            "display_name": "Family",
            "reach": {"mode": "owners", "owners": ["acme"]},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tokens = format!(
        "/api/v1/bots/{}/tokens",
        body["bot"]["id"].as_str().unwrap()
    );
    let (status, body) = send(&router, HttpMethod::POST, &tokens, &pat, Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let agent = body["token"].as_str().unwrap().to_string();

    // Created after enrollment, and granted to the owner: in reach.
    core.create_repository("acme", repo("fresh")).unwrap();
    core.grant_repo_access("dee", "dee", "acme", "fresh", RepoAccessLevel::Write)
        .unwrap();
    for (uri, expected) in [
        ("/api/v1/repos/acme%2Fwidgets", StatusCode::OK),
        ("/api/v1/repos/acme%2Ffresh", StatusCode::OK),
        ("/api/v1/repos/acme%2Fsecret", StatusCode::FORBIDDEN),
        ("/api/v1/repos/globex%2Fledger", StatusCode::FORBIDDEN),
    ] {
        let (status, body) = send(&router, HttpMethod::GET, uri, &agent, None).await;
        assert_eq!(status, expected, "{uri}: {body}");
    }
}
