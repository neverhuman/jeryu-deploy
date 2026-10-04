use super::*;

/// Seeds one repository, a global admin, a writer granted write on it, and a
/// reader with no grant at all.
fn feature_flag_state() -> WebState {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.create_account("jeryu-admin", "admin-password", UserRole::Admin)
        .unwrap();
    core.create_account("writer", "writer-password", UserRole::User)
        .unwrap();
    core.create_account("reader", "reader-password", UserRole::User)
        .unwrap();
    core.grant_repo_access(
        "jeryu-admin",
        "writer",
        "alice",
        "jeryu",
        RepoAccessLevel::Write,
    )
    .unwrap();
    WebState::new(core)
}

/// Seed a repo + open PR + one failing check, build `WebState`, and assert
/// the model served by `/api/v1/read-model/tui` (i.e. `state.tui`) reflects the
/// seeded load: a populated `RepoActivity` with `failed_jobs == 1`, a non-empty
/// pool fabric, and Healthy system components — NOT the empty fixture.
#[tokio::test]
async fn tui_read_model_reflects_seeded_repo_pr_and_failing_check() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    // An open PR so the repo counts as active work.
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "feature".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("deadbeef".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    // A completed check-run that FAILED — must surface as one failed job.
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "ci".to_string(),
            head_sha: "deadbeef".to_string(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Failure),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();

    let state = Arc::new(WebState::new(core));

    // The pool activity is genuinely populated, not the empty fixture.
    let activity = &state.tui.pool_activity;
    assert_eq!(activity.repos.len(), 1, "the seeded repo must be present");
    let repo = &activity.repos[0];
    assert_eq!(repo.repo, "alice/jeryu");
    assert_eq!(repo.failed_jobs, 1, "the failing check is one failed job");
    assert!(!activity.pools.is_empty(), "a default pool must roll up");
    assert_eq!(activity.pools[0].pool, "default");
    assert_eq!(activity.pools[0].failed_jobs, 1);

    // No component is probed, so health is Unknown rather than a blanket Healthy.
    assert!(matches!(state.tui.system.scm.status, HealthLevel::Unknown));

    // The actual `/api/v1/read-model/tui` handler serves exactly this model.
    let served = tui_read_model(State(state.clone())).await.0;
    assert_eq!(served.pool_activity, *activity);
    assert_eq!(served.pool_activity.repos[0].failed_jobs, 1);
    assert!(served.workcells.items.is_empty());
    // Sanity: this is NOT the empty default model.
    assert_ne!(
        served.pool_activity,
        TuiReadModel::default().pool_activity,
        "the TUI read model must not serve an empty pool activity"
    );
}

/// An empty server yields an empty pool fabric (Unknown health), and the
/// fixture sample remains available purely as a test fallback.
#[test]
fn empty_server_assembles_empty_pool_activity_and_fixture_still_available() {
    let model =
        crate::read_model::assemble_read_model(&[], &crate::read_model::FleetCapacity::default());
    assert!(model.pool_activity.repos.is_empty());
    assert!(model.pool_activity.pools.is_empty());
    assert!(matches!(model.pool_activity.health(), HealthLevel::Unknown));
    // The fixture is still reachable as a fallback. Its `pool_activity` is the
    // empty default — exactly why serving it left the Pools pane blank, which
    // is what the live assembler above now replaces.
    assert!(sample_read_model().pool_activity.pools.is_empty());
}

#[test]
fn bootstrap_and_repo_list_reflect_core_repositories() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: true,
            description: Some("forge".to_string()),
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let state = WebState::new(core);
    let bootstrap = bootstrap_payload(&state).expect("bootstrap serializes");
    assert_eq!(bootstrap.websocket_url, "/api/v1/ws");
    assert_eq!(bootstrap.recent_repositories.len(), 1);
    assert!(bootstrap.feature_flags.workcells);
    let repos = repo_list_response(&state);
    assert_eq!(repos.total, 1);
    assert_eq!(repos.repositories[0].id.owner, "alice");
}

