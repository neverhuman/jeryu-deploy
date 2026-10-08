//! Public managed launch refusals; these tests never execute a legacy driver.
use super::*;
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;

async fn refused(state: Arc<WebState>, id: &str, body: Value) {
    let response = crate::web::workcells::run_agent(
        State(state.clone()),
        AxumPath(id.into()),
        axum::body::Bytes::from(body.to_string()),
    )
    .await;
    assert_eq!(response.status().as_u16(), 424);
    assert_eq!(response_json(response).await["code"], "sandbox_unavailable");
    assert!(state.agent_runs.list().is_empty());
    assert!(state.agent_runs.legacy_preparations().is_empty());
}
#[tokio::test]
async fn workcell_run_agent_refuses_arbitrary_host_command_without_writing_sentinel() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let workspace = tempdir().unwrap();
    let repo = workspace.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sentinel = workspace.path().join("host-sentinel");
    std::fs::write(&sentinel, "unchanged").unwrap();
    let inode = std::fs::metadata(&sentinel).unwrap().ino();
    let digest = Sha256::digest(std::fs::read(&sentinel).unwrap());
    let id = claim_run_workcell(state.clone(), workspace.path(), vec![repo.clone()], 23).await;
    refused(state, &id, serde_json::json!({
        "workcell_id":id,"runner_epoch":23,"repo_root":repo,
        "program":"/bin/sh","args":["-c",format!("printf overwritten > '{}'",sentinel.display())],
        "require_cgroup":false,"timeout_ms":10000,"output_budget_bytes":4096
    })).await;
    assert_eq!(std::fs::metadata(&sentinel).unwrap().ino(), inode);
    assert_eq!(Sha256::digest(std::fs::read(&sentinel).unwrap()), digest);
}
#[tokio::test]
async fn workcell_run_agent_denies_before_unclaimed_root_or_program_resolution() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let workspace = tempdir().unwrap();
    let repo = workspace.path().join("repo");
    let outside = workspace.path().join("outside");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let empty = claim_run_workcell(state.clone(), workspace.path(), vec![], 41).await;
    refused(
        state.clone(),
        &empty,
        serde_json::json!({
            "workcell_id":empty,"runner_epoch":41,"program":"/bin/sh","require_cgroup":false
        }),
    )
    .await;
    let id = claim_run_workcell(state.clone(), workspace.path(), vec![repo.clone()], 43).await;
    for (root, program) in [
        (outside.as_path(), "/bin/sh"),
        (repo.as_path(), "missing-agent"),
    ] {
        refused(state.clone(), &id, serde_json::json!({
            "workcell_id":id,"runner_epoch":43,"repo_root":root,"program":program,"require_cgroup":false
        })).await;
    }
}
#[tokio::test]
async fn workcell_run_agent_refuses_relative_and_directory_programs_before_driver() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let workspace = tempdir().unwrap();
    let repo = workspace.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("agent.sh"), "#!/bin/sh\necho should-not-run\n").unwrap();
    let id = claim_run_workcell(state.clone(), workspace.path(), vec![repo.clone()], 53).await;
    for program in ["agent.sh", "."] {
        refused(
            state.clone(),
            &id,
            serde_json::json!({
                "workcell_id":id,"runner_epoch":53,"repo_root":repo,"program":program,
                "require_cgroup":false,"output_budget_bytes":32
            }),
        )
        .await;
    }
}
