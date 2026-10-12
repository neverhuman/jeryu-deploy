use super::{LaunchDenial, admission};
use crate::web::sessions::{SessionRuntime, SessionRuntimeConfig};
use crate::web::{WebState, agent_runs, sessions, workcells};
use axum::body::{Bytes, to_bytes};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use jeryu_core::ForgeCore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

fn state(runtime: SessionRuntime) -> Arc<WebState> {
    Arc::new(
        WebState::new(ForgeCore::new()).with_session_runtime(SessionRuntimeConfig {
            runtime,
            docker_bin: Some("/bin/sh".to_string()),
            spawn_companion_shell: true,
        }),
    )
}

async fn value(response: Response, status: u16) -> Value {
    assert_eq!(response.status().as_u16(), status);
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("bounded body");
    serde_json::from_slice(&bytes).expect("typed JSON")
}

#[test]
fn managed_launch_runtime_selection_is_explicit_and_defaults_to_firecracker() {
    for selector in [None, Some(""), Some("firecracker"), Some(" FIRECRACKER ")] {
        assert_eq!(
            SessionRuntimeConfig::runtime_from_value(selector),
            SessionRuntime::Firecracker
        );
    }
    for (selector, runtime) in [
        ("auto", SessionRuntime::Auto),
        ("native", SessionRuntime::Native),
        ("docker", SessionRuntime::Docker),
        ("firecraker", SessionRuntime::Invalid),
        ("host", SessionRuntime::Invalid),
    ] {
        assert_eq!(
            SessionRuntimeConfig::runtime_from_value(Some(selector)),
            runtime
        );
    }
}

#[test]
fn managed_launch_never_authorizes_a_legacy_host_driver() {
    for runtime in [
        SessionRuntime::Auto,
        SessionRuntime::Docker,
        SessionRuntime::Native,
    ] {
        assert_eq!(admission(runtime), Err(LaunchDenial::HostExecution));
    }
    assert_eq!(
        admission(SessionRuntime::Firecracker),
        Err(LaunchDenial::SandboxUnavailable)
    );
    assert_eq!(
        admission(SessionRuntime::Invalid),
        Err(LaunchDenial::InvalidRuntime)
    );
}

