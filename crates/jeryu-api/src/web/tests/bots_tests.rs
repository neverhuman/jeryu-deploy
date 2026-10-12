//! HTTP coverage for account Grokbot and Musebot credentials.
//!
//! A bot token is a normal user of its owner: forge admin is not inherited,
//! and reach is checked with `authorize_bot` rather than the human repo gate.

use std::net::SocketAddr;
use std::path::Path;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use jeryu_core::{
    BotKind, BotReach, BotRepoRef, BotTaskKind, CreateIssueRequest, CreateRepositoryRequest,
    RepoAccessLevel, UserRole,
};
use serde_json::{Value, json};
use sha2::Sha256;
use tower::ServiceExt;

use super::{WebState, app};

async fn send(
    router: &axum::Router,
    method: HttpMethod,
    uri: &str,
    token: Option<&str>,
    host: Option<&str>,
    body: Option<&str>,
) -> (StatusCode, Value, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(host) = host {
        builder = builder.header(header::HOST, host);
    }
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let response = router
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.unwrap_or("").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json, text)
}

fn router_for(core: jeryu_core::ForgeCore) -> axum::Router {
    app(
        WebState::new(core).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    )
}

fn repo(name: &str, private: bool) -> CreateRepositoryRequest {
    CreateRepositoryRequest {
        name: name.to_string(),
        private,
        description: None,
        default_branch: Some("main".to_string()),
    }
}

