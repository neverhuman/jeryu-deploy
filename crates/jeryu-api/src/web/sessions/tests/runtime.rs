use super::*;

#[tokio::test]
async fn session_store_component_replays_explicit_terminal_bytes_without_a_launch() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _) = fake_state(core, storage.path());
    refuse(
        &state,
        "alice/jeryu",
        json!({"agent_id":"streamer","command":"/bin/sh"}),
        "sandbox_unavailable",
    )
    .await;
    let (_, _controls) = seed_session_component(
        &state,
        "alice/jeryu",
        "streamer",
        "run-stream",
        "local",
        storage.path(),
    );
    let bytes = b"SESSION-COMPONENT\r\n\xff";
    state.agent_runs.push_test_tty(
        "run-stream",
        crate::web::agent_runs::test_raw_tty_event("run-stream", 1, bytes),
    );
    let status = run_status(&state, "run-stream").await;
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(status["tty_events"][0]["bytes_b64"].as_str().unwrap())
            .unwrap(),
        bytes
    );
    assert_eq!(status["state"], "running");
}

#[test]
fn session_planner_component_sets_workspace_and_branch_environment() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    let base = seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _) = fake_state(core, storage.path());
    let plan = component_plan(
        &state,
        "alice/jeryu",
        "agent-env",
        "run-env",
        storage.path(),
    )
    .unwrap();
    assert_eq!(plan.workspace, storage.path());
    assert!(plan.container.env.contains(&(
        "JERYU_BRANCH".into(),
        "agents/agent-env/sessions/run-env".into()
    )));
    assert!(
        plan.container
            .env
            .contains(&("JANKURAI_DIFF_BASE".into(), base))
    );
}

#[tokio::test]
async fn session_store_component_reports_a_missing_binary_without_launching_it() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _) = fake_state(core, storage.path());
    let (_, _controls) = seed_session_component(
        &state,
        "alice/jeryu",
        "ghost-agent",
        "run-missing",
        "local",
        storage.path(),
    );
    state.agent_runs.note_agent_unavailable("run-missing");
    let status = run_status(&state, "run-missing").await;
    assert_eq!(status["state"], "failed");
    let text: String = status["tty_events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["text"].as_str())
        .collect();
    assert!(text.contains("agent ghost-agent not available: /opt/jeryu/bin/mock"));
}

#[test]
fn legacy_oci_component_constructs_hardened_command_without_executing_it() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _) = fake_state(core, storage.path());
    let bin = tempfile::tempdir().unwrap();
    let docker = write_fake_docker(bin.path());
    let plan =
        component_plan(&state, "alice/jeryu", "codex", "run-docker", storage.path()).unwrap();
    let (_, spec) = super::super::runtime::resolve_session_backend(
        &docker_runtime(&docker),
        "codex",
        Path::new("/absent/codex"),
        storage.path(),
        Default::default(),
        &plan.container,
        "run-docker",
    );
    let spec = spec.unwrap();
    let argv = spec.args.join(" ");
    assert!(argv.contains("--read-only"));
    assert!(argv.contains("--network none"));
    assert!(!argv.contains("--network bridge") && !argv.contains("--network host"));
    assert!(argv.contains(":/workspace"));
    assert!(argv.contains("--name jeryu-agent-run-docker") && argv.contains(" codex"));
}

#[test]
fn legacy_materialization_component_creates_real_checkout_on_the_namespaced_branch() {
    let storage = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    let base = seed_repo(&core, storage.path(), "alice", "jeryu");
    let (state, _) = fake_state(core, storage.path());
    let workspace = storage.path().join("component-checkout");
    let bare = storage.path().join("alice/jeryu.git");
    super::super::runtime::materialize_workspace(
        &state.repo_manager.config().git_bin,
        &bare,
        &workspace,
        "agents/agent-7/sessions/run-checkout",
        &base,
    )
    .unwrap();
    assert!(workspace.join(".git").exists());
    assert_eq!(git(&["rev-parse", "HEAD"], &workspace), base);
    assert_eq!(
        git(&["rev-parse", "--abbrev-ref", "HEAD"], &workspace),
        "agents/agent-7/sessions/run-checkout"
    );
    assert!(
        !workspace.join(".agent-home").exists(),
        "materialization imports no auth"
    );
}

#[test]
fn legacy_auth_component_seeds_its_dns_file_without_a_launch() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    super::super::seed_agent_auth_from_home(workspace.path(), "codex", home.path());
    let dns = std::fs::read_to_string(workspace.path().join(".resolv.conf")).unwrap();
    assert!(
        dns.contains("nameserver 8.8.8.8")
            && dns.contains("nameserver 8.8.4.4")
            && dns.contains("options ndots:0")
    );
}
