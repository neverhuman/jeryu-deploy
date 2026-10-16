//! Capability discovery answers for the credential that asked.
//!
//! The manifest is open, so it is served to an anonymous caller too — but what
//! it recommends is never a tool that caller would be refused. These are the
//! conformance tests for the three views (anonymous, scoped agent, admin), for
//! a tool no adapter runs, and for a repository outside an agent's reach.

use super::*;
use axum::body::Body;
use axum::http::Request;
use jeryu_core::{BotKind, BotReach, BotRepoRef};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Tokens for the three principals, against one seeded forge.
struct Forge {
    router: AxumRouter,
    admin_token: String,
    user_token: String,
    agent_token: String,
}

/// The repository the agent's bot is granted, and the one it is not.
const IN_REACH: &str = "ada/widgets";
const OUT_OF_REACH: &str = "ada/vault";

async fn seeded_forge(spa: &Path) -> Forge {
    write_file(spa, "index.html", "<!doctype html><title>jeryu</title>");
    let core = ForgeCore::new();
    core.create_account("ada", "ada-password-1", UserRole::Admin)
        .expect("admin account");
    core.create_account("bob", "bob-password-1", UserRole::User)
        .expect("user account");
    for name in ["widgets", "vault"] {
        core.create_repository(
            "ada",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: true,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .expect("repository");
    }
    // The bot's reach may only name repositories its owner can reach.
    core.grant_repo_access("ada", "ada", "ada", "widgets", RepoAccessLevel::Write)
        .expect("owner grant");
    let bot = core
        .enroll_bot(
            "ada",
            "atlas",
            "Atlas",
            BotKind::GROK,
            BotReach::Repositories {
                repos: vec![BotRepoRef {
                    owner: "ada".to_string(),
                    name: "widgets".to_string(),
                }],
            },
        )
        .expect("bot");
    let admin_token = core
        .create_personal_access_token("ada", "admin", None)
        .expect("admin token")
        .secret;
    let user_token = core
        .create_personal_access_token("bob", "user", None)
        .expect("user token")
        .secret;
    let router = app(WebState::new(core).with_auth(true, false, false), spa);
    let issued = send(
        &router,
        HttpMethod::POST,
        &format!("/api/v1/bots/{}/tokens", bot.bot.id),
        Some(&admin_token),
        Some(r#"{"label":"conformance","ttl_days":7}"#),
    )
    .await;
    assert_eq!(issued.0, StatusCode::CREATED, "{}", issued.1);
    let agent_token = issued.1["token"].as_str().expect("agent token").to_string();
    Forge {
        router,
        admin_token,
        user_token,
        agent_token,
    }
}

async fn send(
    router: &AxumRouter,
    method: HttpMethod,
    uri: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1")
        .header(header::USER_AGENT, "GitHub CLI 2.40.0 go-gh/2.0");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let response = router
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.unwrap_or("").to_string()))
                .expect("request"),
        )
        .await
        .expect("router answered");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read body");
    let value = serde_json::from_slice(&bytes).unwrap_or_else(|err| {
        panic!(
            "{uri} answered JSON: {err}: {}",
            String::from_utf8_lossy(&bytes)
        )
    });
    (status, value)
}

async fn manifest(router: &AxumRouter, token: Option<&str>) -> Value {
    let (status, body) = send(
        router,
        HttpMethod::GET,
        crate::discovery::CAPABILITIES_PATH,
        token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|item| item.as_str().map(str::to_string))
        .collect()
}

/// Every `jeryu.*` tool name anywhere in a payload, however it is nested. The
/// `jeryu.*` wildcard prose says "the tools listed here" and names none, so it
/// is not one.
fn named_tools(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            for word in text.split([' ', ',', ';', '"', '(', ')']) {
                let word = word.trim_end_matches(['.', ':']);
                if word.starts_with("jeryu.") && !word.contains('*') {
                    found.push(word.to_string());
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| named_tools(item, found)),
        Value::Object(fields) => fields.values().for_each(|item| named_tools(item, found)),
        _ => {}
    }
}

/// Every MCP tool an action recommends: the rest are REST routes.
fn recommended_tools(manifest: &Value) -> Vec<String> {
    manifest["recommended_next_actions"]
        .as_array()
        .expect("recommended actions")
        .iter()
        .filter_map(|action| action["action"].as_str())
        .filter(|action| action.starts_with("jeryu."))
        .map(str::to_string)
        .collect()
}

