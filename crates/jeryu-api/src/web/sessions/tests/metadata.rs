use super::*;

#[tokio::test]
async fn session_component_registers_a_unique_branch_at_latest_main() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    let main = seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _) = fake_state(core, storage.path());
    let (created, _controls) = seed_session_component(
        &state,
        "alice/jeryu",
        "agent-7",
        "run-42",
        "local",
        storage.path(),
    );
    assert_eq!(created["branch"], "agents/agent-7/sessions/run-42");
    assert_ne!(created["branch"], "main");
    assert_eq!(created["base_oid"], main);
    assert_eq!(created["run_id"], "run-42");
    assert_eq!(created["session_id"], "run-42");
    assert_eq!(created["ws_scope"], "agent_run.run-42");
    assert_eq!(created["status_url"], "/api/v1/agent-runs/run-42");
    assert!(created.get("shell_run_id").is_none());
    let resolved = state.repo_manager.resolve_parts("alice", "jeryu").unwrap();
    let registered = RefService::new((*state.repo_manager).clone())
        .list_refs(&resolved)
        .unwrap()
        .into_iter()
        .find(|r| r.name == "refs/heads/agents/agent-7/sessions/run-42")
        .unwrap();
    assert_eq!(registered.oid, main);
}

#[tokio::test]
async fn session_component_resolves_non_main_default_branch() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    let main = seed_repo_on_branch(&core, storage.path(), "alice", "legacy-web", "master");
    let (state, _) = fake_state(core, storage.path());
    let (created, _controls) = seed_session_component(
        &state,
        "alice/legacy-web",
        "agent-7",
        "run-master",
        "local",
        storage.path(),
    );
    assert_eq!(created["branch"], "agents/agent-7/sessions/run-master");
    assert_eq!(created["base_oid"], main);
    let resolved = state
        .repo_manager
        .resolve_parts("alice", "legacy-web")
        .unwrap();
    let refs = RefService::new((*state.repo_manager).clone())
        .list_refs(&resolved)
        .unwrap();
    assert_eq!(
        refs.iter()
            .find(|r| r.name == "refs/heads/agents/agent-7/sessions/run-master")
            .unwrap()
            .oid,
        main
    );
}

#[tokio::test]
async fn durable_component_ids_skip_persisted_branches_and_continue_after_restart() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    let main = seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _) = fake_state(core, storage.path());
    let resolved = state.repo_manager.resolve_parts("alice", "jeryu").unwrap();
    let refs = RefService::new((*state.repo_manager).clone());
    refs.update_ref(
        &resolved,
        "test",
        "refs/heads/agents/agent-7/sessions/ar-000001",
        &main,
        None,
    )
    .unwrap();
    let names = refs.list_refs(&resolved).unwrap();
    let db = storage.path().join("durable-ids.sqlite");
    let before = crate::web::agent_runs::AgentRunStore::open(&db).unwrap();
    let first = super::super::next_session_run_id(&before, "agent-7", |name| {
        names.iter().any(|r| r.name == name)
    })
    .unwrap();
    assert_eq!(first, "ar-000002");
    drop(before);
    let after = crate::web::agent_runs::AgentRunStore::open(&db).unwrap();
    assert_eq!(
        super::super::next_session_run_id(&after, "agent-7", |name| names
            .iter()
            .any(|r| r.name == name))
        .unwrap(),
        "ar-000003"
    );
}

#[tokio::test]
async fn companion_component_records_linkage_and_delivers_terminate_without_claiming_guest_completion()
 {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _) = fake_state(core, storage.path());
    let (_created, _agent_controls) = seed_session_component(
        &state,
        "alice/jeryu",
        "agent",
        "run-agent",
        "local",
        storage.path(),
    );
    let (_, shell_controls) = seed_session_component(
        &state,
        "alice/jeryu",
        "shell",
        "run-shell",
        "local",
        storage.path(),
    );
    state
        .agent_runs
        .register_shell_companion("run-agent", "run-shell");
    let runs = response_json(
        super::super::list(
            State(state.clone()),
            AxumPath("alice/jeryu".into()),
            every_row(),
        )
        .await,
    )
    .await;
    let items = runs["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["shell_run_id"], "run-shell");
    let control = response_json(
        crate::web::agent_runs::control(
            State(state.clone()),
            AxumPath("run-shell".into()),
            Bytes::from(json!({"kind":"terminate"}).to_string()),
        )
        .await,
    )
    .await;
    assert_eq!(control["accepted"], true);
    assert_eq!(control["command"], "terminate");
    assert!(matches!(
        shell_controls.recv_timeout(Duration::from_secs(1)),
        Ok(jeryu_agentbridge::pty_driver::AgentControl::Terminate)
    ));
    assert_eq!(
        run_status(&state, "run-shell").await["state"],
        "running",
        "accepted is not completed"
    );
    state
        .agent_runs
        .simulate_test_completion("run-shell", AgentRunState::Failed);
    assert_eq!(run_status(&state, "run-shell").await["state"], "failed");
}

