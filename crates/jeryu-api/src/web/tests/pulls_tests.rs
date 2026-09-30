use super::*;

fn bare_ref(storage_root: &std::path::Path, owner: &str, repo: &str, ref_name: &str) -> String {
    let bare = storage_root.join(owner).join(format!("{repo}.git"));
    let output = std::process::Command::new("git")
        .args(["rev-parse", ref_name])
        .current_dir(&bare)
        .output()
        .unwrap_or_else(|e| panic!("git rev-parse {ref_name} failed to spawn: {e}"));
    assert!(
        output.status.success(),
        "git rev-parse {ref_name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn build_bare_repo_with_main_and_feature(
    storage_root: &std::path::Path,
    owner: &str,
    repo: &str,
    feature_path: &str,
    feature_contents: &str,
) -> (String, String) {
    use std::process::Command;

    let bare = storage_root.join(owner).join(format!("{repo}.git"));
    std::fs::create_dir_all(bare.parent().expect("bare parent")).expect("create owner dir");
    let work = storage_root.join(format!("{owner}-{repo}-merge-work"));
    std::fs::create_dir_all(&work).expect("create work dir");

    let git = |args: &[&str], cwd: &std::path::Path| {
        let output = Command::new("git")
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
    std::fs::write(work.join("BASE.txt"), "base\n").expect("write base file");
    git(&["add", "BASE.txt"], &work);
    git(&["commit", "--quiet", "-m", "base"], &work);
    let base_sha = git(&["rev-parse", "HEAD"], &work);

    git(&["checkout", "--quiet", "-b", "feature"], &work);
    let path = work.join(feature_path);
    std::fs::create_dir_all(path.parent().expect("feature file parent"))
        .expect("create feature file dir");
    std::fs::write(&path, feature_contents).expect("write feature file");
    git(&["add", feature_path], &work);
    git(&["commit", "--quiet", "-m", "feature"], &work);
    let head_sha = git(&["rev-parse", "HEAD"], &work);
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
    git(&["symbolic-ref", "HEAD", "refs/heads/main"], bare.as_path());

    (base_sha, head_sha)
}

async fn passport_for_intrinsic_proof(
    status: Option<jeryu_core::CheckRunStatus>,
    conclusion: Option<CheckConclusion>,
) -> serde_json::Value {
    let core = ForgeCore::new();
    let _repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "intrinsic-proof".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    let pr = core
        .create_pull_request(
            "alice",
            "intrinsic-proof",
            "alice",
            CreatePullRequestRequest {
                title: "intrinsic proof posture".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some("intrinsic-proof-head".to_string()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    if status.is_some() || conclusion.is_some() {
        core.create_check_run(
            "alice",
            "intrinsic-proof",
            CreateCheckRunRequest {
                name: "jankurai/proof".to_string(),
                head_sha: "intrinsic-proof-head".to_string(),
                status,
                conclusion,
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap();
    }
    let state = WebState::new(core);
    let current = state
        .github
        .core()
        .get_pull_request("alice", "intrinsic-proof", pr.number)
        .unwrap();
    let detail = crate::web::pulls::detail_for_pr_with_audit_enforcement(&state, &current, true);
    serde_json::to_value(detail).unwrap()
}

async fn passport_for_required_check(
    status: Option<jeryu_core::CheckRunStatus>,
    conclusion: Option<CheckConclusion>,
) -> serde_json::Value {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "required-context".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    core.set_branch_protection(
        "alice",
        "required-context",
        "main",
        SetBranchProtectionRequest {
            required_status_checks: vec!["ci/required".to_string()],
            required_approving_review_count: 1,
            ..SetBranchProtectionRequest::default()
        },
    )
    .unwrap();
    let pr = core
        .create_pull_request(
            "alice",
            "required-context",
            "alice",
            CreatePullRequestRequest {
                title: "required context posture".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some("required-context-head".to_string()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    core.create_review(
        "alice",
        "required-context",
        pr.number,
        "bob",
        CreateReviewRequest {
            body: None,
            event: ReviewState::Approved,
            comments: Vec::new(),
            expected_head_sha: Some("required-context-head".to_string()),
        },
    )
    .unwrap();
    if status.is_some() || conclusion.is_some() {
        core.create_check_run(
            "alice",
            "required-context",
            CreateCheckRunRequest {
                name: "ci/required".to_string(),
                head_sha: "required-context-head".to_string(),
                status,
                conclusion,
                details_url: Some("https://forge.invalid/checks/required".to_string()),
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap();
    }
    let state = Arc::new(WebState::new(core));
    response_json(
        crate::web::pulls::detail(
            State(state),
            authenticated_account("bob"),
            AxumPath((repo.id.to_string(), pr.number)),
        )
        .await,
    )
    .await
}

#[tokio::test]
async fn pulls_routes_return_live_pr_detail_diff_checks_and_threads() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    let pr = core
        .create_pull_request(
            "alice",
            "jeryu",
            "alice",
            CreatePullRequestRequest {
                title: "feature".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some("head-a".to_string()),
                base_sha: Some("base-a".to_string()),
                changed_files: vec!["crates/jeryu-api/src/web.rs".to_string()],
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "ci/fast".to_string(),
            head_sha: "head-a".to_string(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Success),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    core.create_review(
        "alice",
        "jeryu",
        pr.number,
        "alice",
        jeryu_core::CreateReviewRequest {
            body: None,
            event: jeryu_core::ReviewState::Commented,
            comments: vec![jeryu_core::ReviewCommentInput {
                path: "crates/jeryu-api/src/web.rs".to_string(),
                line: Some(12),
                body: "check this".to_string(),
            }],
            expected_head_sha: Some("head-a".to_string()),
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new(core));

    let list = response_json(
        crate::web::pulls::list(
            State(state.clone()),
            AxumPath(repo.id.to_string()),
            Query(crate::web::pulls::PullListQuery {
                state: None,
                paging: Default::default(),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["title"], "feature");
    assert_eq!(list["items"][0]["repo"]["owner"], "alice");

    let detail = response_json(
        crate::web::pulls::detail(
            State(state.clone()),
            authenticated_account("alice"),
            AxumPath((repo.id.to_string(), pr.number)),
        )
        .await,
    )
    .await;
    assert_eq!(detail["summary"]["head_sha"], "head-a");
    assert_eq!(detail["head_tree_sha"], serde_json::Value::Null);
    assert_eq!(detail["base_tree_sha"], serde_json::Value::Null);
    assert_eq!(detail["reviews"][0]["head_sha"], "head-a");
    // A comment is not a verdict (jeryu-core split.11, "Comments preserve explicit
    // verdicts"), so the author's comment-only review is shown but not effective.
    assert_eq!(detail["reviews"][0]["effective"], false);
    assert_eq!(detail["reviews"][0]["stale"], false);
    // The viewer's review state is their effective verdict; a comment is not one.
    assert_eq!(
        detail["summary"]["review"]["user_review_state"],
        serde_json::Value::Null
    );
    assert!(
        detail["passport_hash"]
            .as_str()
            .unwrap()
            .starts_with("passport:")
    );

    let diff = response_json(
        crate::web::pulls::diff(
            State(state.clone()),
            AxumPath((repo.id.to_string(), pr.number)),
        )
        .await,
    )
    .await;
    assert_eq!(diff["files"][0]["path"], "crates/jeryu-api/src/web.rs");
    assert_eq!(diff["files"][0]["hunks"].as_array().unwrap().len(), 0);
    assert_eq!(diff["truncated"], false);

    let checks = response_json(
        crate::web::pulls::checks(
            State(state.clone()),
            AxumPath((repo.id.to_string(), pr.number)),
        )
        .await,
    )
    .await;
    assert_eq!(checks["passing"], 1);
    assert_eq!(checks["checks"][0]["status"], "success");

    let threads = response_json(
        crate::web::pulls::threads(
            State(state.clone()),
            AxumPath((repo.id.to_string(), pr.number)),
        )
        .await,
    )
    .await;
    assert_eq!(
        threads["threads"][0]["file_path"],
        "crates/jeryu-api/src/web.rs"
    );
    assert_eq!(
        threads["threads"][0]["comments"][0]["body_markdown"],
        "check this"
    );
}

#[tokio::test]
async fn pull_detail_marks_review_history_stale_and_uses_latest_current_head_verdict() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "review-heads".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    core.set_branch_protection(
        "alice",
        "review-heads",
        "main",
        SetBranchProtectionRequest {
            required_approving_review_count: 1,
            ..SetBranchProtectionRequest::default()
        },
    )
    .unwrap();
    let head_a = "a".repeat(40);
    let head_b = "b".repeat(40);
    let pr = core
        .create_pull_request(
            "alice",
            "review-heads",
            "alice",
            CreatePullRequestRequest {
                title: "head-bound review readback".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some(head_a.clone()),
                base_sha: Some("c".repeat(40)),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    core.create_review(
        "alice",
        "review-heads",
        pr.number,
        "bob",
        CreateReviewRequest {
            body: Some("approved old head".to_string()),
            event: ReviewState::Approved,
            comments: Vec::new(),
            expected_head_sha: Some(head_a.clone()),
        },
    )
    .unwrap();
    core.refresh_pull_request_heads_for_ref("alice", "review-heads", "feature", &head_b)
        .unwrap();
    let state = Arc::new(WebState::new(core.clone()));
    let path = || AxumPath((repo.id.to_string(), pr.number));

    let moved = response_json(
        crate::web::pulls::detail(State(state.clone()), authenticated_account("bob"), path()).await,
    )
    .await;
    assert_eq!(moved["summary"]["review"]["approvals"], 0);
    assert_eq!(
        moved["summary"]["review"]["user_review_state"],
        serde_json::Value::Null
    );
    assert_eq!(moved["reviews"][0]["head_sha"], head_a);
    assert_eq!(moved["reviews"][0]["effective"], false);
    assert_eq!(moved["reviews"][0]["stale"], true);

    core.create_review(
        "alice",
        "review-heads",
        pr.number,
        "bob",
        CreateReviewRequest {
            body: Some("fix the current head".to_string()),
            event: ReviewState::ChangesRequested,
            comments: Vec::new(),
            expected_head_sha: Some(head_b.clone()),
        },
    )
    .unwrap();
    let requested = response_json(
        crate::web::pulls::detail(State(state.clone()), authenticated_account("bob"), path()).await,
    )
    .await;
    assert_eq!(requested["summary"]["review"]["changes_requested"], 1);
    assert_eq!(
        requested["summary"]["review"]["user_review_state"],
        "CHANGES_REQUESTED"
    );
    assert!(
        requested["merge_passport"]["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|blocker| blocker["code"] == "passport_blocked_changes_requested")
    );

    core.create_review(
        "alice",
        "review-heads",
        pr.number,
        "bob",
        CreateReviewRequest {
            body: Some("current head repaired".to_string()),
            event: ReviewState::Approved,
            comments: Vec::new(),
            expected_head_sha: Some(head_b.clone()),
        },
    )
    .unwrap();
    let approved = response_json(
        crate::web::pulls::detail(State(state), authenticated_account("bob"), path()).await,
    )
    .await;
    assert_eq!(approved["summary"]["review"]["approvals"], 1);
    assert_eq!(approved["summary"]["review"]["changes_requested"], 0);
    assert_eq!(
        approved["summary"]["review"]["user_review_state"],
        "APPROVED"
    );
    assert_eq!(approved["reviews"].as_array().unwrap().len(), 3);
    assert_eq!(approved["reviews"][1]["state"], "CHANGES_REQUESTED");
    assert_eq!(approved["reviews"][1]["effective"], false);
    assert_eq!(approved["reviews"][1]["stale"], false);
    assert_eq!(approved["reviews"][2]["state"], "APPROVED");
    assert_eq!(approved["reviews"][2]["effective"], true);
    assert_eq!(approved["reviews"][2]["stale"], false);
    assert_eq!(approved["merge_passport"]["status"], "pass");
}

#[tokio::test]
async fn pulls_mutations_return_typed_repair_errors() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    core.set_branch_protection(
        "alice",
        "jeryu",
        "main",
        SetBranchProtectionRequest {
            required_status_checks: vec!["ci/fast".to_string()],
            required_approving_review_count: 1,
            ..SetBranchProtectionRequest::default()
        },
    )
    .unwrap();
    let pr = core
        .create_pull_request(
            "alice",
            "jeryu",
            "alice",
            CreatePullRequestRequest {
                title: "blocked".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some("head-b".to_string()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    let state = Arc::new(WebState::new(core));

    let missing_repo = crate::web::pulls::list(
        State(state.clone()),
        AxumPath("repo-missing".to_string()),
        Query(crate::web::pulls::PullListQuery {
            state: None,
            paging: Default::default(),
        }),
    )
    .await;
    assert_eq!(missing_repo.status(), StatusCode::NOT_FOUND);
    let missing_repo_body = response_json(missing_repo).await;
    assert_eq!(missing_repo_body["code"], "not_found");
    for key in [
        "purpose",
        "reason",
        "common_fixes",
        "docs_url",
        "repair_hint",
    ] {
        assert!(missing_repo_body.get(key).is_some(), "missing {key}");
    }

    let missing_pr = crate::web::pulls::detail(
        State(state.clone()),
        authenticated_account("bob"),
        AxumPath((repo.id.to_string(), 404)),
    )
    .await;
    assert_eq!(missing_pr.status(), StatusCode::NOT_FOUND);
    assert_eq!(response_json(missing_pr).await["code"], "not_found");

    let invalid_review = crate::web::pulls::review(
        State(state.clone()),
        authenticated_account("bob"),
        AxumPath((repo.id.to_string(), pr.number)),
        axum::body::Bytes::from("{"),
    )
    .await;
    assert_eq!(invalid_review.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response_json(invalid_review).await["code"],
        "pull_review_invalid_request"
    );

    let stale = crate::web::pulls::approve(
        State(state.clone()),
        authenticated_account("bob"),
        AxumPath((repo.id.to_string(), pr.number)),
        axum::body::Bytes::from(r#"{"expected_head_sha":"old"}"#),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let stale_body = response_json(stale).await;
    assert_eq!(stale_body["error"]["code"], "merge_sha_stale");
    assert_eq!(stale_body["error"]["details"]["current_head_sha"], "head-b");

    let detail = response_json(
        crate::web::pulls::detail(
            State(state.clone()),
            authenticated_account("bob"),
            AxumPath((repo.id.to_string(), pr.number)),
        )
        .await,
    )
    .await;
    assert_eq!(
        detail["merge_passport"]["blockers"][0]["code"],
        "passport_blocked_checks_missing"
    );
    assert!(
        detail["merge_passport"]["blockers"][0]["details"]
            .as_str()
            .unwrap()
            .contains("ci/fast")
    );
    let merge = crate::web::pulls::merge(
        State(state.clone()),
        axum::Extension(crate::web::auth::trusted_local_account(&state)),
        AxumPath((repo.id.to_string(), pr.number)),
        axum::body::Bytes::from(
            serde_json::json!({
                "expected_head_sha": "head-b",
                "expected_passport_hash": detail["passport_hash"].as_str().unwrap(),
                "merge_method": "merge"
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(merge.status(), StatusCode::CONFLICT);
    let merge_body = response_json(merge).await;
    assert_eq!(merge_body["code"], "merge_blocked");
    assert_eq!(merge_body["purpose"], "merge pull request");
    assert_eq!(
        merge_body["error"]["details"]["passport_status"],
        serde_json::json!("blocked")
    );
}

#[tokio::test]
async fn pulls_passport_gates_each_context_in_every_check_state() {
    use jeryu_core::CheckRunStatus::{Completed, InProgress};
    // (check state, expected passport status, expected first blocker code)
    let cells: [(Option<_>, Option<CheckConclusion>, &str, Option<&str>); 4] = [
        (
            None,
            None,
            "blocked",
            Some("passport_blocked_checks_missing"),
        ),
        (
            Some(Completed),
            Some(CheckConclusion::Failure),
            "blocked",
            Some("passport_blocked_checks"),
        ),
        (
            Some(InProgress),
            None,
            "blocked",
            Some("passport_blocked_pending_checks"),
        ),
        (
            Some(Completed),
            Some(CheckConclusion::Success),
            "pass",
            None,
        ),
    ];
    for (status, conclusion, expected_status, expected_code) in cells {
        for (context, detail) in [
            (
                "ci/required",
                passport_for_required_check(status.clone(), conclusion.clone()).await,
            ),
            (
                "jankurai/proof",
                passport_for_intrinsic_proof(status.clone(), conclusion.clone()).await,
            ),
        ] {
            let cell = format!("{context} {status:?}/{conclusion:?}");
            let passport = &detail["merge_passport"];
            assert_eq!(passport["status"], expected_status, "{cell}");
            match expected_code {
                Some(code) => assert_eq!(passport["blockers"][0]["code"], code, "{cell}"),
                None => assert_eq!(passport["blockers"], serde_json::json!([]), "{cell}"),
            }
        }
    }
}

#[test]
fn pulls_audit_enforcement_spellings_match_protected_core() {
    for value in ["1", "true", "yes", "on", " true "] {
        assert!(crate::web::pulls::audit_merge_enforced_value(Some(value)));
    }
    for value in [None, Some(""), Some("0"), Some("false"), Some("TRUE")] {
        assert!(!crate::web::pulls::audit_merge_enforced_value(value));
    }
}

/// The gate rolls out per repository: veox-telemetry's main scored 47 against a
/// floor of 85 on 2026-09-29, so turning the gate on family-wide would have
/// blocked every pull request it has. A repo is under the gate only when it is
/// named (or its whole owner is).
#[test]
fn pulls_audit_gate_rollout_names_the_repositories_it_covers() {
    let listed = crate::web::pulls::audit_gate_repo_listed;
    let setting = Some("jeryu/jeryu-deploy, jeryu/jeryu-ci-runner\nveox-ai/*");
    assert!(listed(setting, "jeryu", "jeryu-deploy"));
    assert!(listed(setting, "jeryu", "jeryu-ci-runner"));
    assert!(
        listed(setting, "JERYU", "Jeryu-Deploy"),
        "names are not case"
    );
    assert!(
        listed(setting, "veox-ai", "veox-telemetry"),
        "a whole owner"
    );
    assert!(!listed(setting, "jeryu", "jeryu-web"));
    assert!(!listed(setting, "other", "jeryu-deploy"));
    for empty in [None, Some(""), Some(" , ")] {
        assert!(!listed(empty, "jeryu", "jeryu-deploy"), "{empty:?}");
    }
}

/// With the gate on for a repository, an approval on a head whose
/// `jankurai/proof` is red, missing or unfinished is refused, and the refusal
/// carries the proof's own verdict so the pull request shows why. A scorer
/// failure is one of the red cases, not a pass.
#[tokio::test]
async fn pulls_jankurai_gate_refuses_approval_until_the_proof_passes() {
    use jeryu_core::{CheckRunOutput, CheckRunStatus};

    let proof = |status: Option<CheckRunStatus>,
                 conclusion: Option<CheckConclusion>,
                 title: Option<&'static str>| {
        let core = ForgeCore::new();
        core.create_repository(
            "veox-ai",
            CreateRepositoryRequest {
                name: "veox-telemetry".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
        let pr = core
            .create_pull_request(
                "veox-ai",
                "veox-telemetry",
                "alice",
                CreatePullRequestRequest {
                    title: "under the gate".to_string(),
                    head: "feature".to_string(),
                    base: "main".to_string(),
                    head_sha: Some("gated-head".to_string()),
                    ..CreatePullRequestRequest::default()
                },
            )
            .unwrap();
        if status.is_some() || conclusion.is_some() {
            core.create_check_run(
                "veox-ai",
                "veox-telemetry",
                CreateCheckRunRequest {
                    name: "jankurai/proof".to_string(),
                    head_sha: "gated-head".to_string(),
                    status,
                    conclusion,
                    output: title.map(|title| CheckRunOutput {
                        title: title.to_string(),
                        summary: "- score: 47\n- floor: 85".to_string(),
                        text: None,
                    }),
                    ..CreateCheckRunRequest::default()
                },
            )
            .unwrap();
        }
        (WebState::new(core), pr)
    };

    // Off for this repository: the verdict is reported, never a refusal.
    let (state, pr) = proof(
        Some(CheckRunStatus::Completed),
        Some(CheckConclusion::Failure),
        Some("score 47 < floor 85"),
    );
    assert!(
        crate::web::pulls::jankurai_gate_refusal_with(&state, &pr, false).is_none(),
        "a repository outside the rollout is not frozen by the gate"
    );

    // A passing proof is the only thing that clears the gate.
    let (state, pr) = proof(
        Some(CheckRunStatus::Completed),
        Some(CheckConclusion::Success),
        Some("score 92 >= floor 85"),
    );
    assert!(crate::web::pulls::jankurai_gate_refusal_with(&state, &pr, true).is_none());

    let cases = [
        (None, None, None, "has not run on this head", "missing"),
        (
            Some(CheckRunStatus::InProgress),
            None,
            None,
            "is still running on this head",
            "pending",
        ),
        (
            Some(CheckRunStatus::Completed),
            Some(CheckConclusion::Failure),
            Some("score 47 < floor 85"),
            "score 47 < floor 85",
            "failing",
        ),
        (
            Some(CheckRunStatus::Completed),
            Some(CheckConclusion::Failure),
            Some("jankurai audit produced no score: the auditor exited 101"),
            "the auditor exited 101",
            "failing",
        ),
    ];
    for (status, conclusion, title, expected, expected_state) in cases {
        let (state, pr) = proof(status, conclusion, title);
        let response = crate::web::pulls::jankurai_gate_refusal_with(&state, &pr, true)
            .unwrap_or_else(|| panic!("{expected_state}: the gate must refuse the approval"));
        assert_eq!(response.status(), StatusCode::CONFLICT, "{expected_state}");
        let body = response_json(response).await;
        assert_eq!(body["code"], "approval_blocked_jankurai_proof");
        assert_eq!(body["error"]["details"]["check_state"], expected_state);
        assert_eq!(body["error"]["details"]["head_sha"], "gated-head");
        let reason = body["reason"].as_str().unwrap().to_string();
        assert!(
            reason.contains(expected) && reason.contains("jankurai/proof"),
            "{expected_state}: {reason}"
        );
    }
}

#[tokio::test]
async fn pulls_mutations_submit_review_and_comment() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    let pr = core
        .create_pull_request(
            "alice",
            "jeryu",
            "alice",
            CreatePullRequestRequest {
                title: "ready".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some("head-ready".to_string()),
                base_sha: Some("base-ready".to_string()),
                changed_files: vec!["src/lib.rs".to_string()],
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "ci/fast".to_string(),
            head_sha: "head-ready".to_string(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Success),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new(core));
    let path = || AxumPath((repo.id.to_string(), pr.number));

    let review = response_json(
        crate::web::pulls::review(
            State(state.clone()),
            authenticated_account("bob"),
            path(),
            axum::body::Bytes::from(
                serde_json::json!({
                    "verdict": "comment",
                    "expected_head_sha": "head-ready",
                    "body_markdown": "reviewed",
                    "thread_comments": [{
                        "thread_id": null,
                        "body_markdown": "nit",
                        "file_path": "src/lib.rs",
                        "line": 7,
                        "anchor_sha": "head-ready"
                    }],
                    "evidence": null
                })
                .to_string(),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(review["summary"]["review"]["unresolved_threads"], 1);

    let comment = response_json(
        crate::web::pulls::comment(
            State(state.clone()),
            authenticated_account("carol"),
            path(),
            axum::body::Bytes::from(
                serde_json::json!({
                    "thread_id": null,
                    "body_markdown": "follow-up",
                    "file_path": "src/lib.rs",
                    "line": 8,
                    "anchor_sha": "head-ready"
                })
                .to_string(),
            ),
        )
        .await,
    )
    .await;
    assert!(
        comment["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["comments"][0]["body_markdown"] == "follow-up")
    );
    let reviews = state
        .github
        .core()
        .list_reviews("alice", "jeryu", pr.number)
        .unwrap();
    assert!(
        reviews
            .iter()
            .any(|review| review.author == "bob" && review.state == ReviewState::Commented)
    );
    assert!(
        reviews
            .iter()
            .any(|review| review.author == "carol" && review.state == ReviewState::Commented)
    );

    let self_review = crate::web::pulls::review(
        State(state.clone()),
        authenticated_account("alice"),
        path(),
        axum::body::Bytes::from(
            serde_json::json!({
                "verdict": "approve",
                "expected_head_sha": "head-ready",
                "body_markdown": "self approval",
                "thread_comments": [],
                "evidence": null
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(self_review.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response_json(self_review).await["code"],
        "pull_self_approval_forbidden"
    );

    let self_approve = crate::web::pulls::approve(
        State(state.clone()),
        authenticated_account("alice"),
        path(),
        axum::body::Bytes::from(r#"{"expected_head_sha":"head-ready"}"#),
    )
    .await;
    assert_eq!(self_approve.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response_json(self_approve).await["code"],
        "pull_self_approval_forbidden"
    );
    assert_eq!(
        state
            .github
            .core()
            .list_reviews("alice", "jeryu", pr.number)
            .unwrap()
            .iter()
            .filter(|review| review.state == ReviewState::Approved)
            .count(),
        0
    );
}

#[tokio::test]
async fn pulls_mutations_allow_record_only_autonomy_advisory() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
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
    let (base_sha, head_sha) = build_bare_repo_with_main_and_feature(
        storage.path(),
        "alice",
        "jeryu",
        "src/merge.rs",
        "pub fn merge_ready() -> bool { true }\n",
    );
    let pr = core
        .create_pull_request(
            "alice",
            "jeryu",
            "alice",
            CreatePullRequestRequest {
                title: "merge-ready".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some(head_sha.clone()),
                base_sha: Some(base_sha),
                changed_files: vec!["src/merge.rs".to_string()],
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    core.set_branch_protection(
        "alice",
        "jeryu",
        "main",
        SetBranchProtectionRequest {
            required_status_checks: vec!["ci/fast".to_string()],
            required_approving_review_count: 1,
            ..SetBranchProtectionRequest::default()
        },
    )
    .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "ci/fast".to_string(),
            head_sha: head_sha.clone(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Success),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "jeryu/autonomy".to_string(),
            head_sha: head_sha.clone(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::ActionRequired),
            details_url: Some("https://forge.invalid/advisory".to_string()),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new_with_git_storage(
        core,
        storage.path().to_path_buf(),
    ));
    let path = || AxumPath((repo.id.to_string(), pr.number));

    let approved = response_json(
        crate::web::pulls::approve(
            State(state.clone()),
            authenticated_account("bob"),
            path(),
            axum::body::Bytes::from(
                serde_json::json!({
                    "expected_head_sha": head_sha.clone(),
                    "body_markdown": "ship it"
                })
                .to_string(),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(approved["summary"]["review"]["approvals"], 1);
    let reviews = state
        .github
        .core()
        .list_reviews("alice", "jeryu", pr.number)
        .unwrap();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].author, "bob");

    let detail = response_json(
        crate::web::pulls::detail(State(state.clone()), authenticated_account("bob"), path()).await,
    )
    .await;
    assert_eq!(detail["summary"]["checks"]["total"], 2);
    assert_eq!(detail["summary"]["checks"]["failing"], 1);
    assert_eq!(detail["merge_passport"]["status"], "pass");
    assert_eq!(detail["merge_passport"]["blockers"], serde_json::json!([]));
    let head_tree = detail["head_tree_sha"]
        .as_str()
        .expect("real head commit must expose its tree");
    let base_tree = detail["base_tree_sha"]
        .as_str()
        .expect("real base commit must expose its tree");
    assert_eq!(head_tree.len(), 40);
    assert_eq!(base_tree.len(), 40);
    assert_ne!(head_tree, base_tree);
    let passport_hash = detail["passport_hash"].as_str().unwrap();

    state
        .github
        .core()
        .create_check_run(
            "alice",
            "jeryu",
            CreateCheckRunRequest {
                name: "jeryu/autonomy".to_string(),
                head_sha: head_sha.clone(),
                status: Some(jeryu_core::CheckRunStatus::Completed),
                conclusion: Some(CheckConclusion::Neutral),
                details_url: Some("https://forge.invalid/advisory/newer".to_string()),
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap();
    let advisory_refresh = response_json(
        crate::web::pulls::detail(State(state.clone()), authenticated_account("bob"), path()).await,
    )
    .await;
    assert_eq!(advisory_refresh["merge_passport"]["status"], "pass");
    assert_eq!(advisory_refresh["passport_hash"], passport_hash);

    state
        .github
        .core()
        .create_commit_status(
            "alice",
            "jeryu",
            &head_sha,
            "ci",
            CreateCommitStatusRequest {
                state: CommitStatusState::Failure,
                context: "ci/fast".to_string(),
                description: Some("required lane regressed".to_string()),
                target_url: None,
            },
        )
        .unwrap();
    let merge = crate::web::pulls::merge(
        State(state.clone()),
        axum::Extension(crate::web::auth::trusted_local_account(&state)),
        path(),
        axum::body::Bytes::from(
            serde_json::json!({
                "expected_head_sha": head_sha,
                "expected_passport_hash": passport_hash,
                "merge_method": "merge"
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(merge.status(), StatusCode::CONFLICT);
    assert_eq!(response_json(merge).await["code"], "merge_passport_stale");
}

#[tokio::test]
async fn web_pull_merge_advances_real_bare_main_ref() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
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
    let (base_sha, head_sha) = build_bare_repo_with_main_and_feature(
        storage.path(),
        "alice",
        "jeryu",
        "src/web_merge.rs",
        "pub fn merged() -> bool { true }\n",
    );
    let pr = core
        .create_pull_request(
            "alice",
            "jeryu",
            "alice",
            CreatePullRequestRequest {
                title: "real merge".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some(head_sha.clone()),
                base_sha: Some(base_sha.clone()),
                changed_files: vec!["src/web_merge.rs".to_string()],
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "ci/fast".to_string(),
            head_sha: head_sha.clone(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Success),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    core.create_review(
        "alice",
        "jeryu",
        pr.number,
        "bob",
        CreateReviewRequest {
            body: None,
            event: ReviewState::Approved,
            comments: Vec::new(),
            expected_head_sha: Some(head_sha.clone()),
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new_with_git_storage(
        core,
        storage.path().to_path_buf(),
    ));
    let path = || AxumPath((repo.id.to_string(), pr.number));
    let detail = response_json(
        crate::web::pulls::detail(State(state.clone()), authenticated_account("bob"), path()).await,
    )
    .await;
    assert_eq!(detail["merge_passport"]["status"], "pass");
    let passport_hash = detail["passport_hash"].as_str().unwrap();

    let response = crate::web::pulls::merge(
        State(state.clone()),
        axum::Extension(crate::web::auth::trusted_local_account(&state)),
        path(),
        axum::body::Bytes::from(
            serde_json::json!({
                "expected_head_sha": head_sha.clone(),
                "expected_passport_hash": passport_hash,
                "merge_method": "merge"
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let merged = response_json(response).await;

    assert_eq!(merged["summary"]["state"], "merged");
    // A merged pull request has no merge question left. The passport type has
    // no third verdict, so it stays "blocked", but the one blocker says so in
    // plain words instead of "the merge gate is blocked" or stale check rows;
    // `summary.state` is what tells a client the pull request is finished.
    let blockers = merged["merge_passport"]["blockers"].as_array().unwrap();
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert_eq!(blockers[0]["code"], "passport_blocked_mergeability");
    assert_eq!(blockers[0]["details"], "merged");
    assert!(
        blockers[0]["message"]
            .as_str()
            .unwrap()
            .contains("already merged"),
        "{blockers:?}"
    );
    assert_eq!(
        bare_ref(storage.path(), "alice", "jeryu", "refs/heads/main"),
        head_sha,
        "web merge route must move the real bare main ref"
    );
    let events = state
        .events
        .query(&crate::web::pipeline::EventsQuery::default())
        .unwrap();
    assert_eq!(events.len(), 1, "the merge is one pipeline event");
    assert_eq!(events[0].kind, "pr.merged");
    assert_eq!(events[0].repo.as_deref(), Some("alice/jeryu"));
    assert_eq!(events[0].pr, Some(1));
    assert_eq!(events[0].actor.as_deref(), Some("jeryu-admin"));
}

#[tokio::test]
async fn mounted_pulls_routes_are_reachable() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "mounted".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("head-c".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    let response = app(
        WebState::new(core),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    )
    .oneshot(
        Request::builder()
            .uri(format!("/api/v1/repos/{}/pulls", repo.id))
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["items"][0]["title"], "mounted");
}

#[tokio::test]
async fn web_pull_passport_uses_latest_run_per_check_name_within_current_head() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    let head_sha = "current-head".to_string();
    let pr = core
        .create_pull_request(
            "alice",
            "jeryu",
            "alice",
            CreatePullRequestRequest {
                title: "latest proof wins".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some(head_sha.clone()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    for conclusion in [CheckConclusion::Failure, CheckConclusion::Success] {
        core.create_check_run(
            "alice",
            "jeryu",
            CreateCheckRunRequest {
                name: "jankurai/proof".to_string(),
                head_sha: head_sha.clone(),
                status: Some(jeryu_core::CheckRunStatus::Completed),
                conclusion: Some(conclusion),
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap();
    }
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "jankurai/proof".to_string(),
            head_sha: "different-head".to_string(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Failure),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    core.create_review(
        "alice",
        "jeryu",
        pr.number,
        "bob",
        CreateReviewRequest {
            body: None,
            event: ReviewState::Approved,
            comments: Vec::new(),
            expected_head_sha: Some(head_sha.clone()),
        },
    )
    .unwrap();
    assert_eq!(
        core.list_check_runs("alice", "jeryu", Some(&head_sha))
            .unwrap()
            .total_count,
        2,
        "historical check-runs remain in forge history"
    );
    assert_eq!(
        core.list_check_runs("alice", "jeryu", None)
            .unwrap()
            .total_count,
        3,
        "another head's newer failure remains historical but cannot affect this passport"
    );
    let state = Arc::new(WebState::new(core));
    let detail = response_json(
        crate::web::pulls::detail(
            State(state),
            authenticated_account("bob"),
            AxumPath((repo.id.to_string(), pr.number)),
        )
        .await,
    )
    .await;

    assert_eq!(detail["summary"]["checks"]["total"], 1);
    assert_eq!(detail["summary"]["checks"]["passing"], 1);
    assert_eq!(detail["summary"]["checks"]["failing"], 0);
    assert_eq!(detail["merge_passport"]["status"], "pass");
}

/// The attention inbox treats a red check as blocking only when the base
/// branch requires it. Live on 2026-09-19 two pull requests whose passport
/// passed were listed as "checks failing" because `jankurai/proof`, which
/// their branch does not require, was red.
#[tokio::test]
async fn attention_posture_separates_required_from_optional_failing_checks() {
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
    let pr = core
        .create_pull_request(
            "alice",
            "jeryu",
            "alice",
            CreatePullRequestRequest {
                title: "feature".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some("deadbeef".to_string()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    let fail = |name: &str| {
        core.create_check_run(
            "alice",
            "jeryu",
            CreateCheckRunRequest {
                name: name.to_string(),
                head_sha: "deadbeef".to_string(),
                status: Some(jeryu_core::CheckRunStatus::Completed),
                conclusion: Some(CheckConclusion::Failure),
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap();
    };
    fail("jankurai/proof");
    let state = WebState::new(core.clone());
    let posture = crate::web::pulls::attention_posture(&state, &pr).expect("an open pull request");
    assert!(posture.failing.is_empty(), "{posture:?}");
    assert_eq!(posture.failing_optional, ["jankurai/proof"]);

    core.set_branch_protection(
        "alice",
        "jeryu",
        "main",
        SetBranchProtectionRequest {
            required_status_checks: vec!["jeryu/required".to_string()],
            ..SetBranchProtectionRequest::default()
        },
    )
    .unwrap();
    fail("jeryu/required");
    let posture = crate::web::pulls::attention_posture(&state, &pr).expect("an open pull request");
    assert_eq!(posture.failing, ["jeryu/required"]);
    assert_eq!(posture.failing_optional, ["jankurai/proof"]);
}

#[tokio::test]
async fn pull_checks_explain_each_failure_and_why_it_is_not_required() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    core.set_branch_protection(
        "alice",
        "jeryu",
        "main",
        SetBranchProtectionRequest {
            required_status_checks: vec!["jeryu/required".to_string()],
            ..SetBranchProtectionRequest::default()
        },
    )
    .unwrap();
    let pr = core
        .create_pull_request(
            "alice",
            "jeryu",
            "alice",
            CreatePullRequestRequest {
                title: "feature".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some("deadbeef".to_string()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "jankurai/proof".to_string(),
            head_sha: "deadbeef".to_string(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Failure),
            output: Some(jeryu_core::CheckRunOutput {
                title: "score 42 < floor 85".to_string(),
                summary: "- score: 42\n- floor: 85\n- caps applied: dead-language".to_string(),
                text: Some(
                    "Top findings:\n- dead-language at crates/jeryu-api/src/web.rs:412: \
                     the word is not neutral"
                        .to_string(),
                ),
            }),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    core.create_commit_status(
        "alice",
        "jeryu",
        "deadbeef",
        "gatebot",
        CreateCommitStatusRequest {
            state: CommitStatusState::Failure,
            context: "jeryu/required".to_string(),
            description: Some("cargo test failed".to_string()),
            target_url: Some("https://forge.invalid/gate/runs/7".to_string()),
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new(core));
    let checks = response_json(
        crate::web::pulls::checks(State(state), AxumPath((repo.id.to_string(), pr.number))).await,
    )
    .await;
    assert_eq!(checks["failing"], 2, "{checks}");
    let row = |name: &str| {
        checks["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name} row in {checks}"))
    };

    let proof = row("jankurai/proof");
    assert_eq!(proof["kind"], "check_run");
    assert_eq!(proof["title"], "score 42 < floor 85");
    assert_eq!(proof["required"], false);
    assert_eq!(
        proof["advisory"]["label"],
        "advisory - gate not enabled here"
    );
    assert!(
        proof["advisory"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("not one of them yet")),
        "the row says why the gate does not bind here: {proof}"
    );
    assert_eq!(proof["advisory"]["url"], "/quality-gate");
    assert_eq!(proof["web_url"], "/quality-gate/heads/alice/jeryu/deadbeef");
    // The link is only useful if the row shows it, and the reason is only
    // useful if the row shows the score, the floor, the caps and the findings.
    assert_eq!(proof["web_url_label"], "View report");
    assert_eq!(
        proof["description"],
        "- score: 42\n- floor: 85\n- caps applied: dead-language"
    );
    assert!(
        proof["details_text"]
            .as_str()
            .is_some_and(|text| text.contains("crates/jeryu-api/src/web.rs:412")),
        "{proof}"
    );

    let gate = row("jeryu/required");
    assert_eq!(gate["kind"], "status");
    assert_eq!(gate["status"], "failure");
    assert_eq!(gate["required"], true);
    assert!(gate["advisory"].is_null());
    assert_eq!(gate["description"], "cargo test failed");
    assert_eq!(gate["web_url"], "https://forge.invalid/gate/runs/7");
    assert_eq!(gate["web_url_label"], "View log");
}

/// A draft opened with an invented repository, so the fixture says nothing
/// about any deployment's own repositories. The core is backed by SQLite:
/// the in-memory one keeps no audit trail, and the transition's audit row is
/// part of what is under test. The returned directory has to outlive it.
fn draft_fixture() -> (
    tempfile::TempDir,
    Arc<WebState>,
    jeryu_core::Repository,
    u64,
) {
    let dir = tempdir().unwrap();
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).unwrap();
    let repo = core
        .create_repository(
            "acme",
            CreateRepositoryRequest {
                name: "widget-shop".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    let pr = core
        .create_pull_request(
            "acme",
            "widget-shop",
            "dana",
            CreatePullRequestRequest {
                title: "cart totals".to_string(),
                head: "cart-totals".to_string(),
                base: "rc/auto".to_string(),
                head_sha: Some("d".repeat(40)),
                base_sha: Some("e".repeat(40)),
                draft: true,
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    (dir, Arc::new(WebState::new(core)), repo, pr.number)
}

fn action_ids(detail: &Value) -> Vec<String> {
    detail["summary"]["available_actions"]
        .as_array()
        .expect("available_actions is an array")
        .iter()
        .map(|action| action["action_id"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn blocker_codes(detail: &Value) -> Vec<String> {
    detail["merge_passport"]["blockers"]
        .as_array()
        .expect("blockers is an array")
        .iter()
        .map(|blocker| blocker["code"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn draft_advertises_the_ready_route_and_the_passport_blocker_names_it() {
    let (_dir, state, repo, number) = draft_fixture();
    let detail = response_json(
        crate::web::pulls::detail(
            State(state),
            authenticated_account("dana"),
            AxumPath((repo.id.to_string(), number)),
        )
        .await,
    )
    .await;

    assert_eq!(detail["summary"]["draft"], true);
    assert!(action_ids(&detail).contains(&"pull.ready_for_review".to_string()));
    assert!(!action_ids(&detail).contains(&"pull.convert_to_draft".to_string()));
    let ready = detail["summary"]["available_actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["action_id"] == "pull.ready_for_review")
        .expect("the draft offers the ready action");
    assert_eq!(ready["method"], "POST");
    assert_eq!(
        ready["href"],
        format!("/api/v1/repos/acme/widget-shop/pulls/{number}/ready")
    );

    // The blocker states the rule and carries the route that clears it, so a
    // panel showing it can offer the button rather than only the sentence.
    let draft_blocker = detail["merge_passport"]["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|blocker| blocker["code"] == "passport_blocked_draft")
        .expect("a draft is blocked as a draft");
    assert_eq!(
        draft_blocker["details"],
        format!("POST /api/v1/repos/acme/widget-shop/pulls/{number}/ready")
    );
}

#[tokio::test]
async fn author_marks_a_draft_ready_and_the_draft_blocker_clears() {
    let (_dir, state, repo, number) = draft_fixture();
    let path = || AxumPath((repo.id.to_string(), number));

    let ready = response_json(
        crate::web::pulls::ready_for_review(
            State(state.clone()),
            authenticated_account("dana"),
            path(),
        )
        .await,
    )
    .await;
    assert_eq!(ready["summary"]["draft"], false);
    assert!(!blocker_codes(&ready).contains(&"passport_blocked_draft".to_string()));
    assert!(action_ids(&ready).contains(&"pull.convert_to_draft".to_string()));

    // The transition is on the pipeline timeline in the actor's name.
    let events = state
        .events
        .query(&crate::web::pipeline::EventsQuery {
            kind: Some("pr.ready_for_review".to_string()),
            ..Default::default()
        })
        .expect("events read");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].actor.as_deref(), Some("dana"));
    assert_eq!(events[0].summary, "acme/widget-shop#1 marked ready by dana");
    assert_eq!(events[0].outcome.as_deref(), Some("ready"));

    // And it has its own audit row, separate from the timeline event.
    let audit = state
        .core
        .list_audit(&format!("acme/widget-shop#{number}"))
        .expect("audit read");
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, "pull_request.ready_for_review");
    assert_eq!(audit[0].actor, "dana");
    assert_eq!(audit[0].detail["draft"], false);

    // Repeating it changes nothing and records nothing a second time.
    let again = response_json(
        crate::web::pulls::ready_for_review(
            State(state.clone()),
            authenticated_account("dana"),
            path(),
        )
        .await,
    )
    .await;
    assert_eq!(again["summary"]["draft"], false);
    assert_eq!(
        state
            .core
            .list_audit(&format!("acme/widget-shop#{number}"))
            .unwrap()
            .len(),
        1
    );

    // Back to a draft: the blocker returns, with a fresh audit row.
    let drafted = response_json(
        crate::web::pulls::convert_to_draft(
            State(state.clone()),
            authenticated_account("dana"),
            path(),
        )
        .await,
    )
    .await;
    assert_eq!(drafted["summary"]["draft"], true);
    assert!(blocker_codes(&drafted).contains(&"passport_blocked_draft".to_string()));
    let audit = state
        .core
        .list_audit(&format!("acme/widget-shop#{number}"))
        .expect("audit read");
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[1].action, "pull_request.convert_to_draft");
}

#[tokio::test]
async fn an_admin_moves_someone_elses_draft_and_a_stranger_cannot() {
    let (_dir, state, repo, number) = draft_fixture();
    let path = || AxumPath((repo.id.to_string(), number));

    let refused = crate::web::pulls::ready_for_review(
        State(state.clone()),
        authenticated_account("mallory"),
        path(),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let body = response_json(refused).await;
    assert_eq!(body["code"], "pull_draft_forbidden");

    let by_admin = response_json(
        crate::web::pulls::ready_for_review(
            State(state.clone()),
            authenticated_admin_account("root"),
            path(),
        )
        .await,
    )
    .await;
    assert_eq!(by_admin["summary"]["draft"], false);
    let audit = state
        .core
        .list_audit(&format!("acme/widget-shop#{number}"))
        .expect("audit read");
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].actor, "root");
}

#[tokio::test]
async fn a_closed_pull_request_has_no_draft_transition_left() {
    let (_dir, state, repo, number) = draft_fixture();
    state
        .core
        .update_pull_request(
            "acme",
            "widget-shop",
            number,
            jeryu_core::UpdatePullRequestRequest {
                state: Some(jeryu_core::PullRequestState::Closed),
                ..Default::default()
            },
        )
        .unwrap();
    let refused = crate::web::pulls::ready_for_review(
        State(state.clone()),
        authenticated_account("dana"),
        AxumPath((repo.id.to_string(), number)),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(response_json(refused).await["code"], "pull_draft_not_open");
}

#[tokio::test]
async fn the_pull_list_carries_drafts_and_their_non_main_base() {
    let (_dir, state, repo, number) = draft_fixture();
    let list = response_json(
        crate::web::pulls::list(
            State(state),
            AxumPath(repo.id.to_string()),
            Query(crate::web::pulls::PullListQuery {
                state: Some("open".to_string()),
                paging: Default::default(),
            }),
        )
        .await,
    )
    .await;
    // `state=open` is the default view: a draft into a branch that is not the
    // default one is in it, or the owner cannot find their own pull request.
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["number"], number);
    assert_eq!(list["items"][0]["draft"], true);
    assert_eq!(list["items"][0]["base_ref"], "rc/auto");
}
