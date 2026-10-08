use super::*;

pub(super) fn git(args: &[&str], cwd: &Path) -> String {
    let output = crate::test_git::git_command()
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "jeryu-test")
        .env("GIT_AUTHOR_EMAIL", "jeryu-test@example.com")
        .env("GIT_COMMITTER_NAME", "jeryu-test")
        .env("GIT_COMMITTER_EMAIL", "jeryu-test@example.com")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub(super) fn seed_repo(core: &ForgeCore, storage_root: &Path, owner: &str, name: &str) -> String {
    seed_repo_on_branch(core, storage_root, owner, name, "main")
}

pub(super) fn seed_repo_on_branch(
    core: &ForgeCore,
    storage_root: &Path,
    owner: &str,
    name: &str,
    default_branch: &str,
) -> String {
    core.create_repository(
        owner,
        CreateRepositoryRequest {
            name: name.to_string(),
            private: false,
            description: None,
            default_branch: Some(default_branch.to_string()),
        },
    )
    .expect("create repository");

    let bare = storage_root.join(owner).join(format!("{name}.git"));
    std::fs::create_dir_all(bare.parent().expect("bare parent")).expect("create owner dir");
    let work = storage_root.join(format!("{owner}-{name}-work"));
    std::fs::create_dir_all(&work).expect("create work dir");

    git(&["init", "--quiet", "-b", default_branch], &work);
    std::fs::write(work.join("README.md"), "# seed\n").expect("write seed");
    git(&["add", "README.md"], &work);
    git(&["commit", "--quiet", "-m", "seed"], &work);
    let branch_oid = git(&["rev-parse", "HEAD"], &work);
    git(
        &[
            "clone",
            "--quiet",
            "--bare",
            ".",
            bare.to_str().expect("bare utf8"),
        ],
        &work,
    );
    branch_oid
}

pub(super) fn capture_agent_commit(
    storage_root: &Path,
    owner: &str,
    name: &str,
    parent_oid: &str,
    file: &str,
    content: &str,
) -> String {
    let bare = storage_root.join(owner).join(format!("{name}.git"));
    let work = storage_root.join(format!("{owner}-{name}-agent-work"));
    let _ = std::fs::remove_dir_all(&work);
    git(
        &[
            "clone",
            "--quiet",
            bare.to_str().expect("bare utf8"),
            work.to_str().expect("work utf8"),
        ],
        storage_root,
    );
    git(&["checkout", "--quiet", "--detach", parent_oid], &work);
    let path = work.join(file);
    std::fs::create_dir_all(path.parent().expect("file parent")).expect("create file dir");
    std::fs::write(&path, content).expect("write agent file");
    git(&["add", file], &work);
    git(&["commit", "--quiet", "-m", "agent work"], &work);
    let new_oid = git(&["rev-parse", "HEAD"], &work);
    // Land the object in the bare repo on a scratch ref so publish can advance the
    // session branch to it. The agent itself never pushes — this is host-side.
    git(
        &["push", "--quiet", "origin", "HEAD:refs/heads/_captured"],
        &work,
    );
    new_oid
}

pub(super) fn fake_state(
    core: ForgeCore,
    storage_root: &Path,
) -> (Arc<WebState>, Arc<FakeContainerRuntime>) {
    let fake = Arc::new(FakeContainerRuntime::default());
    let mut state = WebState::new_with_git_storage_and_warm_pool(
        core,
        storage_root.to_path_buf(),
        fake.clone(),
        0,
    );
    state.session_runtime.spawn_companion_shell = false;
    (Arc::new(state), fake)
}

pub(super) fn fake_state_with_runtime(
    core: ForgeCore,
    storage_root: &Path,
    runtime: super::super::SessionRuntimeConfig,
) -> (Arc<WebState>, Arc<FakeContainerRuntime>) {
    let fake = Arc::new(FakeContainerRuntime::default());
    let state = WebState::new_with_git_storage_and_warm_pool(
        core,
        storage_root.to_path_buf(),
        fake.clone(),
        0,
    )
    .with_session_runtime(runtime);
    (Arc::new(state), fake)
}

pub(super) fn warm_container_ids(state: &Arc<WebState>) -> Vec<String> {
    state
        .warm_pool
        .lock()
        .expect("warm pool mutex")
        .warm_container_ids()
}

pub(super) fn warm_depth(state: &Arc<WebState>) -> usize {
    state
        .warm_pool
        .lock()
        .expect("warm pool mutex")
        .warm_depth()
}

pub(super) fn start_warm_count(fake: &FakeContainerRuntime) -> usize {
    fake.lifecycle_ops()
        .iter()
        .filter(|op| matches!(op, LifecycleOp::StartWarm { .. }))
        .count()
}

pub(super) fn write_fake_docker(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("fake-docker.sh");
    std::fs::write(
        &path,
        "#!/bin/sh\nprintf 'DOCKER-ARGV: %s\\n' \"$*\"\nprintf 'DOCKER-AGENT-LIVE\\n'\n",
    )
    .expect("write fake docker");
    let mut perms = std::fs::metadata(&path)
        .expect("fake docker meta")
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod fake docker");
    path
}