#[test]
fn map_method_covers_supported_verbs_only() {
    assert!(matches!(map_method(&HttpMethod::GET), Some(Method::Get)));
    assert!(matches!(
        map_method(&HttpMethod::PATCH),
        Some(Method::Patch)
    ));
    assert!(matches!(map_method(&HttpMethod::POST), Some(Method::Post)));
    assert!(matches!(map_method(&HttpMethod::PUT), Some(Method::Put)));
    assert!(map_method(&HttpMethod::DELETE).is_none());
}

#[test]
fn app_router_builds_without_route_conflicts() {
    // Axum panics during construction on overlapping/ambiguous routes, so
    // building the full router is the regression guard for the REST mount,
    // the steering middleware layer, and the /.jeryu/capabilities route.
    let _app = app(
        WebState::new(ForgeCore::new()),
        std::path::Path::new("/tmp"),
    );
}

#[tokio::test]
async fn serve_rejects_trust_local_dev_on_public_bind() {
    let temp = tempfile::tempdir().unwrap();
    let config = WebServerConfig {
        bind: std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        spa_dir: temp.path().join("spa"),
        data_dir: temp.path().join("data"),
        git_storage_root: temp.path().join("git"),
        split_manifests: Vec::new(),
        auth_required: true,
        trust_local_dev: true,
        secure_cookies: false,
    };
    let err = serve(config).await.expect_err("public bind must fail");
    assert!(err.to_string().contains("trust_local_dev"));
}

/// The tool names the live `/mcp` backend dispatches for a default server.
fn installed_mcp_tools() -> BTreeSet<String> {
    live_mcp_tools(&Arc::new(WebState::new(ForgeCore::new())))
}

#[test]
fn capabilities_payload_exposes_the_gh_command_map() {
    let payload = capabilities_payload(&installed_mcp_tools());
    assert_eq!(payload["server"], "jeryu");
    assert_eq!(payload["api_version"], "v4");
    assert_eq!(payload["graphql"], "/graphql");
    assert_eq!(payload["websocket"], "/api/v1/ws");
    assert_eq!(payload["mcp_endpoint"], "/mcp");
    assert!(payload["fast_path_advice"].is_string());

    let map = &payload["gh_command_map"];
    for key in [
        "gh auth login",
        "gh auth refresh",
        "gh auth status",
        "gh pr list",
        "gh api",
        "gh repo create",
    ] {
        assert!(map.get(key).is_some(), "missing gh_command_map key: {key}");
    }
    assert_eq!(map["gh repo create"], "POST /repos");
    // The mutating tools have no execution adapter on a default server, so the
    // commands whose jeryu answer is one of them are not advertised at all.
    for (command, tool) in [
        ("gh pr create", MCP_PATCH_TOOL),
        ("gh pr merge", MCP_MERGE_TOOL),
        ("gh issue create", MCP_ISSUE_TOOL),
    ] {
        assert!(
            map.get(command).is_none(),
            "{command} is mapped to the uninstalled {tool}"
        );
    }
    assert!(
        map["gh auth login"]
            .as_str()
            .expect("gh auth login guidance")
            .contains("jeryu gh-setup")
    );
    assert_eq!(
        payload["gh_auth_policy"]["run_instead"],
        "jeryu gh-setup --host http://127.0.0.1:8787 --token-file ~/.jeryu/secrets/merge-token"
    );
    assert_eq!(
        payload["gh_auth_policy"]["token_file"],
        "~/.jeryu/secrets/merge-token"
    );
    assert!(
        payload["gh_auth_policy"]["stale_host_repair"]
            .as_str()
            .expect("stale host repair")
            .contains("--token-file ~/.jeryu/secrets/merge-token")
    );
    assert!(
        payload["gh_auth_policy"]["host_auth_boundary"]
            .as_str()
            .expect("host auth boundary")
            .contains("GitHub.com auth and local Jeryu host auth are separate")
    );
    assert!(!payload.to_string().contains("JERYU-TOKEN"));
}

