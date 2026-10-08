//! Legacy response and path components only; no driver or process is launched.
use super::*;
use jeryu_runnerd::{StartupSync, WorkcellClaimRequest, WorkcellManager};
use serde_json::{Value, json};

async fn value(response: AxumResponse, expected: u16) -> Value {
    assert_eq!(response.status().as_u16(), expected);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}
fn lease(workspace: &Path, roots: Vec<PathBuf>) -> WorkcellLease {
    WorkcellManager::new()
        .claim(WorkcellClaimRequest {
            agent_id: "component".into(),
            workspace_root: workspace.into(),
            repo_roots: roots,
            branch_budget: 1,
            runner_id: "component-node".into(),
            runner_epoch: 1,
            git_status_summary: "clean".into(),
            ci_snapshot_age_ms: Some(0),
            startup: StartupSync::Rebased {
                main_ref: "origin/main".into(),
                base_sha: "component-base".into(),
                head_sha: "component-head".into(),
            },
        })
        .unwrap()
}
#[tokio::test]
async fn workcell_run_path_component_rejects_empty_unclaimed_missing_and_symlink_escape() {
    let workspace = tempfile::tempdir().unwrap();
    let repo = workspace.path().join("repo");
    let outside = workspace.path().join("outside");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(repo.join("agent.sh"), "fixture").unwrap();
    std::fs::write(outside.join("agent.sh"), "outside").unwrap();
    let empty = lease(workspace.path(), vec![]);
    let err = value(*selected_run_root(&empty, None).unwrap_err(), 403).await;
    assert_eq!(err["code"], "workcell_run_path_denied");
    assert!(
        err["reason"]
            .as_str()
            .unwrap()
            .contains("no claimed repo roots")
    );
    let lease = lease(workspace.path(), vec![repo.clone()]);
    assert_eq!(
        selected_run_root(&lease, None).unwrap(),
        repo.canonicalize().unwrap()
    );
    let err = value(*selected_run_root(&lease, Some(&outside)).unwrap_err(), 403).await;
    assert!(
        err["reason"]
            .as_str()
            .unwrap()
            .contains("outside the claimed workcell slice")
    );
    let missing = repo.join("missing");
    let err = value(*selected_run_root(&lease, Some(&missing)).unwrap_err(), 403).await;
    assert!(err["reason"].as_str().unwrap().contains("does not exist"));
    let err = value(
        *resolve_program_in_run_root(&repo, "missing-agent").unwrap_err(),
        403,
    )
    .await;
    assert!(
        err["reason"]
            .as_str()
            .unwrap()
            .contains("program does not exist")
    );
    assert_eq!(
        resolve_program_in_run_root(&repo, "agent.sh").unwrap(),
        repo.join("agent.sh")
    );
    let err = value(
        *resolve_program_in_run_root(&repo, outside.join("agent.sh").to_str().unwrap())
            .unwrap_err(),
        403,
    )
    .await;
    assert!(
        err["reason"]
            .as_str()
            .unwrap()
            .contains("outside the selected repo root")
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.join("agent.sh"), repo.join("escape")).unwrap();
        let err = value(
            *resolve_program_in_run_root(&repo, "escape").unwrap_err(),
            403,
        )
        .await;
        assert_eq!(err["code"], "workcell_run_path_denied");
    }
}
#[tokio::test]
async fn workcell_run_driver_error_component_keeps_typed_failures() {
    for (error, status, code) in [
        (
            DriverError::Workspace("workspace failure".into()),
            422,
            "workcell_run_workspace_denied",
        ),
        (
            DriverError::Policy("policy failure".into()),
            422,
            "workcell_run_policy_denied",
        ),
        (
            DriverError::SandboxUnavailable("missing".into()),
            424,
            "workcell_run_sandbox_unavailable",
        ),
        (
            DriverError::Supervision("failed".into()),
            500,
            "workcell_run_supervision_failed",
        ),
    ] {
        let body = value(driver_error_response(error), status).await;
        assert_eq!(body["code"], code);
        assert!(!body["reason"].as_str().unwrap().is_empty());
        assert_eq!(body["docs_url"], "docs/testing.md#workcells");
    }
}
#[test]
fn workcell_run_event_component_serializes_order_streams_budget_and_failed_outcome() {
    let sink = SerializingAgentSink::default();
    for event in [
        AgentEvent::Started { pid: 7 },
        AgentEvent::Stdout("agent-out".into()),
        AgentEvent::Stderr("agent-err".into()),
        AgentEvent::Budget {
            used: 33,
            limit: 32,
        },
        AgentEvent::Finished {
            exit_code: Some(1),
            timed_out: true,
            budget_exceeded: true,
        },
    ] {
        sink.emit(event);
    }
    let events = serde_json::to_value(sink.events()).unwrap();
    assert_eq!(events[0]["kind"], "started");
    assert_eq!(events[0]["pid"], 7);
    assert_eq!(events[1]["stream"], "stdout");
    assert_eq!(events[1]["text"], "agent-out");
    assert_eq!(events[2]["stream"], "stderr");
    assert_eq!(events[2]["text"], "agent-err");
    assert_eq!(events[3], json!({"kind":"budget","used":33,"limit":32}));
    assert_eq!(events[4]["kind"], "finished");
    assert_eq!(events[4]["exit_code"], 1);
    assert_eq!(events[4]["timed_out"], true);
    assert_eq!(events[4]["budget_exceeded"], true);
}
