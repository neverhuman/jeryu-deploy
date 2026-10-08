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
