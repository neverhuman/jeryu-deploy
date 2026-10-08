use super::*;

#[tokio::test]
async fn create_refuses_before_claiming_or_starting_oci_cells() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, fake) = fake_state(core, storage.path());
    assert_eq!(warm_depth(&state), 0);
    assert_eq!(start_warm_count(&fake), 0);
    let before = warm_container_ids(&state);
    refuse(
        &state,
        "alice/jeryu",
        json!({"agent_id":"agent-7","run_id":"run-42"}),
        "sandbox_unavailable",
    )
    .await;
    assert_eq!(warm_container_ids(&state), before);
    assert_eq!(start_warm_count(&fake), 0);
    assert!(!state.agent_runs.intents().is_recorded("run-42"));
}

#[tokio::test]
async fn repeated_create_refusals_never_claim_or_refill_oci_cells() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, fake) = fake_state(core, storage.path());
    for run in ["run-1", "run-2"] {
        refuse(
            &state,
            "alice/jeryu",
            json!({"agent_id":"agent-7","run_id":run}),
            "sandbox_unavailable",
        )
        .await;
        assert!(!state.agent_runs.intents().is_recorded(run));
        assert_eq!(warm_depth(&state), 0);
        assert_eq!(start_warm_count(&fake), 0);
    }
}

#[tokio::test]
async fn create_admission_precedes_unknown_repo_lookup() {
    let storage = tempfile::tempdir().unwrap();
    let (state, _) = fake_state(ForgeCore::new(), storage.path());
    refuse(
        &state,
        "nobody/ghost",
        json!({"agent_id":"agent-1"}),
        "sandbox_unavailable",
    )
    .await;
}

#[tokio::test]
async fn create_refuses_before_invalid_identity_planning_and_planner_still_rejects_spoofing() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, fake) = fake_state(core, storage.path());
    for (agent, run) in [("../../heads/main", "run-1"), ("agent-1", "run 1")] {
        refuse(
            &state,
            "alice/jeryu",
            json!({"agent_id":agent,"run_id":run}),
            "sandbox_unavailable",
        )
        .await;
        let error = component_plan(&state, "alice/jeryu", agent, run, storage.path()).unwrap_err();
        assert_eq!(error.code(), "invalid_session_id");
    }
    assert_eq!(start_warm_count(&fake), 0);
}

#[tokio::test]
async fn create_refuses_before_a_broken_intent_store_and_record_failure_remains_visible() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _) = fake_state(core, storage.path());
    state.agent_runs.intents().break_for_test();
    refuse(
        &state,
        "alice/jeryu",
        json!({"agent_id":"agent-7"}),
        "sandbox_unavailable",
    )
    .await;
    assert!(
        state
            .agent_runs
            .record_intent(&AgentRunIntent {
                run_id: Some("broken-intent".into()),
                kind: "session",
                repo: Some("alice/jeryu".into()),
                program: "/opt/jeryu/bin/mock".into(),
                args: vec![],
                workspace: None,
            })
            .is_err()
    );
}

#[tokio::test]
async fn missing_agent_is_refused_before_resolution_or_authentication_import() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _) = fake_state(core, storage.path());
    refuse(
        &state,
        "alice/jeryu",
        json!({"agent_id":"ghost-agent","run_id":"run-missing"}),
        "sandbox_unavailable",
    )
    .await;
    assert!(
        std::fs::read_dir(state.session_auth_home.path())
            .unwrap()
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn native_cannot_launch_or_retry_docker_even_when_docker_is_configured() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    seed_repo(&core, storage.path(), "alice", "jeryu");
    let bin = tempfile::tempdir().unwrap();
    let docker = write_fake_docker(bin.path());
    let runtime = super::super::SessionRuntimeConfig {
        runtime: super::super::SessionRuntime::Native,
        docker_bin: Some(docker.to_string_lossy().into()),
        spawn_companion_shell: false,
    };
    let (state, fake) = fake_state_with_runtime(core, storage.path(), runtime);
    refuse(
        &state,
        "alice/jeryu",
        json!({"agent_id":"native-agent","command":"/bin/sh","args":["-c","printf forbidden"]}),
        "host_launch_denied",
    )
    .await;
    assert_eq!(start_warm_count(&fake), 0);
    let plan = component_plan(
        &state,
        "alice/jeryu",
        "native-agent",
        "run-native",
        storage.path(),
    )
    .unwrap();
    let (backend, spec) = super::super::runtime::resolve_session_backend(
        &state.session_runtime,
        "native-agent",
        Path::new("/absent/native"),
        storage.path(),
        Default::default(),
        &plan.container,
        "run-native",
    );
    assert!(matches!(
        backend,
        crate::web::agent_runs::PtyBackend::Native
    ));
    assert!(spec.is_none(), "missing native must not select Docker");
}

#[tokio::test]
async fn docker_with_or_without_executable_is_a_hard_public_refusal() {
    for configured in [false, true] {
        let storage = tempfile::tempdir().unwrap();
        let core = ForgeCore::new();
        seed_repo(&core, storage.path(), "alice", "jeryu");
        let bin = tempfile::tempdir().unwrap();
        let docker = write_fake_docker(bin.path());
        let mut runtime = docker_runtime(&docker);
        if !configured {
            runtime.docker_bin = None;
        }
        let (state, fake) = fake_state_with_runtime(core, storage.path(), runtime);
        refuse(
            &state,
            "alice/jeryu",
            json!({"agent_id":"codex","run_id":"run-docker"}),
            "host_launch_denied",
        )
        .await;
        assert_eq!(start_warm_count(&fake), 0);
        if !configured {
            let plan = component_plan(&state, "alice/jeryu", "codex", "run-docker", storage.path())
                .unwrap();
            let (_, spec) = super::super::runtime::resolve_session_backend(
                &state.session_runtime,
                "codex",
                Path::new("/absent/codex"),
                storage.path(),
                Default::default(),
                &plan.container,
                "run-docker",
            );
            assert!(spec.is_none());
        }
    }
}
