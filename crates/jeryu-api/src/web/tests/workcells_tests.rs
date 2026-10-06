use super::*;

/// Builds a real bare repository at `<storage_root>/<owner>/<repo>.git` with a
/// base commit and a head commit that adds `head_files` (repo-relative path +
/// contents). Optional `base_files` let the base carry shared fixture files
/// that should not appear in the export diff. Returns `(base_sha, head_sha)` so
/// the workcell export slice gate can run a genuine `git diff base..head`
/// against it.
fn build_bare_repo_with_diff(
    storage_root: &std::path::Path,
    owner: &str,
    repo: &str,
    base_files: &[(&str, &str)],
    head_files: &[(&str, &str)],
) -> (String, String) {
    let bare = storage_root.join(owner).join(format!("{repo}.git"));
    std::fs::create_dir_all(bare.parent().expect("bare parent")).expect("create owner dir");

    // Work tree to author commits, then mirror-push into the bare repo.
    let work = storage_root.join(format!("{owner}-{repo}-work"));
    std::fs::create_dir_all(&work).expect("create work dir");

    let git = |args: &[&str], cwd: &std::path::Path| {
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
    };

    git(&["init", "--quiet", "-b", "main"], &work);
    // Base commit: a single placeholder file unrelated to the diff under test.
    std::fs::write(work.join("BASE.txt"), "base\n").expect("write base file");
    git(&["add", "BASE.txt"], &work);
    for (rel, contents) in base_files {
        let path = work.join(rel);
        std::fs::create_dir_all(path.parent().expect("base file parent"))
            .expect("create base file dir");
        std::fs::write(&path, contents).expect("write base file");
        git(&["add", rel], &work);
    }
    git(&["commit", "--quiet", "-m", "base"], &work);
    let base_sha = git(&["rev-parse", "HEAD"], &work);

    // Head commit: add the requested in-slice/out-of-slice files.
    for (rel, contents) in head_files {
        let path = work.join(rel);
        std::fs::create_dir_all(path.parent().expect("file parent")).expect("create file dir");
        std::fs::write(&path, contents).expect("write head file");
        git(&["add", rel], &work);
    }
    git(&["commit", "--quiet", "-m", "head"], &work);
    let head_sha = git(&["rev-parse", "HEAD"], &work);

    // Mirror into the bare repo the API's RepoManager will resolve.
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

    (base_sha, head_sha)
}

fn run_agent_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn run_agent_json(
    state: Arc<WebState>,
    path_workcell_id: &str,
    body: serde_json::Value,
) -> Value {
    response_json(
        crate::web::workcells::run_agent(
            State(state),
            AxumPath(path_workcell_id.to_string()),
            axum::body::Bytes::from(serde_json::to_vec(&body).unwrap()),
        )
        .await,
    )
    .await
}

async fn claim_run_workcell(
    state: Arc<WebState>,
    workspace_root: &std::path::Path,
    repo_roots: Vec<std::path::PathBuf>,
    runner_epoch: u64,
) -> String {
    let claim_body = serde_json::json!({
        "agent_id": format!("agent-wrath-run-{runner_epoch}"),
        "workspace_root": workspace_root,
        "repo_roots": repo_roots,
        "branch_budget": 1,
        "runner_id": format!("node-run-{runner_epoch}"),
        "runner_epoch": runner_epoch,
        "git_status_summary": "clean",
        "ci_snapshot_age_ms": 0,
        "startup": {
            "state": "rebased",
            "main_ref": "origin/main",
            "base_sha": "base",
            "head_sha": "head"
        }
    });
    let lease = response_json(
        crate::web::workcells::claim(
            State(state),
            axum::body::Bytes::from(serde_json::to_vec(&claim_body).unwrap()),
        )
        .await,
    )
    .await;
    lease["workcell_id"]
        .as_str()
        .expect("claimed workcell id")
        .to_string()
}