pub(super) fn docker_runtime(fake_docker: &Path) -> super::super::SessionRuntimeConfig {
    super::super::SessionRuntimeConfig {
        runtime: super::super::SessionRuntime::Docker,
        docker_bin: Some(fake_docker.to_string_lossy().to_string()),
        spawn_companion_shell: false,
    }
}

pub(super) async fn response_json(response: AxumResponse) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("JSON")
}

pub(super) async fn refuse(state: &Arc<WebState>, repo: &str, body: Value, code: &str) -> Value {
    let response = super::super::create(
        State(state.clone()),
        AxumPath(repo.into()),
        HeaderMap::new(),
        None,
        None,
        Bytes::from(body.to_string()),
    )
    .await;
    let expected = if code == "host_launch_denied" {
        axum::http::StatusCode::FORBIDDEN
    } else {
        axum::http::StatusCode::FAILED_DEPENDENCY
    };
    assert_eq!(response.status(), expected);
    let refused = response_json(response).await;
    assert_eq!(refused["code"], code);
    assert!(refused["run_id"].is_null());
    assert!(state.agent_runs.list().is_empty());
    assert!(state.agent_runs.legacy_preparations().is_empty());
    refused
}

pub(super) fn component_plan(
    state: &WebState,
    repo_id: &str,
    agent: &str,
    run: &str,
    workspace: &Path,
) -> Result<jeryu_runner_oci::AgentSessionPlan, jeryu_runner_core::RunnerError> {
    let repo = super::super::find_repo(state, repo_id).expect("seeded repo");
    let resolved = state
        .repo_manager
        .resolve_parts(&repo.owner, &repo.name)
        .expect("bare");
    let refs = RefService::new((*state.repo_manager).clone())
        .list_refs(&resolved)
        .expect("refs");
    let base = refs
        .iter()
        .find(|r| r.name == format!("refs/heads/{}", repo.default_branch))
        .expect("default")
        .oid
        .clone();
    let job = JobRequest {
        job_id: run.into(),
        repo_id: repo.full_name.clone(),
        commit_sha: base.clone(),
        workspace: workspace.into(),
        command: "/opt/jeryu/bin/mock".into(),
        args: vec![],
        env: Default::default(),
        trust_tier: TrustTier::T4ForkPr,
        requested_runner: Some(RunnerClass::OciDocker),
        network_policy: NetworkPolicy::Deny,
        secret_policy: SecretPolicy::None,
        token_policy: TokenPolicy::None,
        timeout_ms: 1000,
        fork: true,
    };
    let decision = select_runner(&job)?;
    let plan = SandboxPlan::from_decision(workspace, &decision);
    super::super::plan_agent_session(
        &repo.owner,
        &repo.name,
        agent,
        run,
        &base,
        &resolved.path.to_string_lossy(),
        &job,
        &plan,
    )
}

pub(super) fn seed_session_component(
    state: &Arc<WebState>,
    repo: &str,
    agent: &str,
    run: &str,
    runner: &str,
    workspace: &Path,
) -> (
    Value,
    std::sync::mpsc::Receiver<jeryu_agentbridge::pty_driver::AgentControl>,
) {
    let plan = component_plan(state, repo, agent, run, workspace).expect("component plan");
    let repository = super::super::find_repo(state, repo).expect("repo");
    let resolved = state
        .repo_manager
        .resolve_parts(&repository.owner, &repository.name)
        .expect("bare");
    RefService::new((*state.repo_manager).clone())
        .update_ref(
            &resolved,
            "test-component",
            &format!("refs/heads/{}", plan.branch),
            &plan.base_oid,
            None,
        )
        .expect("explicit fixture branch");
    state
        .agent_runs
        .record_intent(&AgentRunIntent {
            run_id: Some(run.into()),
            kind: "session",
            repo: Some(repo.into()),
            program: "/opt/jeryu/bin/mock".into(),
            args: vec![],
            workspace: None,
        })
        .expect("explicit fixture intent");
    let (tx, rx) = std::sync::mpsc::channel();
    state.agent_runs.insert_session(SessionRecordInit {
        run_id: run.into(),
        repo: repo.into(),
        branch: plan.branch.clone(),
        base_oid: plan.base_oid.clone(),
        runner: runner.into(),
        agent: agent.into(),
        program: "/opt/jeryu/bin/mock".into(),
        args: vec![],
        workspace: workspace.into(),
        control_tx: Some(tx),
        owner: Some(repository.owner),
    });
    (
        serde_json::to_value(super::super::session_response(
            run,
            &plan.branch,
            &plan.base_oid,
        ))
        .expect("response"),
        rx,
    )
}

pub(super) async fn run_status(state: &Arc<WebState>, run: &str) -> Value {
    response_json(crate::web::agent_runs::status(State(state.clone()), AxumPath(run.into())).await)
        .await
}
