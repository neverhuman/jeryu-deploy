//! The `jeryu.pull.workflow_status` tool against its native route.
//!
//! One agent token, two transports: the tool answers with the same head-bound
//! `workflow` block `GET /api/v1/repos/:id/pulls/:number` serves, and a
//! repository outside the token's grant is refused on both.

use std::path::Path;

use axum::body::Body;
use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::{
    BotKind, BotReach, BotRepoRef, CheckConclusion, CreateCheckRunRequest,
    CreatePullRequestRequest, CreateRepositoryRequest, RepoAccessLevel, SetBranchProtectionRequest,
    UserRole,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{WebState, app};

const HEAD_SHA: &str = "3333333333333333333333333333333333333333";

struct Forge {
    router: axum::Router,
    agent: String,
}

/// An admin-owned forge with two private repositories, a pull request in each,
/// and an agent token whose bot reaches only `dee/widgets`.
async fn forge() -> Forge {
    let core = jeryu_core::ForgeCore::new();
    core.create_account("dee", "dee-password-1", UserRole::Admin)
        .unwrap();
    for name in ["widgets", "vault"] {
        core.create_repository(
            "dee",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: true,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
        core.set_branch_protection(
            "dee",
            name,
            "main",
            SetBranchProtectionRequest {
                required_status_checks: vec!["ci/required".to_string()],
                required_approving_review_count: 1,
                ..SetBranchProtectionRequest::default()
            },
        )
        .unwrap();
        core.create_pull_request(
            "dee",
            name,
            "dee",
            CreatePullRequestRequest {
                title: format!("{name} change"),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some(HEAD_SHA.to_string()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    }
    // Evidence on the head, so the block has a required check to report.
    core.create_check_run(
        "dee",
        "widgets",
        CreateCheckRunRequest {
            name: "ci/required".to_string(),
            head_sha: HEAD_SHA.to_string(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Success),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    // The owner's own grant on the repository the bot's reach names: reach can
    // never widen past the account behind it.
    core.grant_repo_access("dee", "dee", "dee", "widgets", RepoAccessLevel::Write)
        .unwrap();
    let bot = core
        .enroll_bot(
            "dee",
            "atlas",
            "Atlas",
            BotKind::GROK,
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
    let router = app(
        WebState::new(core).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let (status, body) = rest(
        &router,
        HttpMethod::POST,
        &format!("/api/v1/bots/{}/tokens", bot.bot.id),
        &owner_pat,
        Some(r#"{"label":"laptop","ttl_days":30}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Forge {
        router,
        agent: body["token"].as_str().unwrap().to_string(),
    }
}

async fn rest(
    router: &axum::Router,
    method: HttpMethod,
    uri: &str,
    token: &str,
    body: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::HOST, "127.0.0.1");
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

impl Forge {
    async fn mcp(&self, session: Option<&str>, body: Value) -> (StatusCode, Value, Option<String>) {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {}", self.agent))
            .header(header::HOST, "127.0.0.1")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(session) = session {
            builder = builder
                .header("Mcp-Session-Id", session.to_string())
                .header("MCP-Protocol-Version", jeryu_mcp::MCP_PROTOCOL_VERSION);
        }
        let response = self
            .router
            .clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let session = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            session,
        )
    }

    /// An initialized MCP session on the agent token.
    async fn session(&self) -> String {
        let (status, body, session) = self
            .mcp(
                None,
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": jeryu_mcp::MCP_PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": { "name": "agent", "version": "1" }
                    }
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        session.expect("session id")
    }

    async fn call(&self, session: &str, arguments: Value) -> Value {
        let (status, body, _) = self
            .mcp(
                Some(session),
                json!({
                    "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                    "params": { "name": "jeryu.pull.workflow_status", "arguments": arguments }
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }
}

#[tokio::test]
async fn an_agent_token_reads_the_same_workflow_block_on_mcp_and_rest() {
    let forge = forge().await;
    let session = forge.session().await;

    // Discovery: the tool is offered to the agent token, read-only.
    let (status, listed, _) = forge
        .mcp(
            Some(&session),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let descriptor = listed["result"]["tools"]
        .as_array()
        .expect("tool list")
        .iter()
        .find(|tool| tool["name"] == "jeryu.pull.workflow_status")
        .unwrap_or_else(|| panic!("workflow status tool not offered: {listed}"));
    assert_eq!(descriptor["annotations"]["readOnlyHint"], true);

    // REST, as the same token.
    let (status, detail) = rest(
        &forge.router,
        HttpMethod::GET,
        "/api/v1/repos/dee%2Fwidgets/pulls/1",
        &forge.agent,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let native = detail["workflow"].clone();
    assert_eq!(native["head_sha"], HEAD_SHA, "{native}");
    assert_eq!(native["outcome"], "review_needed", "{native}");
    assert_eq!(native["required_checks"][0]["state"], "passing");

    // MCP, same token, same pull request: the same block, head-bound.
    let answered = forge
        .call(&session, json!({ "repo": "dee/widgets", "number": 1 }))
        .await;
    assert_eq!(answered["result"]["isError"], false, "{answered}");
    let data = &answered["result"]["structuredContent"]["data"];
    assert_eq!(data["repo"], "dee/widgets", "{answered}");
    assert_eq!(data["number"], 1);
    assert_eq!(data["workflow"], native, "{answered}");
}

#[tokio::test]
async fn a_repository_outside_the_grant_is_refused_on_both_transports() {
    let forge = forge().await;
    let session = forge.session().await;

    let (status, body) = rest(
        &forge.router,
        HttpMethod::GET,
        "/api/v1/repos/dee%2Fvault/pulls/1",
        &forge.agent,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.get("workflow").is_none(), "{body}");

    let refused = forge
        .call(&session, json!({ "repo": "dee/vault", "number": 1 }))
        .await;
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    let structured = &refused["result"]["structuredContent"];
    assert_eq!(structured["success"], false, "{refused}");
    assert_eq!(
        structured["message"], "pull.workflow_status requires read access to dee/vault",
        "{refused}"
    );
    assert!(structured["data"].is_null(), "{refused}");
}

#[tokio::test]
async fn unsupported_arguments_and_absent_pull_requests_fail_rather_than_answer() {
    let forge = forge().await;
    let session = forge.session().await;

    // The catalog schema refuses malformed arguments before any read.
    for arguments in [
        json!({}),
        json!({ "repo": "dee/widgets" }),
        json!({ "repo": "dee/widgets", "number": 0 }),
        json!({ "repo": "dee/widgets", "number": "1" }),
        json!({ "repo": "", "number": 1 }),
    ] {
        let response = forge.call(&session, arguments.clone()).await;
        assert_eq!(
            response["error"]["code"], -32602,
            "{arguments} answered {response}"
        );
    }

    // A pull request this repository does not have is an error, not an empty block.
    let missing = forge
        .call(&session, json!({ "repo": "dee/widgets", "number": 404 }))
        .await;
    assert_eq!(missing["result"]["isError"], true, "{missing}");
    assert_eq!(
        missing["result"]["structuredContent"]["message"],
        "pull.workflow_status: dee/widgets#404 not found",
        "{missing}"
    );
}