fn run_or_skip(
    driver: &AgentDriver,
    workspace: &std::path::Path,
    spec: &CommandSpec,
    sink: &CollectingSink,
) -> Option<jeryu_agentbridge::driver::AgentRunResult> {
    match driver.run(workspace, spec, sink) {
        Ok(result) => Some(result),
        Err(jeryu_agentbridge::driver::DriverError::SandboxUnavailable(reason)) => {
            if sandbox_is_required(std::env::var("JERYU_REQUIRE_SANDBOX").ok().as_deref()) {
                panic!("JERYU_REQUIRE_SANDBOX=1 but the sandbox is unavailable: {reason}");
            }
            eprintln!(
                "SKIP: sandbox unavailable (set JERYU_REQUIRE_SANDBOX=1 to fail closed): {reason}"
            );
            None
        }
        Err(other) => panic!("driver run failed unexpectedly: {other}"),
    }
}

/// A host that must prove the workcell path sets `JERYU_REQUIRE_SANDBOX=1`, so a
/// missing sandbox fails the test instead of reporting a silent green skip.
fn sandbox_is_required(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes"))
}

fn write_exec_script(label: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "jeryu-r5-{label}-{}-{}.sh",
        std::process::id(),
        jeryu_runner_core::receipt::now_ms()
    ));
    std::fs::write(&path, contents).expect("write staging script");
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&path)
            .expect("read script metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("mark script executable");
    }
    path
}

