use super::*;

fn known_mcp_tools() -> BTreeSet<String> {
    jeryu_mcp::tool_manifest()
        .into_iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect()
}

fn header_value<'a>(headers: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v.as_str())
}

#[test]
fn github_rest_edge_dispatches_repos_user_and_404() {
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
    let state = WebState::new(core);
    // The forwarder targets `state.github.handle(method, path, body)`; the
    // mounted `GET /repos` must return a GitHub-shaped 200 listing the repo.
    let repos = state.github.handle(Method::Get, "/repos", "");
    assert_eq!(repos.status, 200);
    assert!(repos.body.contains("alice"));
    assert!(repos.body.contains("jeryu"));
    // `GET /user` is mounted so `gh auth status` resolves a principal.
    assert_eq!(state.github.handle(Method::Get, "/user", "").status, 200);
    // An unknown route returns a clean GitHub-shaped 404, never a panic/500.
    assert_eq!(
        state
            .github
            .handle(Method::Get, "/repos/x/y/nope", "")
            .status,
        404
    );
}

#[tokio::test]
async fn github_rest_repo_edge_requires_auth_and_filters_grants() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "secret".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.create_account("jeryu-admin", "admin-password", UserRole::Admin)
        .unwrap();
    core.create_account("jordanh", "jordanh-password", UserRole::User)
        .unwrap();
    core.grant_repo_access(
        "jeryu-admin",
        "jordanh",
        "alice",
        "jeryu",
        RepoAccessLevel::Read,
    )
    .unwrap();
    let token = core
        .create_personal_access_token("jordanh", "test", None)
        .unwrap()
        .secret;

    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/repos")
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let list = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/repos")
                .header(header::ACCEPT, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let body = response_json(list).await;
    let names: Vec<_> = body
        .as_array()
        .expect("repo list is an array")
        .iter()
        .filter_map(|repo| repo["name"].as_str())
        .collect();
    assert_eq!(names, vec!["jeryu"]);

    let denied = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/repos/alice/secret")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let api_v3 = app
        .oneshot(
            Request::builder()
                .uri("/api/v3/repos")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(api_v3.status(), StatusCode::OK);
    let body = response_json(api_v3).await;
    assert_eq!(body.as_array().expect("repo list is an array").len(), 1);
}

#[tokio::test]
async fn github_release_creation_checks_auth_and_access_before_unavailable() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "release-test".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.create_account("release-admin", "admin-password", UserRole::Admin)
        .unwrap();
    core.create_account("release-reader", "reader-password", UserRole::User)
        .unwrap();
    core.grant_repo_access(
        "release-admin",
        "release-reader",
        "alice",
        "release-test",
        RepoAccessLevel::Read,
    )
    .unwrap();
    let admin = core
        .create_personal_access_token("release-admin", "release-test", None)
        .unwrap()
        .secret;
    let reader = core
        .create_personal_access_token("release-reader", "release-test", None)
        .unwrap()
        .secret;
    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    for prefix in ["", "/api/v3"] {
        for (token, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("invalid-token"), StatusCode::UNAUTHORIZED),
            (Some(reader.as_str()), StatusCode::FORBIDDEN),
            (Some(admin.as_str()), StatusCode::NOT_IMPLEMENTED),
        ] {
            let mut request = Request::builder()
                .method("POST")
                .uri(format!("{prefix}/repos/alice/release-test/releases"))
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(token) = token {
                request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            let response = app
                .clone()
                .oneshot(
                    request
                        .body(Body::from(r#"{"tag_name":"v1.0.0","name":"Release"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            let body = response_json(response).await;
            assert!(body.get("tag_name").is_none());
            assert!(body.get("html_url").is_none());
        }
    }
}

#[tokio::test]
async fn github_rest_binds_mutation_actor_to_authenticated_principal() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    let token = core
        .create_personal_access_token("alice", "test", None)
        .unwrap()
        .secret;

    let response = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    )
    .oneshot(
        Request::builder()
            .method(HttpMethod::POST)
            .uri("/repos/alice/jeryu/issues")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "title": "principal binding",
                    "actor": "mallory"
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = response_json(response).await;
    assert_eq!(body["user"]["login"], "alice");
}

#[tokio::test]
async fn github_rest_reserves_ci_evidence_and_protection_controls() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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
    core.create_account("repo-admin", "repo-admin-password", UserRole::User)
        .unwrap();
    core.grant_repo_access(
        "jeryu-admin",
        "writer",
        "alice",
        "jeryu",
        RepoAccessLevel::Write,
    )
    .unwrap();
    core.grant_repo_access(
        "jeryu-admin",
        "repo-admin",
        "alice",
        "jeryu",
        RepoAccessLevel::Admin,
    )
    .unwrap();
    let writer_token = core
        .create_personal_access_token("writer", "test", None)
        .unwrap()
        .secret;
    let admin_token = core
        .create_personal_access_token("jeryu-admin", "test", None)
        .unwrap()
        .secret;
    let repo_admin_token = core
        .create_personal_access_token("repo-admin", "test", None)
        .unwrap()
        .secret;
    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );

    let check_body = serde_json::json!({
        "name": "jeryu-deploy/required",
        "head_sha": "deadbeef",
        "status": "completed",
        "conclusion": "success"
    })
    .to_string();
    let status_body = serde_json::json!({
        "state": "success",
        "context": "jeryu-deploy/required"
    })
    .to_string();
    let protection_body = serde_json::json!({
        "required_status_checks": ["jeryu-deploy/required"],
        "required_approving_review_count": 1,
        "enforce_admins": true,
        "required_linear_history": true,
        "allow_force_pushes": false,
        "allow_deletions": false
    })
    .to_string();

    for prefix in ["", "/api/v3"] {
        for (method, path, body) in [
            (
                HttpMethod::POST,
                "/repos/alice/jeryu/check-runs",
                check_body.as_str(),
            ),
            (
                HttpMethod::POST,
                "/repos/alice/jeryu/statuses/deadbeef",
                status_body.as_str(),
            ),
            (
                HttpMethod::PUT,
                "/repos/alice/jeryu/branches/main/protection",
                protection_body.as_str(),
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!("{prefix}{path}"))
                        .header(header::AUTHORIZATION, format!("Bearer {writer_token}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "repo writers must not mutate protected evidence controls through {prefix}{path}"
            );
        }
    }

    for (path, body) in [
        ("/repos/alice/jeryu/check-runs", check_body),
        ("/repos/alice/jeryu/statuses/deadbeef", status_body),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(HttpMethod::POST)
                    .uri(path)
                    .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let protection = app
        .oneshot(
            Request::builder()
                .method(HttpMethod::PUT)
                .uri("/repos/alice/jeryu/branches/main/protection")
                .header(header::AUTHORIZATION, format!("Bearer {repo_admin_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(protection_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(protection.status(), StatusCode::OK);
}

#[test]
fn advisory_headers_always_present_on_any_route() {
    // A plain browser UA still gets the API + fast-path advisories, but no
    // tool hint (we only steer automation/gh-like clients).
    let headers = advisory_headers(
        "Mozilla/5.0 (browser)",
        &HttpMethod::GET,
        "/api/v1/bootstrap",
    );
    assert_eq!(header_value(&headers, HDR_API), Some("v4"));
    assert_eq!(
        header_value(&headers, HDR_FAST_PATH),
        Some("/.jeryu/capabilities")
    );
    assert!(header_value(&headers, HDR_TOOL).is_none());
}

#[test]
fn advisory_headers_steer_gh_like_agents_to_mcp_tools() {
    // The gh CLI UA on a PR-create maps to the propose_patch MCP tool.
    let gh = advisory_headers(
        "GitHub CLI 2.40.0 go-gh/2.0",
        &HttpMethod::POST,
        "/repos/alice/jeryu/pulls",
    );
    assert_eq!(header_value(&gh, HDR_TOOL), Some(MCP_PATCH_TOOL));

    // A merge PUT maps to request_merge for any automation UA (curl here).
    let merge = advisory_headers(
        "curl/8.0",
        &HttpMethod::PUT,
        "/repos/alice/jeryu/pulls/7/merge",
    );
    assert_eq!(header_value(&merge, HDR_TOOL), Some(MCP_MERGE_TOOL));

    // GET PR routes steer to blocker explanation for agent UAs.
    let read = advisory_headers(
        "jeryu-agent/1.0",
        &HttpMethod::GET,
        "/repos/alice/jeryu/pulls",
    );
    assert_eq!(header_value(&read, HDR_TOOL), Some(MCP_BLOCKERS_TOOL));

    // Issue create gets a dedicated mutation tool.
    assert_eq!(
        header_value(
            &advisory_headers(
                "python-requests/2.31",
                &HttpMethod::POST,
                "/repos/a/b/issues"
            ),
            HDR_TOOL
        ),
        Some(MCP_ISSUE_TOOL)
    );

    // Actions writes steer to the local CI runner entrypoint.
    assert_eq!(
        header_value(
            &advisory_headers(
                "GitHub CLI 2.40.0 go-gh/2.0",
                &HttpMethod::POST,
                "/repos/alice/jeryu/actions/workflows/ci-fast.yml/dispatches"
            ),
            HDR_TOOL
        ),
        Some("jeryu.run_tests")
    );
}

#[test]
fn automation_agent_detection_is_case_insensitive_and_scoped() {
    assert!(is_automation_agent("GitHub CLI 2.40.0"));
    assert!(is_automation_agent("github cli"));
    assert!(is_automation_agent("go-gh/2.0"));
    assert!(is_automation_agent("okhttp/4.12.0"));
    assert!(is_automation_agent("curl/8.4.0"));
    assert!(is_automation_agent("python-requests/2.31.0"));
    assert!(is_automation_agent("Jeryu-Agent/1.0"));
    assert!(is_automation_agent("some-agent-runner"));
    // A normal browser is not steered with a tool hint.
    assert!(!is_automation_agent(
        "Mozilla/5.0 (Macintosh) AppleWebKit Safari"
    ));
    assert!(!is_automation_agent(""));
}

#[test]
fn suggested_tool_covers_mutations_and_reads() {
    assert_eq!(
        suggested_tool(&HttpMethod::POST, "/repos/a/b/pulls"),
        Some(MCP_PATCH_TOOL)
    );
    assert_eq!(
        suggested_tool(&HttpMethod::PUT, "/repos/a/b/pulls/3/merge"),
        Some(MCP_MERGE_TOOL)
    );
    assert_eq!(
        suggested_tool(&HttpMethod::GET, "/repos/a/b"),
        Some(MCP_READ_TOOL)
    );
    assert_eq!(
        suggested_tool(&HttpMethod::GET, "/repos/a/b/commits/deadbeef/check-runs"),
        Some(MCP_CHECKS_TOOL)
    );
    // A DELETE (unsupported verb) yields no hint.
    assert!(suggested_tool(&HttpMethod::DELETE, "/repos/a/b").is_none());
}

#[test]
fn advertised_mcp_tools_exist_in_catalog() {
    let known = known_mcp_tools();
    for tool in MCP_GUIDANCE_TOOLS {
        assert!(known.contains(*tool), "missing MCP catalog tool: {tool}");
    }
    for tool in [
        suggested_tool(&HttpMethod::POST, "/repos/a/b/pulls"),
        suggested_tool(&HttpMethod::PUT, "/repos/a/b/pulls/3/merge"),
        suggested_tool(&HttpMethod::GET, "/repos/a/b/commits/deadbeef/check-runs"),
        suggested_tool(&HttpMethod::GET, "/repos/a/b/pulls"),
        suggested_tool(&HttpMethod::GET, "/repos/a/b"),
    ] {
        let tool = tool.expect("tool hint");
        assert!(known.contains(tool), "invalid suggested MCP tool: {tool}");
    }
    let payload = capabilities_payload(&live_mcp_tools(&Arc::new(WebState::new(ForgeCore::new()))));
    for tool in payload["mcp_tools"].as_array().expect("mcp_tools array") {
        let tool = tool.as_str().expect("tool string");
        assert!(known.contains(tool), "invalid capability MCP tool: {tool}");
    }
}

#[tokio::test]
async fn live_unknown_github_route_returns_guided_json_not_spa() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = app(
        WebState::new(ForgeCore::new()),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/repos/alice/jeryu/unknown-thing")
                .header(header::USER_AGENT, "GitHub CLI 2.40.0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let parsed = response_json(response).await;
    assert_eq!(
        parsed["jeryu_repair_hint"]["purpose"],
        "route unsupported GitHub-compatible REST request"
    );
    assert!(parsed["jeryu_mcp_tools"].as_array().unwrap().len() >= 4);
}

#[tokio::test]
async fn live_gh_auth_workaround_route_returns_guided_json_not_spa() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = app(
        WebState::new(ForgeCore::new()),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/login/device/code")
                .header(header::USER_AGENT, "GitHub CLI 2.40.0 go-gh/2.0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let parsed = response_json(response).await;
    assert_eq!(
        parsed["jeryu_repair_hint"]["purpose"],
        "route GitHub CLI auth setup through Jeryu"
    );
    assert_eq!(
        parsed["jeryu_connection"]["gh_setup"],
        "jeryu gh-setup --host http://127.0.0.1:8787 --token-file ~/.jeryu/secrets/merge-token"
    );
    assert_eq!(
        parsed["jeryu_connection"]["gh_token_file"],
        "~/.jeryu/secrets/merge-token"
    );
    assert!(!parsed.to_string().contains("JERYU-TOKEN"));
}

#[tokio::test]
async fn live_api_v3_user_alias_serves_github_cli_status() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = app(
        WebState::new(ForgeCore::new()),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v3/user")
                .header(header::USER_AGENT, "GitHub CLI 2.40.0 go-gh/2.0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let parsed = response_json(response).await;
    assert_eq!(parsed["login"], "jeryu-admin");
}

#[tokio::test]
async fn github_vendor_json_accept_is_not_served_the_spa_shell() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let app = app(
        WebState::new(core),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/repos")
                .header(header::ACCEPT, "application/vnd.github+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(content_type.starts_with("application/json"));
    let parsed = response_json(response).await;
    assert_eq!(parsed.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn live_actions_write_returns_guided_json_and_steering_headers() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = app(
        WebState::new(ForgeCore::new()),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/repos/alice/jeryu/actions/workflows/ci-fast.yml/dispatches")
                .header(header::USER_AGENT, "GitHub CLI 2.40.0 go-gh/2.0")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ref":"main"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        response
            .headers()
            .get("x-jeryu-api")
            .and_then(|value| value.to_str().ok()),
        Some("v4")
    );
    assert_eq!(
        response
            .headers()
            .get("x-jeryu-fast-path")
            .and_then(|value| value.to_str().ok()),
        Some("/.jeryu/capabilities")
    );
    assert_eq!(
        response
            .headers()
            .get("x-jeryu-tool")
            .and_then(|value| value.to_str().ok()),
        Some("jeryu.run_tests")
    );
    let parsed = response_json(response).await;
    assert_eq!(
        parsed["jeryu_repair_hint"]["purpose"],
        "route unsupported GitHub Actions write request"
    );
    assert_eq!(parsed["jeryu_connection"]["mcp"], "/mcp");
    assert_eq!(parsed["jeryu_steering"]["mcp_tool"], "jeryu.run_tests");
}

#[tokio::test]
async fn live_actions_workflow_routes_return_json_and_steering_headers() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "ci/fast".to_string(),
            head_sha: "deadbeef".to_string(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Success),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();

    let app = app(
        WebState::new(core),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );

    let detail = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/repos/alice/jeryu/actions/workflows/ci-fast.yml")
                .header(header::USER_AGENT, "GitHub CLI 2.40.0 go-gh/2.0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(detail.status(), StatusCode::OK);
    assert_eq!(
        detail
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        detail
            .headers()
            .get("x-jeryu-tool")
            .and_then(|value| value.to_str().ok()),
        Some("jeryu.get_ci_run_jobs")
    );
    let detail_body = response_json(detail).await;
    assert_eq!(detail_body["name"], "ci/fast");
    let workflow_id = detail_body["id"].as_u64().expect("workflow id");

    let runs = app
        .oneshot(
            Request::builder()
                .uri("/repos/alice/jeryu/actions/workflows/ci-fast.yml/runs")
                .header(header::USER_AGENT, "GitHub CLI 2.40.0 go-gh/2.0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(runs.status(), StatusCode::OK);
    assert_eq!(
        runs.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let runs_body = response_json(runs).await;
    assert_eq!(runs_body["total_count"], 1);
    assert_eq!(runs_body["workflow_runs"][0]["workflow_id"], workflow_id);
}

#[tokio::test]
async fn live_unsupported_verb_returns_guided_json() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = app(
        WebState::new(ForgeCore::new()),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let delete = app
        .oneshot(
            Request::builder()
                .method(HttpMethod::DELETE)
                .uri("/repos/alice/jeryu")
                .header(header::USER_AGENT, "curl/8.0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete.status(), StatusCode::METHOD_NOT_ALLOWED);
    let parsed = response_json(delete).await;
    assert_eq!(
        parsed["jeryu_repair_hint"]["purpose"],
        "route unsupported GitHub-compatible REST method"
    );
}

/// A list request with `?per_page`/`?page` now passes through (no longer a
/// guided 501) and the RFC5988 `Link` header is surfaced on the wire via
/// `github_response`'s header passthrough.
#[tokio::test]
async fn live_list_query_paginates_and_surfaces_link_header() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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
    // Two open PRs so a per_page=1 page leaves a `next`/`last` link.
    for (head, sha) in [("feat-a", "sha-a"), ("feat-b", "sha-b")] {
        core.create_pull_request(
            "alice",
            "jeryu",
            "alice",
            CreatePullRequestRequest {
                title: head.to_string(),
                head: head.to_string(),
                base: "main".to_string(),
                head_sha: Some(sha.to_string()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    }

    let response = app(
        WebState::new(core),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    )
    .oneshot(
        Request::builder()
            .uri("/repos/alice/jeryu/pulls?per_page=1&page=1")
            .header(header::USER_AGENT, "go-gh/2.0")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let link = response
        .headers()
        .get("Link")
        .expect("Link header present")
        .to_str()
        .unwrap()
        .to_string();
    assert!(link.contains("rel=\"next\""), "Link has next: {link}");
    assert!(link.contains("rel=\"last\""), "Link has last: {link}");
    let parsed = response_json(response).await;
    assert_eq!(
        parsed.as_array().expect("pulls array").len(),
        1,
        "per_page=1 returns a single PR"
    );
}

/// A create-PR request that belongs on an existing open PR is refused on the
/// wire with GitHub's duplicate-PR 422 naming that PR, not a claimed success.
#[tokio::test]
async fn live_overlap_routing_refuses_duplicate_create() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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
    // An existing mergeable PR touching one file.
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "existing".to_string(),
            head: "feat-a".to_string(),
            base: "main".to_string(),
            head_sha: Some("sha-a".to_string()),
            changed_files: vec!["src/a.rs".to_string()],
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();

    let response = app(WebState::new(core), std::path::Path::new("/tmp/jeryu-no-spa"))
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/repos/alice/jeryu/pulls")
                .header(header::USER_AGENT, "go-gh/2.0")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"title":"hot-fix","head":"feat-a2","base":"main","changed_files":["src/a.rs"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        response.headers().get("X-Jeryu-Reused-PR").is_none(),
        "no success signal for a refused duplicate create"
    );
    let parsed = response_json(response).await;
    assert!(
        parsed["message"]
            .as_str()
            .expect("message")
            .starts_with("A pull request already exists for"),
        "GitHub's duplicate-PR message: {parsed}"
    );
    assert_eq!(
        parsed["existing_pull_request"]["number"]
            .as_u64()
            .expect("pr"),
        1,
        "the body names the existing PR"
    );
}

#[tokio::test]
async fn advertised_mcp_endpoint_is_mounted() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let response = app(
        WebState::new(ForgeCore::new()),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    )
    .oneshot(Request::builder().uri("/mcp").body(Body::empty()).unwrap())
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn mcp_endpoint_requires_configured_authentication() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    let token = core
        .create_personal_access_token("alice", "test", None)
        .unwrap()
        .secret;
    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );

    let unauthenticated = app
        .clone()
        .oneshot(Request::builder().uri("/mcp").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let authenticated = app
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authenticated.status(), StatusCode::METHOD_NOT_ALLOWED);
}

/// The MCP transport carries no identity of its own — the `clientInfo` name in
/// `initialize` is whatever the client typed — so the control-plane tools are
/// authorized against the account the request gate authenticated, exactly as
/// `/api/v1/control-plane/*` is.
#[tokio::test]
async fn mcp_control_plane_tools_answer_admins_and_refuse_other_accounts() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    let app = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );

    async fn call_control_plane_status(app: &axum::Router, core: &ForgeCore, login: &str) -> Value {
        let token = core
            .create_personal_access_token(login, "mcp", None)
            .unwrap()
            .secret;
        let post = |method: &'static str, session: Option<String>, body: Value| {
            let mut request = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("Mcp-Method", method);
            if let Some(session) = session {
                request = request
                    .header("Mcp-Session-Id", session)
                    .header("MCP-Protocol-Version", jeryu_mcp::MCP_PROTOCOL_VERSION);
            }
            request.body(Body::from(body.to_string())).unwrap()
        };

        let initialized = app
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
                        "clientInfo": { "name": "jeryu-admin", "version": "1" }
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
            .expect("session id")
            .to_string();

        let called = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Mcp-Method", "tools/call")
                    .header("Mcp-Name", "jeryu.control_plane.status")
                    .header("Mcp-Session-Id", session)
                    .header("MCP-Protocol-Version", jeryu_mcp::MCP_PROTOCOL_VERSION)
                    .body(Body::from(
                        json!({
                            "jsonrpc": "2.0",
                            "id": 2,
                            "method": "tools/call",
                            "params": {
                                "name": "jeryu.control_plane.status",
                                "arguments": {}
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(called.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(called.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // The `clientInfo` name claims to be the admin either way; only the token decides.
    let refused = call_control_plane_status(&app, &core, "bob").await;
    assert_eq!(refused["result"]["isError"], json!(true));
    assert_eq!(
        refused["result"]["structuredContent"]["message"],
        json!("control_plane.status requires the admin role")
    );

    let answered = call_control_plane_status(&app, &core, "alice").await;
    assert_eq!(answered["result"]["isError"], json!(false));
    assert!(answered["result"]["structuredContent"]["data"].is_object());
}

#[tokio::test]
async fn issue_create_with_idempotency_key_replays_instead_of_filing_twice() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    let token = core
        .create_personal_access_token("alice", "test", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let file = |key: Option<&str>| {
        let mut request = Request::builder()
            .method(HttpMethod::POST)
            .uri("/repos/alice/jeryu/issues")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        router.clone().oneshot(
            request
                .body(Body::from(r#"{"title":"retried create"}"#))
                .unwrap(),
        )
    };

    let first = file(Some("create-1")).await.unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    assert!(first.headers().get("idempotent-replayed").is_none());
    let first = response_json(first).await;

    let replay = file(Some("create-1")).await.unwrap();
    assert_eq!(replay.status(), StatusCode::CREATED);
    assert_eq!(replay.headers()["idempotent-replayed"], "true");
    assert_eq!(response_json(replay).await["number"], first["number"]);
    assert_eq!(core.list_issues("alice", "jeryu", None).unwrap().len(), 1);

    // No key: an ordinary second write.
    let fresh = response_json(file(None).await.unwrap()).await;
    assert_ne!(fresh["number"], first["number"]);
    assert_eq!(core.list_issues("alice", "jeryu", None).unwrap().len(), 2);
}

/// The GraphQL edge answers the same repository question the REST edge does,
/// so it must honour the same grants: an ungranted caller sees `repository:
/// null` rather than a private repository's name, privacy and default branch.
#[tokio::test]
async fn github_graphql_repository_query_honours_repo_grants() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "secret".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.create_account("jeryu-admin", "admin-password", UserRole::Admin)
        .unwrap();
    core.create_account("jordanh", "jordanh-password", UserRole::User)
        .unwrap();
    core.create_account("mina", "mina-password", UserRole::User)
        .unwrap();
    core.grant_repo_access(
        "jeryu-admin",
        "mina",
        "alice",
        "secret",
        RepoAccessLevel::Read,
    )
    .unwrap();
    let ungranted = core
        .create_personal_access_token("jordanh", "test", None)
        .unwrap()
        .secret;
    let granted = core
        .create_personal_access_token("mina", "test", None)
        .unwrap()
        .secret;

    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let query = json!({
        "query": "query { repository(owner: \"alice\", name: \"secret\") { name nameWithOwner isPrivate defaultBranchRef { name } } }"
    })
    .to_string();

    let graphql = |token: String, body: String| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/graphql")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };

    let denied = graphql(ungranted, query.clone()).await;
    assert_eq!(denied.status(), StatusCode::OK);
    let body = response_json(denied).await;
    assert_eq!(
        body["data"]["repository"],
        Value::Null,
        "a caller without a grant must not learn the repository exists"
    );

    let allowed = graphql(granted, query).await;
    assert_eq!(allowed.status(), StatusCode::OK);
    let body = response_json(allowed).await;
    assert_eq!(body["data"]["repository"]["nameWithOwner"], "alice/secret");
    assert_eq!(body["data"]["repository"]["isPrivate"], true);
    assert_eq!(
        body["data"]["repository"]["defaultBranchRef"]["name"],
        "main"
    );
}

/// The GitHub edge (`/repos`, `/api/v3`, `/graphql`) authenticates outside the
/// `/api/v1` gate, so it has to apply the same two account-state policies:
/// an account owing a password change cannot act, and a cookie-session
/// mutation needs its CSRF header. Bearer tokens stay CSRF-exempt.
#[tokio::test]
async fn github_edge_applies_password_change_and_csrf_policies() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::User)
        .unwrap();
    let temporary_password = ["temporary", "pass", "123"].join("-");
    core.create_temporary_account("resetuser", &temporary_password, UserRole::User)
        .unwrap();
    let alice_session = core.create_session("alice").unwrap();
    let alice_cookie = format!("jeryu-session={}", alice_session.token);
    let alice_csrf = alice_session.session.csrf_token.clone();
    let reset_cookie = format!(
        "jeryu-session={}",
        core.create_session("resetuser").unwrap().token
    );
    let token = core
        .create_personal_access_token("alice", "test", None)
        .unwrap()
        .secret;

    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );

    // An account that must change its password is refused on every edge path.
    for request in [
        Request::builder()
            .uri("/repos")
            .header(header::ACCEPT, "application/json")
            .header(header::COOKIE, &reset_cookie)
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .uri("/api/v3/user")
            .header(header::COOKIE, &reset_cookie)
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method(HttpMethod::POST)
            .uri("/graphql")
            .header(header::COOKIE, &reset_cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"query":"{ viewer { login } }"}"#))
            .unwrap(),
    ] {
        let uri = request.uri().to_string();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
        let body = response_json(response).await;
        assert_eq!(
            body["message"], "password change required before continuing",
            "{uri}"
        );
    }

    // A cookie mutation without the CSRF header is refused on every edge path.
    for path in ["/repos", "/api/v3/repos", "/graphql"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(HttpMethod::POST)
                    .uri(path)
                    .header(header::ACCEPT, "application/json")
                    .header(header::COOKIE, &alice_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"name":"csrf-probe"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        let body = response_json(response).await;
        assert_eq!(body["message"], "missing or invalid CSRF token", "{path}");
    }

    // The same mutation with the header passes the CSRF check and is decided
    // by the edge's own authorization instead.
    let with_csrf = app
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/repos")
                .header(header::ACCEPT, "application/json")
                .header(header::COOKIE, &alice_cookie)
                .header("x-jeryu-csrf", &alice_csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"csrf-probe"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let with_csrf_body = response_json(with_csrf).await;
    assert_ne!(with_csrf_body["message"], "missing or invalid CSRF token");

    // A bearer token is CSRF-exempt, as it is on /api/v1.
    let bearer_read = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v3/user")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bearer_read.status(), StatusCode::OK);

    let bearer_write = app
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/graphql")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"query":"{ viewer { login } }"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bearer_write.status(), StatusCode::OK);
}

/// `gh auth status`, `gh api user` and every agent that introspects its own
/// identity read `GET /user` and the GraphQL `viewer`. Both must answer with
/// the token's own account: two tokens, two different logins, ids and names.
#[tokio::test]
async fn github_user_and_viewer_report_the_authenticated_account() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_account("jordanh", "jordanh-password", UserRole::User)
        .unwrap();
    core.create_account("mina", "mina-password", UserRole::User)
        .unwrap();
    let jordanh = core
        .create_personal_access_token("jordanh", "test", None)
        .unwrap()
        .secret;
    let mina = core
        .create_personal_access_token("mina", "test", None)
        .unwrap()
        .secret;

    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let get_user = |token: String, path: &'static str| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    let viewer = |token: String| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/graphql")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "query": "query { viewer { login name id } }" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };

    for path in ["/user", "/api/v3/user"] {
        let first = get_user(jordanh.clone(), path).await;
        assert_eq!(first.status(), StatusCode::OK);
        let first = response_json(first).await;
        assert_eq!(first["login"], "jordanh");
        assert_eq!(first["name"], "jordanh");
        assert_eq!(first["node_id"], "U_jordanh");
        assert_eq!(first["type"], "User");

        let second = get_user(mina.clone(), path).await;
        assert_eq!(second.status(), StatusCode::OK);
        let second = response_json(second).await;
        assert_eq!(second["login"], "mina");
        assert_eq!(second["name"], "mina");
        assert_ne!(
            first["id"], second["id"],
            "two accounts must not share one user id"
        );
    }

    let first = response_json(viewer(jordanh).await).await;
    assert_eq!(first["data"]["viewer"]["login"], "jordanh");
    assert_eq!(first["data"]["viewer"]["name"], "jordanh");
    assert_eq!(first["data"]["viewer"]["id"], "U_jordanh");

    let second = response_json(viewer(mina).await).await;
    assert_eq!(second["data"]["viewer"]["login"], "mina");
    assert_eq!(second["data"]["viewer"]["id"], "U_mina");
}
