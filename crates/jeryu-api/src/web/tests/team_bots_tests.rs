//! HTTP coverage for organizations with owners and teams (ADR 0009) and the
//! team bots they own (ADR 0008).

use std::path::Path;

use axum::body::Body;
use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::{CreateRepositoryRequest, RepoAccessLevel, UserRole};
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

struct Fixture {
    router: axum::Router,
    /// Personal access tokens by login.
    root: String,
    owner: String,
    steward: String,
    other_steward: String,
    repo_admin: String,
    stranger: String,
}

/// root administers the forge; olive will own acme; sam and tess will be its
/// stewards; ria administers acme/widgets; zed is nobody's.
fn fixture() -> Fixture {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("root", "root-password-1", UserRole::Admin)
        .unwrap();
    for login in ["olive", "sam", "tess", "ria", "zed"] {
        core.create_account(login, &format!("{login}-password-1"), UserRole::User)
            .unwrap();
    }
    for name in ["widgets", "secret"] {
        core.create_repository(
            "acme-repos",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: true,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }
    core.grant_repo_access(
        "root",
        "ria",
        "acme-repos",
        "widgets",
        RepoAccessLevel::Admin,
    )
    .unwrap();
    let pat = |login: &str| {
        core.create_personal_access_token(login, "test", None)
            .unwrap()
            .secret
    };
    Fixture {
        root: pat("root"),
        owner: pat("olive"),
        steward: pat("sam"),
        other_steward: pat("tess"),
        repo_admin: pat("ria"),
        stranger: pat("zed"),
        router: app(
            WebState::new(core.clone()).with_auth(true, false, false),
            Path::new("/tmp/jeryu-no-spa"),
        ),
    }
}

/// acme owned by olive, with team `stewards` = sam and tess, and team bot
/// `reviewer`. Returns the bot id.
async fn organization_with_a_team_bot(fx: &Fixture) -> String {
    let (status, body) = send(
        &fx.router,
        HttpMethod::POST,
        "/api/v1/orgs",
        &fx.root,
        Some(json!({"login": "acme", "display_name": "Acme", "owners": ["olive"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = send(
        &fx.router,
        HttpMethod::POST,
        "/api/v1/orgs/acme/teams",
        &fx.owner,
        Some(json!({"name": "Stewards", "members": ["sam"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = send(
        &fx.router,
        HttpMethod::PUT,
        "/api/v1/orgs/acme/teams/stewards/members/tess",
        &fx.owner,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["members"], json!(["sam", "tess"]));
    let (status, body) = send(
        &fx.router,
        HttpMethod::POST,
        "/api/v1/orgs/acme/bots",
        &fx.owner,
        Some(json!({"steward_team": "stewards", "slug": "reviewer", "display_name": "Reviewer"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(
        body.get("enrollment_key").is_none(),
        "no key for a team bot: {body}"
    );
    assert_eq!(body["bot"]["acting_login"], "acme.reviewer[bot]");
    assert_eq!(body["bot"]["owner_kind"], "organization");
    body["bot"]["id"].as_str().unwrap().to_string()
}

async fn agent_token(fx: &Fixture, issuer: &str, bot_id: &str) -> String {
    let (status, body) = send(
        &fx.router,
        HttpMethod::POST,
        &format!("/api/v1/bots/{bot_id}/tokens"),
        issuer,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn organizations_are_created_by_administrators_and_run_by_owners() {
    let fx = fixture();
    let (status, body) = send(
        &fx.router,
        HttpMethod::POST,
        "/api/v1/orgs",
        &fx.owner,
        Some(json!({"login": "acme", "owners": ["olive"]})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    organization_with_a_team_bot(&fx).await;
    let (status, body) = send(
        &fx.router,
        HttpMethod::GET,
        "/api/v1/orgs/acme",
        &fx.steward,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["can_manage"], false);
    let (status, _) = send(
        &fx.router,
        HttpMethod::GET,
        "/api/v1/orgs/acme",
        &fx.stranger,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = send(
        &fx.router,
        HttpMethod::PUT,
        "/api/v1/orgs/acme/teams/stewards/members/zed",
        &fx.steward,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = send(
        &fx.router,
        HttpMethod::DELETE,
        "/api/v1/orgs/acme/owners/olive",
        &fx.owner,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "last_owner");
    let (status, body) = send(&fx.router, HttpMethod::GET, "/api/v1/orgs", &fx.owner, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["organizations"][0]["login"], "acme");
}

#[tokio::test]
async fn a_team_bot_acts_as_itself_with_only_the_grants_a_repo_admin_made() {
    let fx = fixture();
    let bot_id = organization_with_a_team_bot(&fx).await;
    let agent = agent_token(&fx, &fx.steward, &bot_id).await;
    let (status, who) = send(
        &fx.router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        &agent,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{who}");
    assert_eq!(who["acting_login"], "acme.reviewer[bot]");

    let widgets = "/api/v1/repos/acme-repos%2Fwidgets";
    let (status, body) = send(&fx.router, HttpMethod::GET, widgets, &agent, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let grant = format!("/api/v1/bots/{bot_id}/grants/acme-repos/widgets");
    for (token, who) in [
        (&fx.owner, "owner"),
        (&fx.steward, "steward"),
        (&agent, "the bot"),
    ] {
        let (status, body) = send(
            &fx.router,
            HttpMethod::PUT,
            &grant,
            token,
            Some(json!({"access": "write"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who}: {body}");
    }
    let (status, body) = send(
        &fx.router,
        HttpMethod::PUT,
        &grant,
        &fx.repo_admin,
        Some(json!({"access": "admin"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = send(
        &fx.router,
        HttpMethod::PUT,
        &grant,
        &fx.repo_admin,
        Some(json!({"access": "write"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["granted_by"], "ria");

    let (status, body) = send(&fx.router, HttpMethod::GET, widgets, &agent, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        &fx.router,
        HttpMethod::GET,
        "/api/v1/repos/acme-repos%2Fsecret",
        &agent,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // A bot never reaches organization or grant management.
    let (status, body) = send(&fx.router, HttpMethod::GET, "/api/v1/orgs", &agent, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = send(
        &fx.router,
        HttpMethod::GET,
        &format!("/api/v1/bots/{bot_id}/grants"),
        &fx.other_steward,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["grants"][0]["access"], "write");
    let (status, _) = send(&fx.router, HttpMethod::DELETE, &grant, &fx.repo_admin, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&fx.router, HttpMethod::GET, widgets, &agent, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_steward_who_leaves_takes_only_their_own_tokens() {
    let fx = fixture();
    let bot_id = organization_with_a_team_bot(&fx).await;
    let from_sam = agent_token(&fx, &fx.steward, &bot_id).await;
    let from_tess = agent_token(&fx, &fx.other_steward, &bot_id).await;
    let (status, body) = send(
        &fx.router,
        HttpMethod::GET,
        "/api/v1/bots",
        &fx.steward,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["team_bots"][0]["id"], bot_id.as_str());
    assert!(body["bots"].as_array().unwrap().is_empty());
    // The organization's owner is not a steward.
    let (status, _) = send(
        &fx.router,
        HttpMethod::POST,
        &format!("/api/v1/bots/{bot_id}/tokens"),
        &fx.owner,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = send(
        &fx.router,
        HttpMethod::DELETE,
        "/api/v1/orgs/acme/teams/stewards/members/sam",
        &fx.owner,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = send(
        &fx.router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        &from_sam,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(
        &fx.router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        &from_tess,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(
        &fx.router,
        HttpMethod::POST,
        "/api/v1/bots/tokens/revoke-issued",
        &fx.other_steward,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revoked"], 1);
    let (status, _) = send(
        &fx.router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        &from_tess,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