/// The manifest is built from the live backend catalog, so a tool the `/mcp`
/// endpoint does not dispatch is advertised nowhere in it, neither in
/// `mcp_tools` nor as the jeryu answer to a `gh` command.
#[test]
fn capabilities_payload_omits_tools_the_backend_does_not_dispatch() {
    let tools = installed_mcp_tools();
    assert!(
        !tools.contains(MCP_MERGE_TOOL),
        "{MCP_MERGE_TOOL} has no execution adapter and must not be installed"
    );
    assert!(
        tools.contains(MCP_AGENT_WORK_TOOL),
        "the live backend is expected to dispatch {MCP_AGENT_WORK_TOOL}"
    );
    let payload = capabilities_payload(&tools);

    let advertised = payload["mcp_tools"].as_array().expect("mcp_tools array");
    assert!(
        !advertised.iter().any(|tool| tool == MCP_MERGE_TOOL),
        "an uninstalled tool is still advertised"
    );
    assert!(payload["gh_command_map"].get("gh pr merge").is_none());
    // A REST answer needs no tool, so it stays mapped either way.
    assert!(payload["gh_command_map"].get("gh pr list").is_some());
    // The CLI hint for the agent surface goes with that surface.
    assert!(payload["gh_auth_policy"].get("agent_auth").is_some());
    assert!(!payload.to_string().contains(MCP_MERGE_TOOL));

    // Dropping the installed agent-work tool drops its hint too.
    let mut without_agent_work = tools.clone();
    assert!(without_agent_work.remove(MCP_AGENT_WORK_TOOL));
    let payload = capabilities_payload(&without_agent_work);
    assert!(payload["gh_auth_policy"].get("agent_auth").is_none());
}

/// Every MCP tool the manifest names is one the live backend dispatches.
#[test]
fn capabilities_payload_only_names_installed_tools() {
    let installed = installed_mcp_tools();
    let payload = capabilities_payload(&installed);
    for tool in payload["mcp_tools"].as_array().expect("mcp_tools array") {
        let tool = tool.as_str().expect("tool name");
        assert!(installed.contains(tool), "uninstalled MCP tool: {tool}");
    }
    for (command, answer) in payload["gh_command_map"]
        .as_object()
        .expect("gh_command_map object")
    {
        let answer = answer.as_str().unwrap_or_default();
        if answer.starts_with("jeryu.") {
            assert!(
                installed.contains(answer),
                "{command} points at uninstalled tool {answer}"
            );
        }
    }
    assert!(payload["gh_auth_policy"]["agent_auth"].is_string());
}

#[test]
fn payload_serialization_errors_are_not_silently_replaced() {
    struct FailingSerialize;

    impl serde::Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(<S::Error as serde::ser::Error>::custom("synthetic failure"))
        }
    }

    assert!(serialize_payload(&FailingSerialize).is_err());
}

/// An admin sees every write surface, repository creation included.
#[test]
fn bootstrap_flags_open_every_write_surface_for_an_admin() {
    let state = feature_flag_state();
    let flags = bootstrap_payload_for_user(&state, &authenticated_admin_account("jeryu-admin").0)
        .expect("bootstrap serializes")
        .feature_flags;
    assert!(flags.repo_create);
    assert!(flags.settings_write);
    assert!(flags.merge_write);
    assert!(flags.agents);
}

/// A repository writer gets the repo-scoped write surfaces, but not the
/// forge-wide repository creation one.
#[test]
fn bootstrap_flags_follow_a_repository_write_grant() {
    let state = feature_flag_state();
    let flags = bootstrap_payload_for_user(&state, &authenticated_account("writer").0)
        .expect("bootstrap serializes")
        .feature_flags;
    assert!(!flags.repo_create, "creating repositories stays admin-only");
    assert!(flags.settings_write);
    assert!(flags.merge_write);
    assert!(flags.agents);
}

/// A viewer with no write grant anywhere sees the read-side surfaces only —
/// the write flags follow the grants, not a constant.
#[test]
fn bootstrap_flags_close_write_surfaces_without_a_grant() {
    let state = feature_flag_state();
    let flags = bootstrap_payload_for_user(&state, &authenticated_account("reader").0)
        .expect("bootstrap serializes")
        .feature_flags;
    assert!(!flags.repo_create);
    assert!(!flags.settings_write);
    assert!(!flags.merge_write);
    assert!(!flags.agents);
    assert!(flags.markdown_html);
    assert!(flags.mcp);
    assert!(flags.workcells);
}

