//! Negative coverage for the durability of an acknowledged agent run.
//!
//! Three refusals the start path owes its callers, driven through the real
//! route: a run is never acknowledged before its intent is on disk, a run id is
//! never a number this process made up, and a store that cannot be written
//! answers a refusal instead of a run.

use std::path::Path;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response as AxumResponse;
use jeryu_core::ForgeCore;
use jeryu_runnerd::{HoldFailedTreeRequest, StartupSync, WorkcellClaimRequest};
use serde_json::{Value, json};
use tempfile::tempdir;

use super::AgentRunStore;
use crate::web::WebState;

async fn response_json(response: AxumResponse) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body reads");
    serde_json::from_slice(&bytes).expect("json response")
}

/// A workcell in live repair over `repo_root`, the one agent-run source the
/// start route serves without a container runtime.
fn repairing_workcell(state: &Arc<WebState>, workspace: &Path, repo_root: &Path) -> (String, u64) {
    let claim = WorkcellClaimRequest {
        agent_id: "agent-durability".to_string(),
        workspace_root: workspace.to_path_buf(),
        repo_roots: vec![repo_root.to_path_buf()],
        branch_budget: 1,
        runner_id: "runner-durability".to_string(),
        runner_epoch: 41,
        git_status_summary: "clean".to_string(),
        ci_snapshot_age_ms: Some(5),
        startup: StartupSync::Rebased {
            main_ref: "refs/heads/main".to_string(),
            base_sha: "base".to_string(),
            head_sha: "head".to_string(),
        },
    };
    let mut manager = state.workcells.lock().expect("workcell manager");
    let held = manager
        .hold_failed_tree(HoldFailedTreeRequest {
            claim,
            ci_run_id: "ci-durability".to_string(),
            failed_run_id: "failed-durability".to_string(),
            failed_receipt_id: "receipt-durability".to_string(),
            failure_log_digest: "sha256:durability".to_string(),
        })
        .expect("hold failed tree");
    let repairing = manager
        .begin_live_repair(&held.workcell_id, held.runner_epoch)
        .expect("begin live repair");
    (repairing.workcell_id, repairing.runner_epoch)
}

/// An agent script that exits at once, so the start answer is the only thing
/// under test and no child outlives the assertion.
fn write_agent_script(repo_root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = repo_root.join("agent.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf ok\n").expect("write agent script");
    let mut perms = std::fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).expect("chmod agent script");
}

fn start_body(workcell_id: &str, runner_epoch: u64, repo_root: &Path) -> Bytes {
    Bytes::from(
        json!({
            "source": {"kind": "workcell", "workcell_id": workcell_id, "runner_epoch": runner_epoch},
            "io_mode": "pipe",
            "repo_root": repo_root,
            "program": "agent.sh",
            "budget": {"wall_secs": 5, "output_bytes": 4096},
            "require_cgroup": false
        })
        .to_string(),
    )
}

/// The store cannot be written, so the start is refused: no run id is handed
/// out, nothing is registered as running, and no agent is launched.
#[tokio::test]
async fn a_run_whose_intent_cannot_be_recorded_is_refused() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let temp = tempdir().expect("workspace");
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).expect("repo root");
    write_agent_script(&repo_root);
    let (workcell_id, runner_epoch) = repairing_workcell(&state, temp.path(), &repo_root);

    state.agent_runs.intents().break_for_test();

    let response = super::super::agent_runs::start(
        State(state.clone()),
        start_body(&workcell_id, runner_epoch, &repo_root),
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let body = response_json(response).await;
    assert_eq!(body["code"], "agent_run_not_recorded");
    assert_eq!(body["purpose"], "start an agent run");
    assert!(
        body["agent_run_id"].is_null(),
        "refusal handed out an id: {body:?}"
    );
    assert!(
        state.agent_runs.list().is_empty(),
        "a refused start left a live run behind"
    );
}

/// The id in the start answer is already on disk when the answer is written:
/// the acknowledgement follows the durable record, never the other way round.
#[tokio::test]
async fn an_acknowledged_run_id_is_already_recorded() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let temp = tempdir().expect("workspace");
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).expect("repo root");
    write_agent_script(&repo_root);
    let (workcell_id, runner_epoch) = repairing_workcell(&state, temp.path(), &repo_root);

    let body = response_json(
        super::super::agent_runs::start(
            State(state.clone()),
            start_body(&workcell_id, runner_epoch, &repo_root),
        )
        .await,
    )
    .await;
    let run_id = body["agent_run_id"]
        .as_str()
        .unwrap_or_else(|| panic!("start should answer with a run id: {body:?}"));
    assert!(
        state.agent_runs.intents().is_recorded(run_id),
        "{run_id} was acknowledged without a recorded intent"
    );
}

/// Run ids come from the store, not from the process: a second registry over
/// the same file — the next process after a restart — carries on past the ids
/// the first one answered with instead of handing the same ones out again.
#[tokio::test]
async fn run_ids_are_never_handed_out_twice_across_a_restart() {
    let temp = tempdir().expect("store dir");
    let store_path = temp.path().join("shift.sqlite");

    let before = AgentRunStore::open(&store_path).expect("open the run store");
    let first = before.allocate_id().expect("first id");
    let second = before.allocate_id().expect("second id");
    drop(before);

    let after = AgentRunStore::open(&store_path).expect("reopen the run store");
    let third = after.allocate_id().expect("id after the restart");

    assert_eq!(
        [first.as_str(), second.as_str()],
        ["ar-000001", "ar-000002"]
    );
    assert_eq!(
        third, "ar-000003",
        "the restarted store handed out an id it had already answered with"
    );
}