/// No surface may name a tool this forge does not dispatch, whatever view it
/// is answering: a recommendation the `/mcp` endpoint refuses as
/// `tool_unavailable` dead-ends the agent that follows it.
fn no_tool_without_an_adapter(label: &str, payload: &Value) {
    let mut named = Vec::new();
    named_tools(payload, &mut named);
    for tool in named {
        assert!(
            crate::tool_access::is_implemented(&tool),
            "{label} names {tool}, which has no adapter here"
        );
    }
}

#[tokio::test]
async fn capability_discovery_answers_each_principal_with_its_own_usable_tools() {
    let spa = tempdir().expect("spa dir");
    let forge = seeded_forge(spa.path()).await;
    let catalog: Vec<String> = strings(&manifest(&forge.router, None).await["mcp_tools"]);
    assert!(!catalog.is_empty(), "the server dispatches tools");

    // Anonymous: discovery answers, but nothing is recommended that a caller
    // without a credential would be refused.
    let anonymous = manifest(&forge.router, None).await;
    assert_eq!(anonymous["principal"]["kind"], "anonymous");
    assert_eq!(anonymous["principal"]["authenticated"], false);
    assert_eq!(strings(&anonymous["mcp_tools"]), catalog);
    assert!(
        strings(&anonymous["your_mcp_tools"]).is_empty(),
        "an anonymous caller is offered tools it cannot call: {}",
        anonymous["your_mcp_tools"]
    );
    assert!(recommended_tools(&anonymous).is_empty());
    assert!(
        anonymous["recommended_next_actions"]
            .to_string()
            .contains("agent token"),
        "{}",
        anonymous["recommended_next_actions"]
    );
    no_tool_without_an_adapter("the anonymous manifest", &anonymous);

    // A scoped agent token: the non-admin tools, and only those.
    let agent = manifest(&forge.router, Some(&forge.agent_token)).await;
    assert_eq!(agent["principal"]["kind"], "agent");
    assert_eq!(agent["principal"]["authenticated"], true);
    let agent_tools = strings(&agent["your_mcp_tools"]);
    assert!(agent_tools.contains(&"jeryu.tool_registry.summary".to_string()));
    assert!(agent_tools.contains(&"jeryu.code.definition".to_string()));
    for tool in &agent_tools {
        assert_eq!(
            agent["mcp_tool_access"][tool].as_str(),
            crate::tool_access::required_access(tool).map(|access| access.as_str()),
            "{tool} is published with the wrong grant"
        );
        assert_ne!(
            agent["mcp_tool_access"][tool], "admin",
            "{tool} is admin-only and offered to an agent token"
        );
    }
    for tool in recommended_tools(&agent) {
        assert!(
            agent_tools.contains(&tool),
            "the agent is told to call {tool}, which it may not"
        );
    }
    no_tool_without_an_adapter("the agent manifest", &agent);

    // A plain account reaches the same tools as an agent token: neither is a
    // forge admin.
    let user = manifest(&forge.router, Some(&forge.user_token)).await;
    assert_eq!(user["principal"]["kind"], "user");
    assert_eq!(strings(&user["your_mcp_tools"]), agent_tools);

    // An admin reaches every dispatched tool, the admin-only ones included.
    let admin = manifest(&forge.router, Some(&forge.admin_token)).await;
    assert_eq!(admin["principal"]["kind"], "admin");
    assert_eq!(strings(&admin["your_mcp_tools"]), catalog);
    assert!(
        strings(&admin["your_mcp_tools"]).len() > agent_tools.len(),
        "the admin view is not wider than the agent view"
    );
    assert!(recommended_tools(&admin).contains(&"jeryu.control_plane.status".to_string()));
    no_tool_without_an_adapter("the admin manifest", &admin);

    // Whoever asks, the manifest describes the same GraphQL subset the edge
    // serves, and names the scoped agent token it takes.
    for view in [&anonymous, &agent, &admin] {
        let operations = strings(&view["graphql_support"]["supported_operations"]);
        assert!(operations.contains(&"query { viewer { login } }".to_string()));
        assert_eq!(
            operations,
            crate::github::GRAPHQL_SUPPORTED_OPERATIONS
                .iter()
                .map(|operation| (*operation).to_string())
                .collect::<Vec<_>>()
        );
        let note = view["graphql_support"]["auth"]
            .as_str()
            .expect("graphql auth note");
        assert!(note.contains("personal access token"), "{note}");
        assert!(note.contains("insufficient_scope"), "{note}");
        // The plain string field still answers for clients that read it.
        assert_eq!(view["graphql"], "/graphql");
    }
}