#[tokio::test]
async fn managed_launch_entrypoints_fail_before_host_paths_and_preserve_sentinel() {
    let temporary = tempfile::tempdir().expect("sentinel directory");
    let sentinel = temporary.path().join("host-sentinel");
    std::fs::write(&sentinel, b"unchanged sentinel").expect("sentinel");
    let inode = std::fs::metadata(&sentinel).expect("metadata").ino();
    let digest = Sha256::digest(std::fs::read(&sentinel).expect("bytes"));
    let command = format!("printf overwritten > '{}'", sentinel.display());
    for (runtime, status, code) in [
        (SessionRuntime::Firecracker, 424, "sandbox_unavailable"),
        (SessionRuntime::Native, 403, "host_launch_denied"),
        (SessionRuntime::Docker, 403, "host_launch_denied"),
        (SessionRuntime::Auto, 403, "host_launch_denied"),
        (SessionRuntime::Invalid, 422, "managed_runtime_invalid"),
    ] {
        let state = state(runtime);
        let session = value(
            sessions::create(
                State(state.clone()),
                Path("missing/repository".to_string()),
                HeaderMap::new(),
                None,
                None,
                Bytes::from(
                    json!({"agent_id":"claude","command":"/bin/sh","args":["-c",command]})
                        .to_string(),
                ),
            )
            .await,
            status,
        )
        .await;
        assert_eq!(session["code"], code);
        let run = value(
            agent_runs::start(
                State(state.clone()),
                Bytes::from(
                    json!({"source":{"kind":"local_path","local_path":temporary.path()},
                "program":"/bin/sh","args":["-c",command]})
                    .to_string(),
                ),
            )
            .await,
            status,
        )
        .await;
        assert_eq!(run["code"], code);
        let claimed = state
            .workcells
            .lock()
            .expect("metadata store")
            .claim(jeryu_runnerd::WorkcellClaimRequest {
                agent_id: "test-controller".to_string(),
                workspace_root: temporary.path().to_path_buf(),
                repo_roots: vec![temporary.path().to_path_buf()],
                branch_budget: 1,
                runner_id: "fixture-runner".to_string(),
                runner_epoch: 1,
                git_status_summary: "clean".to_string(),
                ci_snapshot_age_ms: None,
                startup: jeryu_runnerd::StartupSync::Rebased {
                    main_ref: "refs/heads/main".to_string(),
                    base_sha: "fixture-base".to_string(),
                    head_sha: "fixture-base".to_string(),
                },
            })
            .expect("metadata-only fixture; no execution admitted");
        let workcell = value(
            workcells::run_agent(
                State(state.clone()),
                Path(claimed.workcell_id.clone()),
                Bytes::from(
                    json!({"workcell_id":claimed.workcell_id,"runner_epoch":1,
                "repo_root":temporary.path(),"program":"/bin/sh","args":["-c",command]})
                    .to_string(),
                ),
            )
            .await,
            status,
        )
        .await;
        assert_eq!(workcell["code"], code);
        let shell = value(
            agent_runs::shell(State(state.clone()), Path("missing-parent".to_string())).await,
            status,
        )
        .await;
        assert_eq!(shell["code"], code);
        let mcp = agent_runs::mcp_start(
            state.clone(),
            json!({"source":{"kind":"local_path","local_path":temporary.path()},
                "program":"/bin/sh","args":["-c",command]}),
        )
        .expect_err("MCP host launch must be refused");
        let mcp: Value = serde_json::from_str(&mcp).expect("MCP retains typed guidance");
        assert_eq!(mcp["code"], code);
        assert!(
            mcp["docs_url"]
                .as_str()
                .expect("docs")
                .contains("managed-firecracker-admission")
        );
        assert!(
            state.agent_runs.list().is_empty(),
            "no run accepted on denial"
        );
        assert!(
            state.agent_runs.legacy_preparations().is_empty(),
            "no legacy source, credential or driver preparation"
        );
        assert_eq!(std::fs::metadata(&sentinel).expect("metadata").ino(), inode);
        assert_eq!(
            Sha256::digest(std::fs::read(&sentinel).expect("bytes")),
            digest
        );
        assert!(!temporary.path().join(".agent-home").exists());
    }
}

#[tokio::test]
async fn managed_launch_invalid_json_still_returns_schema_guidance() {
    let state = state(SessionRuntime::Firecracker);
    let body = value(
        agent_runs::start(State(state), Bytes::from_static(b"{")).await,
        422,
    )
    .await;
    assert_eq!(body["code"], "agent_run_invalid_request");
}

#[tokio::test]
async fn managed_launch_bot_rejection_leaves_no_pending_operation() {
    let core = ForgeCore::new();
    core.create_account("guard-owner", "test-password-1", jeryu_core::UserRole::User)
        .expect("fixture owner");
    let enrolled = core
        .enroll_bot(
            "guard-owner",
            "guard",
            "Guard",
            jeryu_core::BotKind::GROK,
            jeryu_core::BotReach::General,
        )
        .expect("fixture enrollment");
    let session = core
        .exchange_bot_enrollment_key(&enrolled.enrollment_key)
        .expect("fixture access");
    let principal = crate::web::bot_token::BotPrincipal {
        bot_id: session.bot.id,
        generation: session.generation,
        key_id: session.key_id.clone(),
    };
    let state = Arc::new(
        WebState::new(core.clone()).with_session_runtime(SessionRuntimeConfig {
            runtime: SessionRuntime::Firecracker,
            docker_bin: None,
            spawn_companion_shell: true,
        }),
    );
    let body = json!({"provider":"claude","task":"guard regression"}).to_string();
    let mut headers = HeaderMap::new();
    headers.insert(
        crate::web::idempotency::IDEMPOTENCY_KEY,
        axum::http::HeaderValue::from_static("guard-repeat-1"),
    );
    for _ in 0..2 {
        let response = value(
            sessions::create(
                State(state.clone()),
                Path("missing/repository".to_string()),
                headers.clone(),
                Some(axum::extract::Extension(principal.clone())),
                None,
                Bytes::from(body.clone()),
            )
            .await,
            424,
        )
        .await;
        assert_eq!(response["code"], "sandbox_unavailable");
    }
    assert!(state.agent_runs.list().is_empty());
    assert!(state.agent_runs.legacy_preparations().is_empty());
    assert!(matches!(
        core.reserve_bot_operation(
            principal.bot_id,
            "session.create",
            "guard-repeat-1",
            &crate::web::bots::body_digest(body.as_bytes())
        )
        .expect("reserve only after both denied attempts"),
        jeryu_core::BotOperationGate::Start
    ));
}

