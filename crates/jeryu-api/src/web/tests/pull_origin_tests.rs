//! The contributed-origin gate: a pull request the importer brought in from a
//! mirror merges only on a configured maintainer's approval, and a pull
//! request written inside the forge is untouched by the rule.

use super::*;

/// One repository with a real bare git store, one pull request on a green
/// required-free head, and an approval from `bob`, who is not a maintainer.
fn forge_with_approved_pull(
    storage: &std::path::Path,
    name: &str,
) -> (ForgeCore, jeryu_core::Repository, u64, String) {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "acme",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    let (base_sha, head_sha) = build_bare_repo_with_main_and_feature(
        storage,
        "acme",
        name,
        "src/contribution.rs",
        "pub fn contributed() -> bool { true }\n",
    );
    let pr = core
        .create_pull_request(
            "acme",
            name,
            "outsider",
            CreatePullRequestRequest {
                title: "add a widget".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some(head_sha.clone()),
                base_sha: Some(base_sha.clone()),
                changed_files: vec!["src/contribution.rs".to_string()],
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    core.create_check_run(
        "acme",
        name,
        CreateCheckRunRequest {
            name: "ci/fast".to_string(),
            head_sha: head_sha.clone(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Success),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    approve_as(&core, name, pr.number, "bob", &head_sha);
    (core, repo, pr.number, head_sha)
}

fn approve_as(core: &ForgeCore, name: &str, number: u64, login: &str, head_sha: &str) {
    core.create_review(
        "acme",
        name,
        number,
        login,
        CreateReviewRequest {
            body: None,
            event: ReviewState::Approved,
            comments: Vec::new(),
            expected_head_sha: Some(head_sha.to_string()),
        },
    )
    .unwrap();
}

async fn mark_origin(
    state: &Arc<WebState>,
    repo_id: &str,
    number: u64,
    actor: Extension<AccountSummary>,
    origin: &str,
) -> AxumResponse {
    crate::web::pull_origin::set(
        State(state.clone()),
        actor,
        AxumPath((repo_id.to_string(), number)),
        axum::body::Bytes::from(serde_json::json!({ "origin": origin }).to_string()),
    )
    .await
}

fn blocker_codes(detail: &Value) -> Vec<String> {
    detail["merge_passport"]["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|blocker| blocker["code"].as_str().unwrap().to_string())
        .collect()
}

/// A contributed pull request with a clean review stays unmerged until a
/// maintainer approves: the agent reviewer's approval is advice, the merge
/// route refuses, and the maintainer's approval is what clears the gate.
#[tokio::test]
async fn a_contributed_pull_waits_for_a_maintainer_however_clean_its_review() {
    let storage = tempfile::tempdir().expect("git storage dir");
    let (core, repo, number, head_sha) = forge_with_approved_pull(storage.path(), "widget");
    let state = Arc::new(
        WebState::new_with_git_storage(core.clone(), storage.path().to_path_buf())
            .with_pull_trust(&["importbot"], &["maintainer"]),
    );
    let path = || AxumPath((repo.id.to_string(), number));

    // Before the importer marks it, the pull request reads as internal and the
    // gate is open: this is the posture the rest of the forge already has.
    let detail = response_json(
        crate::web::pulls::detail(State(state.clone()), authenticated_account("bob"), path()).await,
    )
    .await;
    assert_eq!(detail["merge_passport"]["status"], "pass");

    // The importer marks it as brought in from a mirror.
    let marked = mark_origin(
        &state,
        &repo.id.to_string(),
        number,
        authenticated_account("importbot"),
        "contributed",
    )
    .await;
    assert_eq!(marked.status(), StatusCode::OK);
    let marked = response_json(marked).await;
    assert_eq!(marked["origin"], "contributed");
    assert_eq!(marked["contributed"], true);
    assert_eq!(marked["set_by"], "importbot");
    assert_eq!(marked["maintainer_approved"], false);

    // `bob`'s approval is on the exact head and nothing else blocks, but bob
    // is not a maintainer, so the gate holds and says why.
    let detail = response_json(
        crate::web::pulls::detail(State(state.clone()), authenticated_account("bob"), path()).await,
    )
    .await;
    assert_eq!(detail["merge_passport"]["status"], "blocked");
    assert_eq!(
        blocker_codes(&detail),
        vec!["passport_blocked_contributed_origin".to_string()],
    );
    assert_eq!(detail["summary"]["mergeable"]["can_merge"], false);
    assert_eq!(
        detail["summary"]["mergeable"]["reason"],
        "contributed pull request without a maintainer approval on this head",
    );
    assert_eq!(detail["summary"]["labels"][0], "origin:contributed");
    assert_eq!(detail["summary"]["review"]["approvals"], 1);

    // The merge route refuses the held head with the same blocker.
    let refused = crate::web::pulls::merge(
        State(state.clone()),
        axum::Extension(crate::web::auth::trusted_local_account(&state)),
        path(),
        axum::body::Bytes::from(
            serde_json::json!({
                "expected_head_sha": head_sha.clone(),
                "expected_passport_hash": detail["passport_hash"].as_str().unwrap(),
                "merge_method": "merge",
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let refused = response_json(refused).await;
    assert_eq!(refused["code"], "merge_blocked");
    assert_ne!(
        bare_ref(storage.path(), "acme", "widget", "refs/heads/main"),
        head_sha,
        "a held contribution must not have landed",
    );

    // The maintainer approves the exact head. Nothing about the pull request
    // changed except who signed off, and that is what the gate was waiting on.
    approve_as(&core, "widget", number, "maintainer", &head_sha);
    let detail = response_json(
        crate::web::pulls::detail(
            State(state.clone()),
            authenticated_account("maintainer"),
            path(),
        )
        .await,
    )
    .await;
    assert_eq!(detail["merge_passport"]["status"], "pass");
    assert!(blocker_codes(&detail).is_empty(), "{detail:?}");

    let merged = crate::web::pulls::merge(
        State(state.clone()),
        axum::Extension(crate::web::auth::trusted_local_account(&state)),
        path(),
        axum::body::Bytes::from(
            serde_json::json!({
                "expected_head_sha": head_sha.clone(),
                "expected_passport_hash": detail["passport_hash"].as_str().unwrap(),
                "merge_method": "merge",
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(merged.status(), StatusCode::OK);
    assert_eq!(
        bare_ref(storage.path(), "acme", "widget", "refs/heads/main"),
        head_sha,
        "the maintainer-approved contribution lands on main",
    );
}

/// An internal pull request is unaffected: the same approval that cannot land
/// a contribution lands it, and its label says where it came from.
#[tokio::test]
async fn an_internal_pull_request_is_unaffected_by_the_contributed_gate() {
    let storage = tempfile::tempdir().expect("git storage dir");
    let (_core, repo, number, head_sha) = forge_with_approved_pull(storage.path(), "globex");
    let state = Arc::new(
        WebState::new_with_git_storage(_core, storage.path().to_path_buf())
            .with_pull_trust(&["importbot"], &["maintainer"]),
    );
    let path = || AxumPath((repo.id.to_string(), number));

    // Marking it internal is the same route; it just does not hold anything.
    let marked = response_json(
        mark_origin(
            &state,
            &repo.id.to_string(),
            number,
            authenticated_account("importbot"),
            "internal",
        )
        .await,
    )
    .await;
    assert_eq!(marked["origin"], "internal");
    assert_eq!(marked["merge_hold"], Value::Null);

    let detail = response_json(
        crate::web::pulls::detail(State(state.clone()), authenticated_account("bob"), path()).await,
    )
    .await;
    assert_eq!(detail["merge_passport"]["status"], "pass");
    assert_eq!(detail["summary"]["labels"][0], "origin:internal");
    assert!(blocker_codes(&detail).is_empty(), "{detail:?}");

    let merged = crate::web::pulls::merge(
        State(state.clone()),
        axum::Extension(crate::web::auth::trusted_local_account(&state)),
        path(),
        axum::body::Bytes::from(
            serde_json::json!({
                "expected_head_sha": head_sha.clone(),
                "expected_passport_hash": detail["passport_hash"].as_str().unwrap(),
                "merge_method": "merge",
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(merged.status(), StatusCode::OK);
    assert_eq!(
        bare_ref(storage.path(), "acme", "globex", "refs/heads/main"),
        head_sha,
    );
}

/// Who may write the marking: an importer or a global admin, and never the
/// pull request's own author — a contribution cannot declare itself trusted.
#[tokio::test]
async fn only_an_importer_marks_an_origin_and_never_the_author() {
    let storage = tempfile::tempdir().expect("git storage dir");
    let (_core, repo, number, _head) = forge_with_approved_pull(storage.path(), "initech");
    let state = Arc::new(
        WebState::new_with_git_storage(_core, storage.path().to_path_buf())
            .with_pull_trust(&["importbot"], &["maintainer"]),
    );
    let id = repo.id.to_string();

    let refused = mark_origin(
        &state,
        &id,
        number,
        authenticated_account("bob"),
        "internal",
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response_json(refused).await["code"],
        "pull_origin_importer_required",
    );

    // The author is `outsider`. Even as a global admin they may not mark their
    // own pull request.
    let refused = mark_origin(
        &state,
        &id,
        number,
        authenticated_admin_account("outsider"),
        "internal",
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response_json(refused).await["code"],
        "pull_origin_author_forbidden",
    );

    let refused = mark_origin(
        &state,
        &id,
        number,
        authenticated_account("importbot"),
        "trusted",
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response_json(refused).await["code"],
        "pull_origin_invalid_request",
    );

    // A global admin may mark it, and the read route reports the marking and
    // the maintainer list a reader of the hold needs.
    let marked = mark_origin(
        &state,
        &id,
        number,
        authenticated_admin_account("root"),
        "contributed",
    )
    .await;
    assert_eq!(marked.status(), StatusCode::OK);
    let shown = response_json(
        crate::web::pull_origin::show(State(state.clone()), AxumPath((id.clone(), number))).await,
    )
    .await;
    assert_eq!(shown["origin"], "contributed");
    assert_eq!(shown["set_by"], "root");
    assert_eq!(shown["maintainers"][0], "maintainer");
    assert!(
        shown["merge_hold"].as_str().unwrap().contains("maintainer"),
        "{shown:?}",
    );
}

/// With no maintainer list configured nothing contributed merges: the gate
/// fails closed and says the list is missing.
#[tokio::test]
async fn without_a_maintainer_list_no_contribution_merges() {
    let storage = tempfile::tempdir().expect("git storage dir");
    let (_core, repo, number, _head) = forge_with_approved_pull(storage.path(), "stark");
    let state = Arc::new(
        WebState::new_with_git_storage(_core, storage.path().to_path_buf())
            .with_pull_trust(&["importbot"], &[]),
    );
    let id = repo.id.to_string();
    assert_eq!(
        mark_origin(
            &state,
            &id,
            number,
            authenticated_account("importbot"),
            "contributed",
        )
        .await
        .status(),
        StatusCode::OK,
    );
    let detail = response_json(
        crate::web::pulls::detail(
            State(state.clone()),
            authenticated_account("bob"),
            AxumPath((id, number)),
        )
        .await,
    )
    .await;
    assert_eq!(detail["merge_passport"]["status"], "blocked");
    let blockers = detail["merge_passport"]["blockers"].as_array().unwrap();
    assert_eq!(blockers[0]["code"], "passport_blocked_contributed_origin");
    assert!(
        blockers[0]["details"]
            .as_str()
            .unwrap()
            .contains("JERYU_PULL_MAINTAINERS"),
        "{blockers:?}",
    );
}
