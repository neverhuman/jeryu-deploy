use super::*;

#[tokio::test]
async fn publish_advances_branch_ref_and_opens_pull_request() {
    let storage = tempfile::tempdir().expect("git storage");
    let core = ForgeCore::new();
    let main_oid = seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _fake) = fake_state(core, storage.path());

    let (_, _controls) = seed_session_component(
        &state,
        "alice/jeryu",
        "agent-7",
        "run-42",
        "local",
        storage.path(),
    );

    // The agent's work, captured host-side as a child commit of the session base.
    let head_oid = capture_agent_commit(
        storage.path(),
        "alice",
        "jeryu",
        &main_oid,
        "src/fix.rs",
        "pub fn fixed() -> u8 { 1 }\n",
    );
    assert_ne!(head_oid, main_oid);

    let published = response_json(
        super::super::publish(
            State(state.clone()),
            AxumPath("run-42".to_string()),
            HeaderMap::new(),
            Bytes::from(
                json!({
                    "head_oid": head_oid,
                    "author": "agent-7",
                    "title": "Session work"
                })
                .to_string(),
            ),
        )
        .await,
    )
    .await;

    assert!(
        published["pull_request_number"].as_u64().unwrap_or(0) > 0,
        "publish must open a real PR: {published:?}"
    );
    assert_eq!(published["base"], "main");
    assert_eq!(published["branch"], "agents/agent-7/sessions/run-42");

    // The session branch ref was advanced HOST-side to the captured head.
    let resolved = state
        .repo_manager
        .resolve_parts("alice", "jeryu")
        .expect("resolve bare");
    let refs = jeryu_gitd::refs::RefService::new((*state.repo_manager).clone());
    let branch = refs
        .list_refs(&resolved)
        .expect("list refs")
        .into_iter()
        .find(|r| r.name == "refs/heads/agents/agent-7/sessions/run-42")
        .expect("session branch still present");
    assert_eq!(
        branch.oid, head_oid,
        "the branch ref advanced to the captured commit"
    );

    // The PR is recorded against the repo with base main and the session head.
    let pr_number = published["pull_request_number"].as_u64().unwrap();
    let pr = state
        .core
        .get_pull_request("alice", "jeryu", pr_number)
        .expect("pull request exists");
    assert_eq!(pr.base.ref_name, "main");
    assert_eq!(pr.head.ref_name, "agents/agent-7/sessions/run-42");
    assert_eq!(pr.head.sha, head_oid);
}

#[tokio::test]
async fn publication_component_refuses_a_competing_branch_move_without_overwriting_it() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    let base = seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _) = fake_state(core, storage.path());
    let (_, _controls) = seed_session_component(
        &state,
        "alice/jeryu",
        "agent-7",
        "run-cas",
        "local",
        storage.path(),
    );
    let proposed = capture_agent_commit(
        storage.path(),
        "alice",
        "jeryu",
        &base,
        "proposed.txt",
        "proposed",
    );
    let competing = capture_agent_commit(
        storage.path(),
        "alice",
        "jeryu",
        &proposed,
        "competing.txt",
        "competing",
    );
    let resolved = state.repo_manager.resolve_parts("alice", "jeryu").unwrap();
    let refs = RefService::new((*state.repo_manager).clone());
    let branch = "refs/heads/agents/agent-7/sessions/run-cas";
    refs.update_ref(&resolved, "test", branch, &competing, Some(&base))
        .unwrap();
    let response = super::super::publish(
        State(state.clone()),
        AxumPath("run-cas".into()),
        HeaderMap::new(),
        Bytes::from(
            json!({"head_oid":proposed,"author":"agent-7","title":"must refuse"}).to_string(),
        ),
    )
    .await;
    assert!(!response.status().is_success());
    assert_eq!(
        refs.list_refs(&resolved)
            .unwrap()
            .iter()
            .find(|r| r.name == branch)
            .unwrap()
            .oid,
        competing
    );
    assert_eq!(run_status(&state, "run-cas").await["state"], "running");
    assert!(
        state
            .core
            .list_pull_requests("alice", "jeryu", None)
            .unwrap()
            .is_empty()
    );
}