/// The guided bodies the GitHub edge serves carry no principal of their own,
/// so they name only tools every authenticated caller may use — and never one
/// without an adapter.
#[tokio::test]
async fn guided_error_bodies_name_only_tools_any_caller_may_use() {
    let spa = tempdir().expect("spa dir");
    let forge = seeded_forge(spa.path()).await;
    let (status, body) = send(
        &forge.router,
        HttpMethod::GET,
        "/repos/ada/widgets/no-such-thing",
        Some(&forge.agent_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    no_tool_without_an_adapter("the edge's 404", &body);
    let offered = strings(&body["jeryu_mcp_tools"]);
    assert!(!offered.is_empty());
    for tool in &offered {
        assert_ne!(
            crate::tool_access::required_access(tool),
            Some(crate::tool_access::ToolAccess::Admin),
            "{tool} is admin-only and offered to whoever asks"
        );
    }
    // The steering names the supported route rather than a dead MCP tool.
    assert_eq!(body["jeryu_steering"]["mcp_tool"], Value::Null);
    assert_eq!(body["jeryu_steering"]["supported_route"], "GET /api/v3");
}

/// An unsupported GraphQL request is answered with the REST routes that serve
/// the intent and the subset that is served, never with a dead MCP tool.
#[tokio::test]
async fn unsupported_graphql_answers_with_routes_and_the_served_subset() {
    let spa = tempdir().expect("spa dir");
    let forge = seeded_forge(spa.path()).await;
    // GraphQL takes an account credential; a scoped agent token is refused
    // here, which is what the manifest and the docs say.
    let (refused, _) = send(
        &forge.router,
        HttpMethod::POST,
        "/graphql",
        Some(&forge.agent_token),
        Some(r#"{"query":"query { viewer { login } }"}"#),
    )
    .await;
    assert_eq!(refused, StatusCode::FORBIDDEN);

    let (status, body) = send(
        &forge.router,
        HttpMethod::POST,
        "/graphql",
        Some(&forge.admin_token),
        Some(r#"{"query":"mutation { addStar(input: { starrableId: \"R_1\" }) { starrable { id } } }"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    no_tool_without_an_adapter("the graphql refusal", &body);
    assert_eq!(
        strings(&body["graphql_supported_operations"]),
        crate::github::GRAPHQL_SUPPORTED_OPERATIONS
            .iter()
            .map(|operation| (*operation).to_string())
            .collect::<Vec<_>>()
    );
    assert!(
        strings(&body["jeryu_api_routes"])
            .iter()
            .any(|route| route.starts_with("GET /repos")),
        "{body}"
    );

    // Both advertised GraphQL paths answer for the caller: a repository the
    // account may not read is absent, not described.
    for path in ["/graphql", "/api/v3/graphql"] {
        let (status, hidden) = send(
            &forge.router,
            HttpMethod::POST,
            path,
            Some(&forge.user_token),
            Some(
                r#"{"query":"query { repository(owner: \"ada\", name: \"vault\") { nameWithOwner isPrivate } }"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {hidden}");
        assert_eq!(
            hidden["data"]["repository"],
            Value::Null,
            "{path} described a repository the caller may not read"
        );
        // The owner, who may read it, does see it on the same path.
        let (status, seen) = send(
            &forge.router,
            HttpMethod::POST,
            path,
            Some(&forge.admin_token),
            Some(
                r#"{"query":"query { repository(owner: \"ada\", name: \"vault\") { nameWithOwner isPrivate } }"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {seen}");
        assert_eq!(seen["data"]["repository"]["nameWithOwner"], OUT_OF_REACH);
        // An agent token is refused on both.
        let (refused, _) = send(
            &forge.router,
            HttpMethod::POST,
            path,
            Some(&forge.agent_token),
            Some(r#"{"query":"query { viewer { login } }"}"#),
        )
        .await;
        assert_eq!(refused, StatusCode::FORBIDDEN, "{path}");
    }

    // The subset itself still answers, for the token that asked.
    let (status, viewer) = send(
        &forge.router,
        HttpMethod::POST,
        "/graphql",
        Some(&forge.admin_token),
        Some(r#"{"query":"query { viewer { login } }"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{viewer}");
    assert_eq!(viewer["data"]["viewer"]["login"], "ada");
}

/// One MCP session for `token`, with the tools it is offered.
async fn mcp_session(router: &AxumRouter, token: &str) -> (String, Vec<String>) {
    let request = |session: Option<&str>, body: Value| {
        let mut builder = Request::builder()
            .method(HttpMethod::POST)
            .uri("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::HOST, "127.0.0.1")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(session) = session {
            builder = builder
                .header("Mcp-Session-Id", session.to_string())
                .header("MCP-Protocol-Version", jeryu_mcp::MCP_PROTOCOL_VERSION);
        }
        builder.body(Body::from(body.to_string())).expect("request")
    };
    let initialized = router
        .clone()
        .oneshot(request(
            None,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": jeryu_mcp::MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "conformance", "version": "1" }
                }
            }),
        ))
        .await
        .expect("initialize answered");
    assert_eq!(initialized.status(), StatusCode::OK);
    let session = initialized
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|value| value.to_str().ok())
        .expect("session id")
        .to_string();
    let listed = router
        .clone()
        .oneshot(request(
            Some(&session),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        ))
        .await
        .expect("tools/list answered");
    let bytes = axum::body::to_bytes(listed.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read body");
    let body: Value = serde_json::from_slice(&bytes).expect("tools/list JSON");
    let tools = body["result"]["tools"]
        .as_array()
        .expect("tool list")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect();
    (session, tools)
}

async fn mcp_call(
    router: &AxumRouter,
    token: &str,
    session: &str,
    tool: &str,
    args: Value,
) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::HOST, "127.0.0.1")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header("Mcp-Session-Id", session.to_string())
                .header("MCP-Protocol-Version", jeryu_mcp::MCP_PROTOCOL_VERSION)
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                        "params": { "name": tool, "arguments": args }
                    })
                    .to_string(),
                ))
                .expect("tools/call request"),
        )
        .await
        .expect("tools/call answered");
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("tools/call JSON")
}

/// What the manifest promises an agent token is what `/mcp` gives it: the
/// tools it lists, a `tool_unavailable` for one no adapter runs, and a refusal
/// for a repository outside the bot's reach.
#[tokio::test]
async fn an_agent_token_gets_the_tools_its_manifest_promised() {
    let spa = tempdir().expect("spa dir");
    let forge = seeded_forge(spa.path()).await;
    let mut promised =
        strings(&manifest(&forge.router, Some(&forge.agent_token)).await["your_mcp_tools"]);
    let (session, mut offered) = mcp_session(&forge.router, &forge.agent_token).await;
    promised.sort();
    offered.sort();
    assert_eq!(
        offered, promised,
        "/mcp offers a different set than discovery"
    );

    // A tool the manifest never names, called anyway with arguments its
    // catalog schema accepts: refused as unavailable, not answered from a
    // stand-in.
    let unavailable = mcp_call(
        &forge.router,
        &forge.agent_token,
        &session,
        "jeryu.run_tests",
        json!({ "repo": 1, "target_ref": "main", "test_scope": "unit" }),
    )
    .await;
    assert_eq!(unavailable["result"]["isError"], true, "{unavailable}");
    assert_eq!(
        unavailable["result"]["structuredContent"]["data"]["code"],
        "tool_unavailable"
    );

    // A repository outside the bot's reach is refused; the one inside it is
    // not refused for access.
    let out_of_reach = mcp_call(
        &forge.router,
        &forge.agent_token,
        &session,
        "jeryu.code.definition",
        json!({ "repo": OUT_OF_REACH, "symbol": "CodeGraph" }),
    )
    .await;
    assert_eq!(out_of_reach["result"]["isError"], true, "{out_of_reach}");
    assert_eq!(
        out_of_reach["result"]["structuredContent"]["message"],
        format!("code.definition requires read access to {OUT_OF_REACH}")
    );
    let in_reach = mcp_call(
        &forge.router,
        &forge.agent_token,
        &session,
        "jeryu.code.definition",
        json!({ "repo": IN_REACH, "symbol": "CodeGraph" }),
    )
    .await;
    let message = in_reach["result"]["structuredContent"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !message.contains("requires read access"),
        "a granted repository was refused for access: {in_reach}"
    );

    // An admin tool is neither offered nor answered for this token.
    assert!(!offered.contains(&"jeryu.control_plane.status".to_string()));
    let denied = mcp_call(
        &forge.router,
        &forge.agent_token,
        &session,
        "jeryu.control_plane.status",
        json!({}),
    )
    .await;
    assert_eq!(denied["result"]["isError"], true, "{denied}");
    assert_eq!(
        denied["result"]["structuredContent"]["message"],
        "control_plane.status requires the admin role"
    );
}