#[tokio::test]
async fn agent_runs_are_isolated_per_repo() {
    let storage = tempfile::tempdir().expect("git storage");
    let core = ForgeCore::new();
    seed_repo(&core, storage.path(), "alice", "repo-a");
    seed_repo(&core, storage.path(), "bob", "repo-b");
    let (state, _fake) = fake_state(core, storage.path());

    let (_, _controls_a) = seed_session_component(
        &state,
        "alice/repo-a",
        "agent-a",
        "run-a",
        "local",
        storage.path(),
    );
    let (_, _controls_b) = seed_session_component(
        &state,
        "bob/repo-b",
        "agent-b",
        "run-b",
        "local",
        storage.path(),
    );
    let run_a = "run-a".to_string();
    let run_b = "run-b".to_string();
    assert_ne!(run_a, run_b);

    let list_a = response_json(
        super::super::list(
            State(state.clone()),
            AxumPath("alice/repo-a".to_string()),
            every_row(),
        )
        .await,
    )
    .await;
    let items_a = list_a["items"].as_array().expect("items a");
    let ids_a: Vec<&str> = items_a
        .iter()
        .filter_map(|row| row["run_id"].as_str())
        .collect();
    assert!(
        ids_a.contains(&run_a.as_str()),
        "A's list must contain A's run"
    );
    assert!(
        !ids_a.contains(&run_b.as_str()),
        "A's list must NOT leak B's run (data-isolation): {ids_a:?}"
    );

    let list_b = response_json(
        super::super::list(
            State(state),
            AxumPath("bob/repo-b".to_string()),
            every_row(),
        )
        .await,
    )
    .await;
    let ids_b: Vec<&str> = list_b["items"]
        .as_array()
        .expect("items b")
        .iter()
        .filter_map(|row| row["run_id"].as_str())
        .collect();
    assert!(
        ids_b.contains(&run_b.as_str()),
        "B's list must contain B's run"
    );
    assert!(
        !ids_b.contains(&run_a.as_str()),
        "B's list must NOT leak A's run (data-isolation): {ids_b:?}"
    );
}

#[tokio::test]
async fn agent_runs_row_shape_matches_web_contract() {
    let storage = tempfile::tempdir().expect("git storage");
    let core = ForgeCore::new();
    seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _fake) = fake_state(core, storage.path());

    let (_, _controls) = seed_session_component(
        &state,
        "alice/jeryu",
        "agent-7",
        "run-42",
        "node-7",
        storage.path(),
    );

    let list = response_json(
        super::super::list(
            State(state),
            AxumPath("alice/jeryu".to_string()),
            every_row(),
        )
        .await,
    )
    .await;
    let items = list["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "exactly one run for this repo");
    let row = &items[0];
    assert_eq!(row["run_id"], "run-42");
    assert_eq!(row["branch"], "agents/agent-7/sessions/run-42");
    assert_eq!(row["runner"], "node-7");
    assert_eq!(row["status"], "running");
    assert_eq!(row["io_mode"], "pty");
    assert_eq!(row["ws_scope"], "agent_run.run-42");
    assert_eq!(row["agent"], "agent-7");
    // A pty session advertises the live control verbs the web terminal drives.
    let controls = row["supported_controls"].as_array().expect("controls");
    assert!(controls.iter().any(|c| c == "send_input"));
    assert!(controls.iter().any(|c| c == "terminate"));
    assert_eq!(row["tty_live"], true);
}