/// An operator reading `/.jeryu/capabilities` can tell a flag that is off by
/// policy from one that is off for want of a grant.
#[test]
fn capabilities_payload_explains_every_bootstrap_feature_flag() {
    let payload = capabilities_payload(&installed_mcp_tools());
    let notes = payload["web_feature_flags"]["flags"]
        .as_object()
        .expect("feature flag notes object");
    for flag in [
        "repo_create",
        "settings_write",
        "merge_write",
        "markdown_html",
        "agents",
        "mcp",
        "workcells",
    ] {
        let note = notes[flag].as_str().unwrap_or_default();
        assert!(!note.is_empty(), "no operator note for flag {flag}");
    }
    assert!(
        notes["repo_create"].as_str().unwrap().contains("admin"),
        "the admin-only repository creation flag must say so"
    );
}

/// `generated_at` is stamped when a response is built, not taken from the
/// read model captured at process start.
#[tokio::test]
async fn generated_at_is_stamped_at_serialization_time() {
    let mut state = WebState::new(ForgeCore::new());
    let boot = chrono::Utc::now() - chrono::Duration::hours(31);
    state.tui.generated_at = boot;
    let state = Arc::new(state);
    let before = chrono::Utc::now();

    let parse = |value: &str| {
        chrono::DateTime::parse_from_rfc3339(value)
            .expect("generated_at is RFC 3339")
            .with_timezone(&chrono::Utc)
    };
    let served = tui_read_model(State(state.clone())).await.0;
    assert!(served.generated_at >= before, "the TUI read model is stale");
    let repos = repo_list_response(&state);
    assert!(parse(&repos.generated_at) >= before, "repos is stale");
    assert!(parse(&server_time()) >= before, "server_time is stale");
    let bootstrap = bootstrap_payload(&state).expect("bootstrap payload");
    let bootstrap = serde_json::to_value(bootstrap).expect("serialize bootstrap");
    let stamped = bootstrap["generated_at"]
        .as_str()
        .expect("bootstrap generated_at");
    assert!(parse(stamped) >= before, "bootstrap is stale");
}

#[tokio::test]
async fn capabilities_and_mcp_are_served_through_the_router() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = app(
        WebState::new(ForgeCore::new()),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(crate::discovery::CAPABILITIES_PATH)
                .header(header::USER_AGENT, "curl/8.5.0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-jeryu-fast-path")
            .and_then(|value| value.to_str().ok()),
        Some(crate::discovery::CAPABILITIES_PATH)
    );
    assert_eq!(
        response
            .headers()
            .get("x-jeryu-api")
            .and_then(|value| value.to_str().ok()),
        Some("v4")
    );
    let parsed = response_json(response).await;
    assert_eq!(parsed["mcp_endpoint"], "/mcp");

    let response = app
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::NOT_FOUND);
    assert!(response.headers().contains_key("x-jeryu-fast-path"));
}

/// The TUI read model is a resource of its own at `/api/v1/read-model/tui`.
/// The suffixed `/api/v1/bootstrap.tui` spelling still answers with the very
/// same bytes, so clients can move to the resource path on their own schedule.
#[tokio::test]
async fn tui_read_model_is_served_under_its_own_path_and_the_suffixed_alias() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = app(
        WebState::new(ForgeCore::new()),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let fetch = |path: &'static str| {
        let app = app.clone();
        async move {
            let response = app
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            (status, response_json(response).await)
        }
    };

    let (status, resource) = fetch("/api/v1/read-model/tui").await;
    assert_eq!(status, StatusCode::OK, "the read model has its own route");
    assert!(
        resource["schema_version"].is_string(),
        "the read model is served, not an error envelope"
    );
    let (alias_status, alias) = fetch("/api/v1/bootstrap.tui").await;
    assert_eq!(alias_status, StatusCode::OK);
    assert_eq!(
        alias["schema_version"], resource["schema_version"],
        "both paths serve the same read model"
    );
}