#[tokio::test]
async fn workcell_repair_flow_holds_exports_and_releases() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    // The export slice gate runs a real `git diff base..head`, so back the API
    // with a real bare repo. The head commit changes one in-slice file; the
    // lease is a whole-repo lease (workspace_root == repo_roots[0]), so the
    // slice permits it.
    let storage = tempfile::tempdir().expect("git storage dir");
    let workspace_root = storage.path().join("workspace").join("core").join("web");
    let (base_sha, head_sha) = build_bare_repo_with_diff(
        storage.path(),
        "alice",
        "jeryu",
        &[(
            ".github/workflows/ci.yml",
            "name: ci\non: [push, pull_request]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ci\n",
        )],
        &[("crates/jeryu-core/repaired.rs", "// repaired\n")],
    );
    let state = Arc::new(WebState::new_with_git_storage(
        core,
        storage.path().to_path_buf(),
    ));
    state
        .core
        .create_check_run(
            "alice",
            "jeryu",
            CreateCheckRunRequest {
                name: "ci/export-fixture".to_string(),
                head_sha: head_sha.clone(),
                status: Some(jeryu_core::CheckRunStatus::Completed),
                conclusion: Some(CheckConclusion::Success),
                ..CreateCheckRunRequest::default()
            },
        )
        .expect("seed exported head check-run");
    let repair_body = serde_json::json!({
        "agent_id": "agent-wrath-17",
        "workspace_root": workspace_root,
        "repo_roots": [workspace_root],
        "branch_budget": 2,
        "runner_id": "node-0",
        "runner_epoch": 7,
        "git_status_summary": "rebase failed",
        "ci_snapshot_age_ms": 1200,
        "startup": {
            "state": "rebased",
            "main_ref": "origin/main",
            "base_sha": base_sha,
            "head_sha": head_sha,
        },
        "ci_run_id": "ci-parent-17",
        "failed_run_id": "run-17",
        "failed_receipt_id": "receipt-17",
        "failure_log_digest": "sha256:deadbeef"
    });

    let response = response_json(
        crate::web::workcells::repair_live(
            State(state.clone()),
            axum::body::Bytes::from(serde_json::to_vec(&repair_body).unwrap()),
        )
        .await,
    )
    .await;

    let workcell_id = response["held"]["workcell_id"]
        .as_str()
        .expect("held workcell id");
    assert_eq!(response["held"]["state"], "held");
    assert_eq!(response["repairing"]["state"], "repairing");
    assert_eq!(
        response["held"]["frozen_snapshot"]["ci_run_id"],
        "ci-parent-17"
    );
    assert_eq!(
        response["held"]["frozen_snapshot"]["failed_run_id"],
        "run-17"
    );

    let export_body = serde_json::json!({
        "workcell_id": workcell_id,
        "runner_epoch": 7,
        "branch_suffix": "repair-17",
        "changed_files": ["crates/jeryu-core/repaired.rs"],
        "owner": "alice",
        "repo": "jeryu",
        "author": "agent-wrath-17",
        "title": "Repair failed tree",
        "body": "Repaired from failed tree"
    });
    let export = response_json(
        crate::web::workcells::export_pr(
            State(state.clone()),
            AxumPath(workcell_id.to_string()),
            axum::http::HeaderMap::new(),
            axum::body::Bytes::from(serde_json::to_vec(&export_body).unwrap()),
        )
        .await,
    )
    .await;
    assert!(
        export["branch"]
            .as_str()
            .expect("branch")
            .starts_with("agents/agent-wrath-17/workcells/")
    );
    assert!(export["pull_request_number"].as_u64().unwrap() > 0);
    assert_eq!(export["target_branch"], "main");

    let pr = state
        .core
        .get_pull_request(
            "alice",
            "jeryu",
            export["pull_request_number"].as_u64().unwrap(),
        )
        .expect("pull request exists");
    assert_eq!(pr.head.ref_name, export["branch"]);
    assert_eq!(pr.base.ref_name, "main");
    assert_eq!(pr.changed_files, vec!["crates/jeryu-core/repaired.rs"]);

    let check_runs = state
        .core
        .list_check_runs("alice", "jeryu", Some(&head_sha))
        .expect("list check-runs for exported head");
    assert!(
        check_runs.total_count >= 1,
        "the exported PR head should have CI check-runs, got {check_runs:?}"
    );
    assert!(
        check_runs
            .check_runs
            .iter()
            .any(|run| run.conclusion == Some(CheckConclusion::Success)),
        "the exported PR head CI set should include a green run"
    );

    let release = response_json(
        crate::web::workcells::release(
            State(state.clone()),
            AxumPath(workcell_id.to_string()),
            axum::body::Bytes::from(
                serde_json::to_vec(&serde_json::json!({
                    "runner_epoch": 7
                }))
                .unwrap(),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(release["state"], "released");
}

#[test]
fn require_sandbox_env_turns_unavailable_sandbox_into_a_failure() {
    assert!(!sandbox_is_required(None));
    assert!(!sandbox_is_required(Some("")));
    assert!(!sandbox_is_required(Some("0")));
    assert!(sandbox_is_required(Some("1")));
    assert!(sandbox_is_required(Some("true")));
}

#[tokio::test]
async fn workcell_run_agent_stays_in_claimed_repo_root_and_reports_events() {
    let _guard = run_agent_test_lock().lock().await;
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let workspace = tempdir().expect("create workcell workspace");
    let repo_root = workspace.path().join("repo-slice");
    std::fs::create_dir_all(&repo_root).expect("create claimed repo root");
    let claim_body = serde_json::json!({
        "agent_id": "agent-wrath-run",
        "workspace_root": workspace.path(),
        "repo_roots": [repo_root],
        "branch_budget": 1,
        "runner_id": "node-run",
        "runner_epoch": 23,
        "git_status_summary": "clean",
        "ci_snapshot_age_ms": 0,
        "startup": {
            "state": "rebased",
            "main_ref": "origin/main",
            "base_sha": "base",
            "head_sha": "head"
        }
    });
    let lease = response_json(
        crate::web::workcells::claim(
            State(state.clone()),
            axum::body::Bytes::from(serde_json::to_vec(&claim_body).unwrap()),
        )
        .await,
    )
    .await;
    let workcell_id = lease["workcell_id"]
        .as_str()
        .expect("claimed workcell id")
        .to_string();

    let outside_script = write_exec_script(
        "run-outside",
        r#"#!/bin/sh
echo outside
"#,
    );
    let denied_body = serde_json::json!({
        "workcell_id": workcell_id,
        "runner_epoch": 23,
        "program": outside_script,
        "require_cgroup": false
    });
    let denied = response_json(
        crate::web::workcells::run_agent(
            State(state.clone()),
            AxumPath(workcell_id.clone()),
            axum::body::Bytes::from(serde_json::to_vec(&denied_body).unwrap()),
        )
        .await,
    )
    .await;
    assert_eq!(denied["code"], "workcell_run_path_denied");

    let script_src = write_exec_script(
        "run-agent",
        r#"#!/bin/sh
echo agent-out
echo agent-err >&2
"#,
    );
    let staged = stage_editbot(&repo_root, &script_src).expect("stage run script in repo root");
    let run_body = serde_json::json!({
        "workcell_id": workcell_id,
        "runner_epoch": 23,
        "program": staged,
        "require_cgroup": false,
        "timeout_ms": 10000,
        "output_budget_bytes": 4096
    });
    let run = response_json(
        crate::web::workcells::run_agent(
            State(state.clone()),
            AxumPath(workcell_id.clone()),
            axum::body::Bytes::from(serde_json::to_vec(&run_body).unwrap()),
        )
        .await,
    )
    .await;
    let _ = std::fs::remove_file(&outside_script);
    let _ = std::fs::remove_file(&script_src);
    let _ = workspace.close();
    if run["code"] == "workcell_run_sandbox_unavailable" {
        eprintln!("SKIP: sandbox unavailable for workcell run route");
        return;
    }

    assert_eq!(run["workcell_id"], workcell_id);
    assert_eq!(run["outcome"]["succeeded"], true);
    let events = run["events"].as_array().expect("structured run events");
    assert!(events.iter().any(|event| event["kind"] == "started"));
    assert!(events.iter().any(|event| event["kind"] == "finished"));
    assert!(events.iter().any(|event| {
        event["stream"] == "stdout" && event["text"].as_str().unwrap_or("").contains("agent-out")
    }));
    assert!(events.iter().any(|event| {
        event["stream"] == "stderr" && event["text"].as_str().unwrap_or("").contains("agent-err")
    }));
}

#[tokio::test]
async fn workcell_run_agent_rejects_identity_epoch_and_inactive_cell() {
    let _guard = run_agent_test_lock().lock().await;
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let workspace = tempdir().expect("create workcell workspace");
    let repo_root = workspace.path().join("repo-slice");
    std::fs::create_dir_all(&repo_root).expect("create claimed repo root");
    let workcell_id =
        claim_run_workcell(state.clone(), workspace.path(), vec![repo_root], 31).await;

    let invalid = response_json(
        crate::web::workcells::run_agent(
            State(state.clone()),
            AxumPath(workcell_id.clone()),
            axum::body::Bytes::from_static(b"{ not json"),
        )
        .await,
    )
    .await;
    assert_eq!(invalid["code"], "workcell_invalid_request");

    let mismatch = run_agent_json(
        state.clone(),
        "different-cell",
        serde_json::json!({
            "workcell_id": workcell_id,
            "runner_epoch": 31,
            "program": "/bin/sh",
            "require_cgroup": false
        }),
    )
    .await;
    assert_eq!(mismatch["code"], "workcell_id_mismatch");

    let missing = run_agent_json(
        state.clone(),
        "missing-cell",
        serde_json::json!({
            "workcell_id": "missing-cell",
            "runner_epoch": 31,
            "program": "/bin/sh",
            "require_cgroup": false
        }),
    )
    .await;
    assert_eq!(missing["code"], "not_found");

    let fenced = run_agent_json(
        state.clone(),
        &workcell_id,
        serde_json::json!({
            "workcell_id": workcell_id,
            "runner_epoch": 30,
            "program": "/bin/sh",
            "require_cgroup": false
        }),
    )
    .await;
    assert_eq!(fenced["code"], "workcell_epoch_fenced");

    let release = response_json(
        crate::web::workcells::release(
            State(state.clone()),
            AxumPath(workcell_id.clone()),
            axum::body::Bytes::from(
                serde_json::to_vec(&serde_json::json!({
                    "runner_epoch": 31
                }))
                .unwrap(),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(release["state"], "released");

    let inactive = run_agent_json(
        state,
        &workcell_id,
        serde_json::json!({
            "workcell_id": workcell_id,
            "runner_epoch": 31,
            "program": "/bin/sh",
            "require_cgroup": false
        }),
    )
    .await;
    assert_eq!(inactive["code"], "workcell_claim_denied");
}

#[tokio::test]
async fn workcell_run_agent_rejects_unclaimed_roots_and_missing_programs() {
    let _guard = run_agent_test_lock().lock().await;
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let empty_workspace = tempdir().expect("create empty workcell workspace");
    let empty_cell =
        claim_run_workcell(state.clone(), empty_workspace.path(), Vec::new(), 41).await;
    let no_roots = run_agent_json(
        state.clone(),
        &empty_cell,
        serde_json::json!({
            "workcell_id": empty_cell,
            "runner_epoch": 41,
            "program": "/bin/sh",
            "require_cgroup": false
        }),
    )
    .await;
    assert_eq!(no_roots["code"], "workcell_run_path_denied");
    assert!(
        no_roots["reason"]
            .as_str()
            .expect("reason")
            .contains("no claimed repo roots")
    );

    let workspace = tempdir().expect("create workcell workspace");
    let repo_root = workspace.path().join("repo-slice");
    let outside_root = workspace.path().join("outside-slice");
    std::fs::create_dir_all(&repo_root).expect("create claimed repo root");
    std::fs::create_dir_all(&outside_root).expect("create outside repo root");
    let workcell_id =
        claim_run_workcell(state.clone(), workspace.path(), vec![repo_root.clone()], 43).await;

    let outside = run_agent_json(
        state.clone(),
        &workcell_id,
        serde_json::json!({
            "workcell_id": workcell_id,
            "runner_epoch": 43,
            "repo_root": outside_root,
            "program": "/bin/sh",
            "require_cgroup": false
        }),
    )
    .await;
    assert_eq!(outside["code"], "workcell_run_path_denied");
    assert!(
        outside["reason"]
            .as_str()
            .expect("reason")
            .contains("outside the claimed workcell slice")
    );

    let missing_program = run_agent_json(
        state.clone(),
        &workcell_id,
        serde_json::json!({
            "workcell_id": workcell_id,
            "runner_epoch": 43,
            "repo_root": repo_root,
            "program": "missing-agent",
            "require_cgroup": false
        }),
    )
    .await;
    assert_eq!(missing_program["code"], "workcell_run_path_denied");
    assert!(
        missing_program["reason"]
            .as_str()
            .expect("reason")
            .contains("program does not exist")
    );
}

#[tokio::test]
async fn workcell_run_agent_handles_relative_programs_and_driver_failures() {
    let _guard = run_agent_test_lock().lock().await;
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let workspace = tempdir().expect("create workcell workspace");
    let repo_root = workspace.path().join("repo-slice");
    std::fs::create_dir_all(&repo_root).expect("create claimed repo root");
    let workcell_id =
        claim_run_workcell(state.clone(), workspace.path(), vec![repo_root.clone()], 53).await;

    let script = repo_root.join("agent-relative.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
i=0
while [ "$i" -lt 200 ]; do
  echo "relative-agent-$i"
  i=$((i + 1))
  sleep 0.005
done
"#,
    )
    .expect("write relative program");
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&script)
            .expect("read script metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("mark script executable");
    }

    let run = run_agent_json(
        state.clone(),
        &workcell_id,
        serde_json::json!({
            "workcell_id": workcell_id,
            "runner_epoch": 53,
            "repo_root": repo_root,
            "program": "agent-relative.sh",
            "require_cgroup": false,
            "output_budget_bytes": 32
        }),
    )
    .await;
    if run["code"] == "workcell_run_sandbox_unavailable" {
        eprintln!("SKIP: sandbox unavailable for relative workcell run route");
        return;
    }
    assert_eq!(run["workcell_id"], workcell_id);
    assert_eq!(run["outcome"]["budget_exceeded"], true);
    assert!(
        run["events"]
            .as_array()
            .expect("events")
            .iter()
            .any(|event| { event["kind"] == "budget" && event["limit"] == 32 })
    );

    let directory_program = run_agent_json(
        state,
        &workcell_id,
        serde_json::json!({
            "workcell_id": workcell_id,
            "runner_epoch": 53,
            "repo_root": repo_root,
            "program": ".",
            "require_cgroup": false
        }),
    )
    .await;
    assert_eq!(
        directory_program["code"],
        "workcell_run_sandbox_unavailable"
    );
}

#[tokio::test]
async fn r5_jail_loop_exports_namespaced_branch_and_ci_evidence() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();

    let storage = tempfile::tempdir().expect("git storage dir");
    let workspace = tempdir().expect("create throwaway workspace");
    let (base_sha, head_sha) = build_bare_repo_with_diff(
        storage.path(),
        "alice",
        "jeryu",
        &[],
        &[(
            "src/r5.rs",
            "pub fn repaired() -> &'static str { \"r5\" }\n",
        )],
    );
    let state = Arc::new(WebState::new_with_git_storage(
        core.clone(),
        storage.path().to_path_buf(),
    ));
    let repair_body = serde_json::json!({
        "agent_id": "agent-wrath-17",
        "workspace_root": workspace.path(),
        "repo_roots": [workspace.path()],
        "branch_budget": 2,
        "runner_id": "node-0",
        "runner_epoch": 17,
        "git_status_summary": "rebase clean",
        "ci_snapshot_age_ms": 0,
        "startup": {
            "state": "rebased",
            "main_ref": "origin/main",
            "base_sha": base_sha,
            "head_sha": head_sha
        },
        "ci_run_id": "ci-r5-17",
        "failed_run_id": "run-r5-17",
        "failed_receipt_id": "receipt-r5-17",
        "failure_log_digest": "sha256:feedface"
    });

    let response = response_json(
        crate::web::workcells::repair_live(
            State(state.clone()),
            axum::body::Bytes::from(serde_json::to_vec(&repair_body).unwrap()),
        )
        .await,
    )
    .await;
    let workcell_id = response["held"]["workcell_id"]
        .as_str()
        .expect("held workcell id")
        .to_string();
    assert_eq!(response["held"]["state"], "held");
    assert_eq!(response["repairing"]["state"], "repairing");
    assert_eq!(response["held"]["startup_main_ref"], "origin/main");

    let script_src = write_exec_script(
        "editbot",
        r#"#!/bin/sh
set -eu
target_dir=${EDIT_TARGET%/*}
mkdir -p "$target_dir"
printf '%s' "$EDIT_CONTENT" > "$EDIT_TARGET"
"#,
    );
    let staged = stage_editbot(workspace.path(), &script_src).expect("stage edit script");
    let driver = AgentDriver::default()
        .with_require_cgroup(false)
        .with_timeout(Duration::from_secs(10));
    let spec = CommandSpec::new(staged.to_string_lossy().to_string())
        .env("EDIT_TARGET", "src/r5.rs")
        .env(
            "EDIT_CONTENT",
            "pub fn repaired() -> &'static str { \"r5\" }\n",
        );
    let sink = CollectingSink::new();
    let Some(result) = run_or_skip(&driver, workspace.path(), &spec, &sink) else {
        let _ = std::fs::remove_file(&script_src);
        let _ = workspace.close();
        return;
    };
    assert!(result.succeeded(), "edit inside the jail must succeed");
    assert!(
        workspace.path().join("src/r5.rs").is_file(),
        "the jailed edit must land inside the workspace"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("src/r5.rs")).expect("read repaired file"),
        "pub fn repaired() -> &'static str { \"r5\" }\n"
    );

    let export_body = serde_json::json!({
        "workcell_id": workcell_id,
        "runner_epoch": 17,
        "branch_suffix": "repair-17",
        "changed_files": ["src/r5.rs"],
        "owner": "alice",
        "repo": "jeryu",
        "author": "agent-wrath-17",
        "title": "Repair failed tree",
        "body": "Repaired from failed tree"
    });
    let export = response_json(
        crate::web::workcells::export_pr(
            State(state.clone()),
            AxumPath(workcell_id.clone()),
            axum::http::HeaderMap::new(),
            axum::body::Bytes::from(serde_json::to_vec(&export_body).unwrap()),
        )
        .await,
    )
    .await;
    let branch = export["branch"].as_str().expect("branch");
    assert!(branch.starts_with("agents/agent-wrath-17/workcells/"));
    assert_eq!(export["target_branch"], "main");
    assert!(export["pull_request_number"].as_u64().unwrap() > 0);

    let pr_number = export["pull_request_number"].as_u64().unwrap();
    let pr = state
        .core
        .get_pull_request("alice", "jeryu", pr_number)
        .expect("pull request exists");
    assert_eq!(pr.head.ref_name, branch);
    assert_eq!(pr.base.ref_name, "main");
    assert_eq!(pr.changed_files, vec!["src/r5.rs"]);

    let run = core
        .create_check_run(
            "alice",
            "jeryu",
            CreateCheckRunRequest {
                name: "r5-loop".to_string(),
                head_sha: pr.head.sha.clone(),
                status: Some(jeryu_core::CheckRunStatus::Completed),
                conclusion: Some(CheckConclusion::Success),
                output: Some(jeryu_core::CheckRunOutput {
                    title: "R5 lane green".to_string(),
                    summary: "claim -> rebase -> jailed edit -> PR -> CI evidence".to_string(),
                    text: None,
                }),
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap();

    let router = || {
        app(
            WebState::new(core.clone()),
            std::path::Path::new("/tmp/jeryu-no-spa"),
        )
    };
    let evidence = response_json(
        router()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/ci/runs/{}/evidence", run.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    let items = evidence.as_array().expect("evidence array");
    assert!(
        items.len() >= 3,
        "completed CI run must produce a receipt with evidence facets"
    );
    assert_eq!(items[0]["kind"], "run-metadata");
    assert_eq!(items[1]["kind"], "head-commit");
    assert_eq!(
        items[1]["payload"]["headSha"].as_str(),
        Some(pr.head.sha.as_str())
    );
    assert!(
        items
            .iter()
            .any(|item| item["kind"] == "conclusion" && item["payload"]["conclusion"] == "success")
    );

    let _ = std::fs::remove_file(&script_src);
    let _ = workspace.close();
}

/// WC-6: a workcell whose lease only permits `crates/jeryu-api`, exporting a
/// head commit that touches `crates/jeryu-core/x.rs`, must be slice-denied AND
/// must NOT create a pull request. This is the adversarial proof that the gate
/// is wired (the prior `let changed_files = Vec::new();` bypass would have
/// happily created the PR with no slice check).
#[tokio::test]
async fn workcell_export_slice_denies_out_of_slice_head_and_creates_no_pr() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let storage = tempfile::tempdir().expect("git storage dir");
    // Repo root = workspace_root; the lease only allows the api crate, but the
    // head commit changes a file in the core crate (out of slice).
    let repo_root = storage.path().join("work").join("repo");
    let allowed_subdir = repo_root.join("crates").join("jeryu-api");
    let (base_sha, head_sha) = build_bare_repo_with_diff(
        storage.path(),
        "alice",
        "jeryu",
        &[],
        &[("crates/jeryu-core/x.rs", "// out of slice\n")],
    );
    let state = Arc::new(WebState::new_with_git_storage(
        core,
        storage.path().to_path_buf(),
    ));

    // repo_roots is the in-slice subdir only; the runner unions in the
    // workspace_root, so the derived prefixes are ["crates/jeryu-api"].
    let repair_body = serde_json::json!({
        "agent_id": "agent-wrath-6",
        "workspace_root": repo_root,
        "repo_roots": [allowed_subdir],
        "branch_budget": 2,
        "runner_id": "node-6",
        "runner_epoch": 6,
        "git_status_summary": "rebase failed",
        "ci_snapshot_age_ms": 1200,
        "startup": {
            "state": "rebased",
            "main_ref": "origin/main",
            "base_sha": base_sha,
            "head_sha": head_sha,
        },
        "ci_run_id": "ci-parent-6",
        "failed_run_id": "run-6",
        "failed_receipt_id": "receipt-6",
        "failure_log_digest": "sha256:cafebabe"
    });
    let response = response_json(
        crate::web::workcells::repair_live(
            State(state.clone()),
            axum::body::Bytes::from(serde_json::to_vec(&repair_body).unwrap()),
        )
        .await,
    )
    .await;
    let workcell_id = response["held"]["workcell_id"]
        .as_str()
        .expect("held workcell id")
        .to_string();

    let export_body = serde_json::json!({
        "workcell_id": workcell_id,
        "runner_epoch": 6,
        "branch_suffix": "repair-6",
        "owner": "alice",
        "repo": "jeryu",
        "author": "agent-wrath-6",
        "title": "Repair failed tree",
        "body": "Repaired from failed tree"
    });
    let export = response_json(
        crate::web::workcells::export_pr(
            State(state.clone()),
            AxumPath(workcell_id.clone()),
            axum::http::HeaderMap::new(),
            axum::body::Bytes::from(serde_json::to_vec(&export_body).unwrap()),
        )
        .await,
    )
    .await;

    // The export is slice-denied, naming the out-of-slice path.
    assert_eq!(export["code"], "workcell_export_slice_denied");
    assert!(
        export["message"]
            .as_str()
            .expect("denial message")
            .contains("crates/jeryu-core/x.rs"),
        "denial must name the out-of-slice path: {export:?}"
    );

    // And crucially, NO pull request was created (the bypass would have made one).
    let pulls = state
        .core
        .list_pull_requests("alice", "jeryu", None)
        .expect("list pull requests");
    assert!(
        pulls.is_empty(),
        "a slice-denied export must not create a pull request, found: {pulls:?}"
    );
}

#[tokio::test]
async fn workcell_repair_live_requires_ci_run_id() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let repair_body = serde_json::json!({
        "agent_id": "agent-wrath-17",
        "workspace_root": "/workspace/core/web",
        "repo_roots": ["/workspace/core/web"],
        "branch_budget": 1,
        "runner_id": "node-0",
        "runner_epoch": 7,
        "git_status_summary": "rebase failed",
        "startup": {
            "state": "rebased",
            "main_ref": "origin/main",
            "base_sha": "abc123",
            "head_sha": "def456",
        },
        "failed_run_id": "legacy-run-17",
        "failed_receipt_id": "receipt-17",
        "failure_log_digest": "sha256:deadbeef"
    });

    let response = crate::web::workcells::repair_live(
        State(state),
        axum::body::Bytes::from(serde_json::to_vec(&repair_body).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let err = response_json(response).await;
    assert_eq!(err["code"], "ci_run_id_required");
    assert_eq!(
        err["purpose"], "hold a failed workcell and start live repair",
        "repair requests must carry the CI run they are repairing"
    );
    for key in ["reason", "common_fixes", "docs_url", "repair_hint"] {
        assert!(err.get(key).is_some(), "missing repair field: {key}");
    }
}