async fn access_token(router: &axum::Router, enrollment_key: &str, aud: &str) -> Value {
    let (status, body, _) = send(
        router,
        HttpMethod::POST,
        "/api/v1/bot/token",
        None,
        None,
        Some(&json!({ "enrollment_key": enrollment_key, "aud": aud }).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn bot_tokens_honor_reach_and_refuse_admin_inheritance() {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("ada", "ada-password-1", UserRole::Admin)
        .unwrap();
    core.create_repository("ada", repo("open", false)).unwrap();
    core.create_repository("ada", repo("vault", true)).unwrap();
    core.create_repository("ada", repo("widgets", true))
        .unwrap();
    core.create_repository("ada", repo("other", true)).unwrap();
    core.grant_repo_access("ada", "ada", "ada", "widgets", RepoAccessLevel::Write)
        .unwrap();
    core.create_issue(
        "ada",
        "widgets",
        "ada",
        CreateIssueRequest {
            title: "one".to_string(),
            ..CreateIssueRequest::default()
        },
    )
    .unwrap();
    core.create_issue(
        "ada",
        "widgets",
        "ada",
        CreateIssueRequest {
            title: "two".to_string(),
            ..CreateIssueRequest::default()
        },
    )
    .unwrap();
    let general = core
        .enroll_bot("ada", "atlas", "Atlas", BotKind::Grok, BotReach::General)
        .unwrap();
    let repo_bot = core
        .enroll_bot(
            "ada",
            "repo",
            "Repo",
            BotKind::Muse,
            BotReach::Repositories {
                repos: vec![BotRepoRef {
                    owner: "ada".to_string(),
                    name: "widgets".to_string(),
                }],
            },
        )
        .unwrap();
    let task_bot = core
        .enroll_bot(
            "ada",
            "task",
            "Task",
            BotKind::Grok,
            BotReach::Task {
                repo: BotRepoRef {
                    owner: "ada".to_string(),
                    name: "widgets".to_string(),
                },
                task_kind: BotTaskKind::Issue,
                task_id: "1".to_string(),
            },
        )
        .unwrap();
    let router = router_for(core.clone());
    let general_token = access_token(&router, &general.enrollment_key, "http://127.0.0.1").await;
    let access = general_token["access_token"].as_str().unwrap();

    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/repos/ada/vault",
        Some(access),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "repo_not_granted");

    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        "/api/v1/repos/ada/vault/pulls/1/comments",
        Some(access),
        Some("127.0.0.1"),
        Some(r#"{"body":"no"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "repo_not_granted");

    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/repos/ada/open",
        Some(access),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        "/api/v1/repos/ada/open/pulls/1/comments",
        Some(access),
        Some("127.0.0.1"),
        Some(r#"{"body":"no"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "insufficient_scope");

    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        "/api/v1/bots",
        Some(access),
        Some("127.0.0.1"),
        Some(r#"{"slug":"more","display_name":"More","kind":"grok","reach":{"mode":"general"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "insufficient_scope");

    let (status, body, _) = send(
        &router,
        HttpMethod::DELETE,
        "/api/v1/repos/ada/widgets",
        Some(access),
        Some("127.0.0.1"),
        Some(r#"{"confirm_full_name":"ada/widgets","delete_storage":true}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "insufficient_scope");
    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/repos/ada/widgets",
        Some(access),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        "/api/v1/repos/ada/widgets/hooks",
        Some(access),
        Some("127.0.0.1"),
        Some("{}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "insufficient_scope");

    let (status, who, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        Some(access),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{who}");
    assert_eq!(who["kind"], "Grokbot");
    assert_eq!(who["slug"], "atlas");
    assert_eq!(who["owner"], "ada");

    let repo_token = access_token(&router, &repo_bot.enrollment_key, "http://127.0.0.1").await;
    let repo_access = repo_token["access_token"].as_str().unwrap();
    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/repos/ada/other",
        Some(repo_access),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "repo_not_granted");

    let task_token = access_token(&router, &task_bot.enrollment_key, "http://127.0.0.1").await;
    let task_access = task_token["access_token"].as_str().unwrap();
    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        "/api/v1/repos/ada/widgets/issues/2/comments",
        Some(task_access),
        Some("127.0.0.1"),
        Some(r#"{"body":"no"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "task_not_granted");
    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        "/api/v1/repos/ada/other/issues/1/comments",
        Some(task_access),
        Some("127.0.0.1"),
        Some(r#"{"body":"no"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "repo_not_granted");

    let (status, body, text) = send(
        &router,
        HttpMethod::POST,
        "/api/v1/repos/ada/widgets/sessions",
        Some(access),
        Some("127.0.0.1"),
        Some(r#"{"provider":"local","command":"/bin/sh","task":"look"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body} {text}");
    assert!(!text.contains("/bin/sh"));
}

#[tokio::test]
async fn refresh_audience_revocation_and_query_credentials() {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("ada", "ada-password-1", UserRole::User)
        .unwrap();
    let enrolled = core
        .enroll_bot("ada", "atlas", "Atlas", BotKind::Grok, BotReach::General)
        .unwrap();
    let router = router_for(core.clone());

    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        "/api/v1/bot/token",
        None,
        None,
        Some(r#"{"enrollment_key":"jbk_live.nope.nope","aud":"http://evil.example"}"#),
    )
    .await;
    // The error envelope publishes every `/api` 400 as 422.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "invalid_input");

    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/bot/token?access_token=secret",
        None,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "invalid_request");

    let minted = access_token(&router, &enrolled.enrollment_key, "https://forge.example").await;
    let foreign = minted["access_token"].as_str().unwrap();
    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        Some(foreign),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "invalid_token");

    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        Some(&enrolled.enrollment_key),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let _ = access_token(&router, &enrolled.enrollment_key, "http://127.0.0.1").await;
    // An unknown refresh token is the same invalid credential as an expired one.
    // jeryu-core proves that expiry itself does not revoke the family.
    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        "/api/v1/bot/token/refresh",
        None,
        None,
        Some(r#"{"refresh_token":"jbr_not-a-real-refresh-token","aud":"http://127.0.0.1"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let again = access_token(&router, &enrolled.enrollment_key, "http://127.0.0.1").await;
    let again_access = again["access_token"].as_str().unwrap();
    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        Some(again_access),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["slug"], "atlas");

    core.revoke_bot("ada", enrolled.bot.id).unwrap();
    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        Some(again_access),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn bot_mcp_mount_hides_admin_tools_and_requires_repo() {
    use axum::http::Request as HttpRequest;

    let core = jeryu_core::ForgeCore::new();
    core.create_account("ada", "ada-password-1", UserRole::Admin)
        .unwrap();
    let enrolled = core
        .enroll_bot("ada", "atlas", "Atlas", BotKind::Grok, BotReach::General)
        .unwrap();
    let router = router_for(core);
    let minted = access_token(&router, &enrolled.enrollment_key, "http://127.0.0.1").await;
    let token = minted["access_token"].as_str().unwrap();

    let post = |method: &'static str, session: Option<String>, body: Value| {
        let mut request = HttpRequest::builder()
            .method("POST")
            .uri("/mcp/bot")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::HOST, "127.0.0.1")
            .header(header::CONTENT_TYPE, "application/json")
            .header("Mcp-Method", method);
        if let Some(session) = session {
            request = request
                .header("Mcp-Session-Id", session)
                .header("MCP-Protocol-Version", jeryu_mcp::MCP_PROTOCOL_VERSION)
                .header("Mcp-Name", "jeryu.code.definition");
        }
        request.body(Body::from(body.to_string())).unwrap()
    };
    let initialized = router
        .clone()
        .oneshot(post(
            "initialize",
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": jeryu_mcp::MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "grokbot", "version": "1" }
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(initialized.status(), StatusCode::OK);
    let session = initialized
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|value| value.to_str().ok())
        .expect("session")
        .to_string();
    let called = router
        .clone()
        .oneshot(post(
            "tools/call",
            Some(session),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "jeryu.code.definition",
                    "arguments": { "symbol": "CodeGraph" }
                }
            }),
        ))
        .await
        .unwrap();
    let status = called.status();
    let bytes = axum::body::to_bytes(called.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert_eq!(status, StatusCode::OK, "{text}");
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["result"]["isError"], json!(true), "{body}");
    assert_eq!(
        body["result"]["structuredContent"]["message"],
        json!("code.definition requires repo")
    );
}

#[tokio::test]
async fn bots_cannot_change_the_repository_or_widen_a_branch_task() {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("bea", "bea-password-1", UserRole::User)
        .unwrap();
    core.create_repository("bea", repo("widgets", true))
        .unwrap();
    core.grant_repo_access("bea", "bea", "bea", "widgets", RepoAccessLevel::Write)
        .unwrap();
    let general = core
        .enroll_bot("bea", "atlas", "Atlas", BotKind::Grok, BotReach::General)
        .unwrap();
    let branch_bot = core
        .enroll_bot(
            "bea",
            "feat",
            "Feat",
            BotKind::Grok,
            BotReach::Task {
                repo: BotRepoRef {
                    owner: "bea".to_string(),
                    name: "widgets".to_string(),
                },
                task_kind: BotTaskKind::Branch,
                task_id: "feat".to_string(),
            },
        )
        .unwrap();
    let router = router_for(core.clone());
    let minted = access_token(&router, &general.enrollment_key, "http://127.0.0.1").await;
    let access = minted["access_token"].as_str().unwrap();

    for (method, uri, body) in [
        (
            HttpMethod::PATCH,
            "/api/v3/repos/bea/widgets",
            Some(r#"{"archived":true}"#),
        ),
        (
            HttpMethod::PATCH,
            "/api/v3/repos/bea/widgets",
            Some(r#"{"name":"moved"}"#),
        ),
        (
            HttpMethod::DELETE,
            "/api/v1/repos/bea%2Fwidgets",
            Some(r#"{"confirm_full_name":"bea/widgets"}"#),
        ),
        (
            HttpMethod::PATCH,
            "/api/v1/repos/bea%2Fwidgets",
            Some(r#"{"family":"elsewhere"}"#),
        ),
    ] {
        let (status, body, _) = send(
            &router,
            method.clone(),
            uri,
            Some(access),
            Some("127.0.0.1"),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {body}");
    }
    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v3/repos/bea/widgets",
        Some(access),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_ne!(body["archived"], json!(true), "{body}");

    let minted = access_token(&router, &branch_bot.enrollment_key, "http://127.0.0.1").await;
    let access = minted["access_token"].as_str().unwrap();
    for uri in [
        "/api/v1/repos/bea%2Fwidgets/issues?branch=feat",
        "/api/v3/repos/bea/widgets/issues?branch=feat",
    ] {
        let (status, body, _) = send(
            &router,
            HttpMethod::POST,
            uri,
            Some(access),
            Some("127.0.0.1"),
            Some(r#"{"title":"not this bot's task"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
    }
}

async fn post_open(
    router: &axum::Router,
    uri: &str,
    body: &str,
) -> (StatusCode, Value, Option<String>) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json, retry_after)
}

/// `bot-token` and `bot-refresh` are separate 10-per-60-second buckets keyed by
/// client address. The eleventh call on a route is 429; the other route is not.
#[tokio::test]
async fn bot_token_and_refresh_routes_rate_limit_after_ten() {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("ada", "ada-password-1", UserRole::User)
        .unwrap();
    core.enroll_bot("ada", "atlas", "Atlas", BotKind::Grok, BotReach::General)
        .unwrap();
    let router = router_for(core);
    let token_body = r#"{"enrollment_key":"jbk_live.nope.nope","aud":"http://127.0.0.1"}"#;
    let refresh_body =
        r#"{"refresh_token":"jbr_not-a-real-refresh-token","aud":"http://127.0.0.1"}"#;

    for attempt in 1..=10 {
        let (status, body, retry_after) = post_open(&router, "/api/v1/bot/token", token_body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "token attempt {attempt}: {body}"
        );
        assert_eq!(body["code"], "invalid_token");
        assert!(retry_after.is_none(), "token attempt {attempt}");
    }
    let (status, body, retry_after) = post_open(&router, "/api/v1/bot/token", token_body).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "rate_limited");
    assert_eq!(retry_after.as_deref(), Some("60"));

    for attempt in 1..=10 {
        let (status, body, retry_after) =
            post_open(&router, "/api/v1/bot/token/refresh", refresh_body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "refresh attempt {attempt} shares the token bucket: {body}"
        );
        assert_eq!(body["code"], "invalid_token");
        assert!(retry_after.is_none(), "refresh attempt {attempt}");
    }
    let (status, body, retry_after) =
        post_open(&router, "/api/v1/bot/token/refresh", refresh_body).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "rate_limited");
    assert_eq!(retry_after.as_deref(), Some("60"));

    let (status, body, _) = post_open(&router, "/api/v1/bot/token", token_body).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
}

#[tokio::test]
async fn refresh_rotates_through_the_route_and_reuse_is_refused() {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("ada", "ada-password-1", UserRole::User)
        .unwrap();
    let enrolled = core
        .enroll_bot("ada", "atlas", "Atlas", BotKind::Grok, BotReach::General)
        .unwrap();
    let router = router_for(core);
    let minted = access_token(&router, &enrolled.enrollment_key, "http://127.0.0.1").await;
    let refresh = minted["refresh_token"].as_str().unwrap().to_string();
    assert!(refresh.starts_with("jbr_"));

    let (status, rotated, _) = send(
        &router,
        HttpMethod::POST,
        "/api/v1/bot/token/refresh",
        None,
        None,
        Some(&json!({ "refresh_token": refresh, "aud": "http://127.0.0.1" }).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    let rotated_refresh = rotated["refresh_token"].as_str().unwrap().to_string();
    let rotated_access = rotated["access_token"].as_str().unwrap().to_string();
    assert_ne!(rotated_refresh, refresh);
    assert!(rotated_refresh.starts_with("jbr_"));
    let (status, who, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        Some(&rotated_access),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{who}");
    assert_eq!(who["slug"], "atlas");

    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        "/api/v1/bot/token/refresh",
        None,
        None,
        Some(&json!({ "refresh_token": refresh, "aud": "http://127.0.0.1" }).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "invalid_token");
    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        Some(&rotated_access),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        "/api/v1/bot/token/refresh",
        None,
        None,
        Some(&json!({ "refresh_token": rotated_refresh, "aud": "http://127.0.0.1" }).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "invalid_token");
}

#[tokio::test]
async fn refresh_token_is_refused_as_an_api_bearer() {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("ada", "ada-password-1", UserRole::Admin)
        .unwrap();
    core.create_repository("ada", repo("open", false)).unwrap();
    let router = router_for(core);
    let bearer = "jbr_not-a-real-refresh-token";
    for path in ["/api/v1/bot/whoami", "/api/v1/repos/ada/open"] {
        let (status, body, text) = send(
            &router,
            HttpMethod::GET,
            path,
            Some(bearer),
            Some("127.0.0.1"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} {text}");
        assert_eq!(body["code"], "invalid_token", "{path} {body}");
        assert!(!text.contains(bearer), "{path}");
    }
}

#[tokio::test]
async fn git_smart_http_denies_a_bot_bearer_outside_its_grant() {
    let root = tempfile::tempdir().expect("git root");
    let core = jeryu_core::ForgeCore::new();
    core.create_account("ada", "ada-password-1", UserRole::Admin)
        .unwrap();
    core.create_repository("ada", repo("open", false)).unwrap();
    core.create_repository("ada", repo("vault", true)).unwrap();
    let enrolled = core
        .enroll_bot("ada", "atlas", "Atlas", BotKind::Grok, BotReach::General)
        .unwrap();
    let state = WebState::new_with_git_storage(core, root.path().to_path_buf())
        .with_auth(true, false, false);
    for name in ["open", "vault"] {
        state
            .repo_manager
            .create_bare(&jeryu_gitd::RepoId::new("ada", name).unwrap())
            .unwrap();
    }
    let router = app(state, Path::new("/tmp/jeryu-no-spa"));
    let minted = access_token(&router, &enrolled.enrollment_key, "http://127.0.0.1").await;
    let token = minted["access_token"].as_str().unwrap();

    let git = |path: &'static str| {
        let router = router.clone();
        let token = token.to_string();
        async move {
            let mut request = Request::builder()
                .method(HttpMethod::GET)
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::HOST, "127.0.0.1")
                .body(Body::empty())
                .unwrap();
            request.extensions_mut().insert(ConnectInfo(
                "192.0.2.10:43210".parse::<SocketAddr>().unwrap(),
            ));
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let text = String::from_utf8_lossy(&bytes).to_string();
            (status, content_type, text)
        }
    };

    let (status, content_type, text) =
        git("/git/ada/open.git/info/refs?service=git-upload-pack").await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(content_type, "application/x-git-upload-pack-advertisement");

    let (status, _, text) = git("/git/ada/vault.git/info/refs?service=git-upload-pack").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["code"], "repo_not_granted", "{body}");
    assert!(
        text.contains("principal not authorized to read ada/vault.git"),
        "{text}"
    );

    let (status, _, text) = git("/git/ada/open.git/info/refs?service=git-receive-pack").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["code"], "insufficient_scope", "{body}");
    assert!(
        text.contains("principal not authorized to write ada/open.git"),
        "{text}"
    );
}

#[tokio::test]
async fn bot_mcp_hides_admin_tools_and_denies_an_admin_call() {
    use axum::http::Request as HttpRequest;

    let core = jeryu_core::ForgeCore::new();
    core.create_account("ada", "ada-password-1", UserRole::Admin)
        .unwrap();
    let enrolled = core
        .enroll_bot("ada", "atlas", "Atlas", BotKind::Grok, BotReach::General)
        .unwrap();
    let router = router_for(core);
    let minted = access_token(&router, &enrolled.enrollment_key, "http://127.0.0.1").await;
    let token = minted["access_token"].as_str().unwrap().to_string();

    let post = |method: &'static str, session: Option<String>, body: Value| {
        let token = token.clone();
        let mut request = HttpRequest::builder()
            .method("POST")
            .uri("/mcp/bot")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::HOST, "127.0.0.1")
            .header(header::CONTENT_TYPE, "application/json")
            .header("Mcp-Method", method);
        if let Some(session) = session {
            request = request
                .header("Mcp-Session-Id", session)
                .header("MCP-Protocol-Version", jeryu_mcp::MCP_PROTOCOL_VERSION)
                .header("Mcp-Name", "jeryu.control_plane.status");
        }
        request.body(Body::from(body.to_string())).unwrap()
    };
    let initialized = router
        .clone()
        .oneshot(post(
            "initialize",
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": jeryu_mcp::MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "grokbot", "version": "1" }
                }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(initialized.status(), StatusCode::OK);
    let session = initialized
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|value| value.to_str().ok())
        .expect("session")
        .to_string();

    let listed = router
        .clone()
        .oneshot(post(
            "tools/list",
            Some(session.clone()),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
        ))
        .await
        .unwrap();
    let status = listed.status();
    let bytes = axum::body::to_bytes(listed.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        names.contains(&"jeryu.code.definition"),
        "repo tools stay listed: {names:?}"
    );
    assert!(
        names
            .iter()
            .all(|name| !name.contains("control_plane.") && !name.contains("agent_work.")),
        "admin tools stay hidden: {names:?}"
    );

    let called = router
        .oneshot(post(
            "tools/call",
            Some(session),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "jeryu.control_plane.status",
                    "arguments": {}
                }
            }),
        ))
        .await
        .unwrap();
    let status = called.status();
    let bytes = axum::body::to_bytes(called.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["isError"], json!(true), "{body}");
    assert_eq!(
        body["result"]["structuredContent"]["message"],
        json!("control_plane.status requires the admin role")
    );
}

#[tokio::test]
async fn bot_session_rejects_args_env_and_url() {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("ada", "ada-password-1", UserRole::Admin)
        .unwrap();
    core.create_repository("ada", repo("widgets", true))
        .unwrap();
    core.grant_repo_access("ada", "ada", "ada", "widgets", RepoAccessLevel::Write)
        .unwrap();
    let enrolled = core
        .enroll_bot("ada", "atlas", "Atlas", BotKind::Grok, BotReach::General)
        .unwrap();
    let router = router_for(core.clone());
    let minted = access_token(&router, &enrolled.enrollment_key, "http://127.0.0.1").await;
    let access = minted["access_token"].as_str().unwrap();
    let bodies = [
        (
            r#"{"provider":"local","task":"look","args":["/bin/sh"]}"#,
            "/bin/sh",
        ),
        (
            r#"{"provider":"local","task":"look","env":{"PATH":"/tmp/secret-env"}}"#,
            "secret-env",
        ),
        (
            r#"{"provider":"local","task":"look","url":"http://evil.example/hook"}"#,
            "evil.example",
        ),
    ];
    for (body, secret) in bodies {
        let (status, parsed, text) = send(
            &router,
            HttpMethod::POST,
            "/api/v1/repos/ada/widgets/sessions",
            Some(access),
            Some("127.0.0.1"),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert_eq!(parsed["code"], "invalid_input", "{parsed}");
        assert_eq!(
            parsed["message"],
            "a bot session cannot set the command, args, env, or base url"
        );
        assert!(!text.contains(secret), "{text}");
    }
}

/// The unit-test signer in `bot_token`. Production never uses this key.
const TEST_SIGNING_KEY: &[u8] = b"jeryu-bot-test-key-32-bytes!!!!!";

fn tamper_signature(token: &str) -> String {
    let (input, signature) = token.rsplit_once('.').expect("jwt");
    let original = URL_SAFE_NO_PAD
        .decode(signature)
        .expect("signature decodes");
    let mut bytes = signature.as_bytes().to_vec();
    bytes[0] = if bytes[0] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(bytes).unwrap();
    let decoded = URL_SAFE_NO_PAD
        .decode(tampered.as_bytes())
        .expect("tampered signature still decodes");
    assert_ne!(decoded, original);
    format!("{input}.{tampered}")
}

fn token_signed_as(token: &str, alg: &str) -> String {
    let mut parts = token.split('.');
    let _header = parts.next().expect("header");
    let payload = parts.next().expect("payload");
    assert!(parts.next().is_some(), "signature");
    assert!(parts.next().is_none());
    let header = URL_SAFE_NO_PAD.encode(format!(r#"{{"alg":"{alg}","typ":"JWT"}}"#));
    let signing_input = format!("{header}.{payload}");
    let mut mac = Hmac::<Sha256>::new_from_slice(TEST_SIGNING_KEY).unwrap();
    mac.update(signing_input.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{signing_input}.{signature}")
}

#[tokio::test]
async fn hmac_tampered_bot_token_is_refused() {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("ada", "ada-password-1", UserRole::User)
        .unwrap();
    let enrolled = core
        .enroll_bot("ada", "atlas", "Atlas", BotKind::Grok, BotReach::General)
        .unwrap();
    let router = router_for(core);
    let minted = access_token(&router, &enrolled.enrollment_key, "http://127.0.0.1").await;
    let access = minted["access_token"].as_str().unwrap().to_string();
    let (status, who, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        Some(&access),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{who}");

    let tampered = tamper_signature(&access);
    let (status, body, text) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/bot/whoami",
        Some(&tampered),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "invalid_token");
    assert!(!text.contains(&access));
    assert!(!text.contains(&tampered));
}

#[tokio::test]
async fn non_hs256_bot_token_is_refused() {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("ada", "ada-password-1", UserRole::User)
        .unwrap();
    let enrolled = core
        .enroll_bot("ada", "atlas", "Atlas", BotKind::Grok, BotReach::General)
        .unwrap();
    let router = router_for(core);
    let minted = access_token(&router, &enrolled.enrollment_key, "http://127.0.0.1").await;
    let access = minted["access_token"].as_str().unwrap().to_string();
    for alg in ["none", "RS256"] {
        let forged = token_signed_as(&access, alg);
        let (status, body, text) = send(
            &router,
            HttpMethod::GET,
            "/api/v1/bot/whoami",
            Some(&forged),
            Some("127.0.0.1"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{alg} {body}");
        assert_eq!(body["code"], "invalid_token", "{alg}");
        assert!(!text.contains(&forged), "{alg}");
    }
}

#[tokio::test]
async fn agent_tokens_act_as_the_bot_on_rest_and_one_mcp() {
    use axum::http::Request as HttpRequest;

    let core = jeryu_core::ForgeCore::new();
    core.create_account("dee", "dee-password-1", UserRole::Admin)
        .unwrap();
    core.create_repository("dee", repo("widgets", true))
        .unwrap();
    core.create_repository("dee", repo("vault", true)).unwrap();
    core.grant_repo_access("dee", "dee", "dee", "widgets", RepoAccessLevel::Write)
        .unwrap();
    let bot = core
        .enroll_bot(
            "dee",
            "atlas",
            "Atlas",
            BotKind::Grok,
            BotReach::Repositories {
                repos: vec![BotRepoRef {
                    owner: "dee".to_string(),
                    name: "widgets".to_string(),
                }],
            },
        )
        .unwrap();
    let owner_pat = core
        .create_personal_access_token("dee", "owner", None)
        .unwrap()
        .secret;
    let router = router_for(core);
    let tokens = format!("/api/v1/bots/{}/tokens", bot.bot.id);

    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        &tokens,
        Some(&owner_pat),
        Some("127.0.0.1"),
        Some(r#"{"label":"laptop","ttl_days":30}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let agent = body["token"].as_str().unwrap().to_string();
    let token_id = body["token_id"].as_str().unwrap().to_string();
    assert!(agent.starts_with(jeryu_core::AGENT_TOKEN_PREFIX));

    // REST: the bot's grant, nothing more; a forge admin owner does not widen it.
    for (uri, expected) in [
        ("/api/v1/repos/dee%2Fwidgets", StatusCode::OK),
        ("/api/v1/repos/dee%2Fvault", StatusCode::FORBIDDEN),
    ] {
        let (status, body, _) = send(
            &router,
            HttpMethod::GET,
            uri,
            Some(&agent),
            Some("127.0.0.1"),
            None,
        )
        .await;
        assert_eq!(status, expected, "{uri}: {body}");
    }
    // An agent token cannot mint more tokens.
    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        &tokens,
        Some(&agent),
        Some("127.0.0.1"),
        Some("{}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // MCP on the one endpoint: no proxy, no exchange, admin tools hidden.
    let mcp = |token: &str, forwarded: bool, method: &str, session: Option<&str>, body: Value| {
        let mut request = HttpRequest::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::HOST, "127.0.0.1")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header("Mcp-Method", method.to_string());
        if forwarded {
            request = request.header("X-Forwarded-For", "203.0.113.7");
        }
        if let Some(session) = session {
            request = request
                .header("Mcp-Session-Id", session.to_string())
                .header("MCP-Protocol-Version", jeryu_mcp::MCP_PROTOCOL_VERSION);
        }
        request.body(Body::from(body.to_string())).unwrap()
    };
    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": jeryu_mcp::MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "agent", "version": "1" }
        }
    });
    let initialized = router
        .clone()
        .oneshot(mcp(&agent, true, "initialize", None, initialize.clone()))
        .await
        .unwrap();
    assert_eq!(initialized.status(), StatusCode::OK);
    let session = initialized
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|value| value.to_str().ok())
        .expect("session")
        .to_string();
    let listed = router
        .clone()
        .oneshot(mcp(
            &agent,
            true,
            "tools/list",
            Some(&session),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        ))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(listed.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(!names.is_empty(), "{body}");
    assert!(names.contains(&"jeryu.tool_registry.summary"));
    let admin: Vec<_> = names
        .iter()
        .filter(|name| super::super::mcp_backend::is_admin_tool(name))
        .collect();
    assert!(
        admin.is_empty(),
        "admin tools offered to an agent token: {admin:?}"
    );
    // Named too, so a tool reclassified by mistake still fails here.
    for admin_tool in [
        "jeryu.agent_work.start",
        "jeryu.agent_work.control",
        "jeryu.tool_finder.scan",
        "jeryu.control_plane.status",
        "jeryu.get_system_snapshot",
        "jeryu.runner_fabric.status",
    ] {
        assert!(
            !names.contains(&admin_tool),
            "{admin_tool} offered to an agent token"
        );
    }

    // A standard client sends no routing headers and an older protocol
    // version: the one endpoint still answers, offering its own version.
    let plain = router
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {agent}"))
                .header(header::HOST, "127.0.0.1")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0", "id": 9, "method": "initialize",
                        "params": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "clientInfo": { "name": "standard", "version": "1" }
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(plain.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(plain.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["result"]["protocolVersion"],
        json!(jeryu_mcp::MCP_PROTOCOL_VERSION),
        "{body}"
    );

    // An unscoped personal access token is refused on MCP, with or without a
    // proxy header: the decision trusts no request header.
    for forwarded in [true, false] {
        let refused = router
            .clone()
            .oneshot(mcp(
                &owner_pat,
                forwarded,
                "initialize",
                None,
                initialize.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(
            refused.status(),
            StatusCode::FORBIDDEN,
            "forwarded={forwarded}"
        );
    }

    // An agent token cannot list or revoke tokens, and ttl_days is bounded.
    for (method, uri) in [
        (HttpMethod::GET, tokens.clone()),
        (HttpMethod::POST, format!("{tokens}/{token_id}/revoke")),
    ] {
        let (status, body, _) =
            send(&router, method, &uri, Some(&agent), Some("127.0.0.1"), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
    }
    for ttl in ["0", "366", "-1", "4294967296"] {
        let (status, body, _) = send(
            &router,
            HttpMethod::POST,
            &tokens,
            Some(&owner_pat),
            Some("127.0.0.1"),
            Some(&format!(r#"{{"ttl_days":{ttl}}}"#)),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "ttl {ttl}: {body}"
        );
    }

    // The owner sees the token's metadata, never the token, and can revoke it.
    let (status, body, text) = send(
        &router,
        HttpMethod::GET,
        &tokens,
        Some(&owner_pat),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tokens"][0]["token_id"], json!(token_id));
    assert!(!text.contains(&agent));
    let (status, body, _) = send(
        &router,
        HttpMethod::POST,
        &format!("{tokens}/{token_id}/revoke"),
        Some(&owner_pat),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        "/api/v1/repos/dee%2Fwidgets",
        Some(&agent),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn token_routes_answer_only_the_owner_and_keep_no_secret() {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("eve", "eve-password-1", UserRole::User)
        .unwrap();
    core.create_account("finn", "finn-password-1", UserRole::User)
        .unwrap();
    core.create_repository("eve", repo("widgets", true))
        .unwrap();
    core.grant_repo_access("eve", "eve", "eve", "widgets", RepoAccessLevel::Write)
        .unwrap();
    let bot = core
        .enroll_bot("eve", "atlas", "Atlas", BotKind::Muse, BotReach::General)
        .unwrap();
    let eve = core
        .create_personal_access_token("eve", "owner", None)
        .unwrap()
        .secret;
    let finn = core
        .create_personal_access_token("finn", "other", None)
        .unwrap()
        .secret;
    let router = router_for(core);
    let tokens = format!("/api/v1/bots/{}/tokens", bot.bot.id);

    // A non-admin owner manages its own bot's tokens; another account cannot see them.
    for (method, uri) in [
        (HttpMethod::GET, tokens.clone()),
        (HttpMethod::POST, tokens.clone()),
    ] {
        let (status, body, _) = send(
            &router,
            method,
            &uri,
            Some(&finn),
            Some("127.0.0.1"),
            Some("{}"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
    }

    // The same Idempotency-Key twice runs the write once. The first answer holds
    // the token; the kept answer is a placeholder, so the secret is never stored
    // and a retry gets 409 instead of a second token.
    let mut answers = Vec::new();
    for _ in 0..2 {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(HttpMethod::POST)
                    .uri(&tokens)
                    .header(header::AUTHORIZATION, format!("Bearer {eve}"))
                    .header(header::HOST, "127.0.0.1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Idempotency-Key", "issue-once")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        answers.push((status, String::from_utf8_lossy(&bytes).to_string()));
    }
    assert_eq!(answers[0].0, StatusCode::CREATED, "{}", answers[0].1);
    let first: Value = serde_json::from_str(&answers[0].1).unwrap();
    let token = first["token"].as_str().unwrap();
    assert_eq!(answers[1].0, StatusCode::CONFLICT, "{}", answers[1].1);
    assert!(answers[1].1.contains("idempotent_secret_withheld"));
    assert!(!answers[1].1.contains(token));
    let (status, body, _) = send(
        &router,
        HttpMethod::GET,
        &tokens,
        Some(&eve),
        Some("127.0.0.1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tokens"].as_array().unwrap().len(), 1, "{body}");
}

#[tokio::test]
async fn local_dev_trust_is_withheld_from_proxied_requests() {
    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;

    let core = jeryu_core::ForgeCore::new();
    let router = app(
        WebState::new(core).with_auth(true, true, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let loopback: SocketAddr = "127.0.0.1:40000".parse().unwrap();
    let me = |proxy_header: Option<(&str, &str)>| {
        let mut request = Request::builder()
            .uri("/api/v1/auth/me")
            .header(header::HOST, "127.0.0.1")
            .extension(ConnectInfo(loopback));
        if let Some((name, value)) = proxy_header {
            request = request.header(name, value);
        }
        request.body(Body::empty()).unwrap()
    };
    let local = router.clone().oneshot(me(None)).await.unwrap();
    assert_eq!(local.status(), StatusCode::OK);
    for header_pair in [
        ("X-Forwarded-For", "203.0.113.7"),
        ("Forwarded", "for=203.0.113.7"),
        ("X-Real-IP", "203.0.113.7"),
    ] {
        let proxied = router.clone().oneshot(me(Some(header_pair))).await.unwrap();
        assert_eq!(
            proxied.status(),
            StatusCode::UNAUTHORIZED,
            "{} must withhold local trust",
            header_pair.0
        );
    }
}

#[test]
fn local_trust_and_mcp_peer_list_reject_proxies_and_unspecified_peers() {
    use std::net::SocketAddr;

    let state = WebState::new(jeryu_core::ForgeCore::new()).with_auth(true, true, false);
    let loopback: SocketAddr = "127.0.0.1:40000".parse().unwrap();
    let plain = axum::http::HeaderMap::new();
    assert!(super::super::auth::local_dev_trusted(
        &state,
        Some(loopback),
        &plain
    ));
    for name in [
        "forwarded",
        "via",
        "x-forwarded-for",
        "x-forwarded-proto",
        "x-forwarded-port",
        "x-real-ip",
        "x-client-ip",
        "true-client-ip",
        "cf-connecting-ip",
        "x-envoy-external-address",
    ] {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::HeaderName::from_static(name),
            axum::http::HeaderValue::from_static("203.0.113.7"),
        );
        assert!(
            !super::super::auth::local_dev_trusted(&state, Some(loopback), &headers),
            "{name} must withhold local trust"
        );
    }

    let (peers, rejected) = super::super::auth::parse_mcp_personal_token_peers(
        "127.0.0.1, 0.0.0.0, ::, 127.0.0.0/8, nope, ::1",
    );
    assert_eq!(
        peers,
        vec![
            "127.0.0.1".parse::<std::net::IpAddr>().unwrap(),
            "::1".parse().unwrap()
        ]
    );
    assert_eq!(rejected, vec!["0.0.0.0", "::", "127.0.0.0/8", "nope"]);
}