#[tokio::test]
async fn managed_launch_router_auth_and_csrf_reject_before_preparation() {
    use axum::body::Body;
    use axum::http::{Request, header};
    use jeryu_core::{CreateRepositoryRequest, UserRole};
    use tower::ServiceExt;
    let temporary = tempfile::tempdir().expect("router fixture");
    let core = ForgeCore::new();
    for (login, role) in [
        ("guard-admin", UserRole::Admin),
        ("guard-stranger", UserRole::User),
    ] {
        core.create_account(login, "router-test-password", role)
            .expect("account");
    }
    core.create_repository(
        "guard-admin",
        CreateRepositoryRequest {
            name: "private".to_string(),
            private: true,
            default_branch: Some("main".to_string()),
            ..Default::default()
        },
    )
    .expect("metadata-only repository");
    let bearer = core
        .create_personal_access_token("guard-stranger", "negative fixture", None)
        .expect("test-only token")
        .secret;
    let browser = core
        .create_session("guard-admin")
        .expect("test-only browser session");
    for (runtime, status, code) in [
        (SessionRuntime::Firecracker, 424, "sandbox_unavailable"),
        (SessionRuntime::Native, 403, "host_launch_denied"),
        (SessionRuntime::Docker, 403, "host_launch_denied"),
        (SessionRuntime::Auto, 403, "host_launch_denied"),
    ] {
        let state = Arc::new(
            WebState::new_with_git_storage(core.clone(), temporary.path().join("git"))
                .with_auth(true, false, false)
                .with_session_runtime(SessionRuntimeConfig {
                    runtime,
                    docker_bin: Some("/bin/sh".to_string()),
                    spawn_companion_shell: true,
                }),
        );
        let application = crate::web::router(state.clone());
        for (path, body) in [
            (
                "/api/v1/repos/guard-admin/private/sessions",
                json!({"agent_id":"claude"}),
            ),
            (
                "/api/v1/agent-runs",
                json!({"source":{"kind":"local_path","local_path":temporary.path()},
                "program":"/bin/sh","args":["-c","exit 0"]}),
            ),
            ("/api/v1/agent-runs/missing-parent/shell", json!({})),
        ] {
            let request = || {
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
            };
            let anonymous = application
                .clone()
                .oneshot(
                    request()
                        .body(Body::from(body.to_string()))
                        .expect("anonymous request"),
                )
                .await
                .expect("response");
            assert_eq!(anonymous.status().as_u16(), 401);
            let stranger = application
                .clone()
                .oneshot(
                    request()
                        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                        .body(Body::from(body.to_string()))
                        .expect("stranger request"),
                )
                .await
                .expect("response");
            assert_eq!(
                stranger.status().as_u16(),
                403,
                "non-owner launch is forbidden before any preparation"
            );
            let cookie = format!("jeryu-session={}", browser.token);
            let missing_csrf = application
                .clone()
                .oneshot(
                    request()
                        .header(header::COOKIE, &cookie)
                        .body(Body::from(body.to_string()))
                        .expect("browser request"),
                )
                .await
                .expect("response");
            assert_eq!(value(missing_csrf, 403).await["code"], "csrf_required");
            let allowed_identity = application
                .clone()
                .oneshot(
                    request()
                        .header(header::COOKIE, &cookie)
                        .header("x-jeryu-csrf", &browser.session.csrf_token)
                        .body(Body::from(body.to_string()))
                        .expect("authenticated request"),
                )
                .await
                .expect("response");
            assert_eq!(value(allowed_identity, status).await["code"], code);
            assert!(state.agent_runs.legacy_preparations().is_empty());
            assert!(state.agent_runs.list().is_empty());
        }
    }
}
