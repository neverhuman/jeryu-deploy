//! The contributed-origin gate: a pull request the importer brought in from a
//! mirror merges only on a configured maintainer's approval, and a pull
//! request written inside the forge is untouched by the rule.

use super::*;

#[tokio::test]
async fn both_rest_merge_edges_require_a_maintainer() {
    let storage = tempfile::tempdir().unwrap();
    let (core, _, number, head) = forge_with_approved_pull(storage.path(), "edge-hold");
    let state = Arc::new(
        WebState::new_with_git_storage(core.clone(), storage.path().to_path_buf())
            .with_pull_trust(&["importbot"], &["maintainer"]),
    );
    state
        .pull_origin
        .set(
            "acme",
            "edge-hold",
            number,
            crate::web::pull_origin::PullOrigin::Contributed,
            "importbot",
            chrono::Utc::now().timestamp_millis(),
        )
        .unwrap();
    say_ok_to_test(&state, "edge-hold", number);
    let base = bare_ref(storage.path(), "acme", "edge-hold", "refs/heads/main");
    for prefix in ["", "/api/v3"] {
        let uri = format!("{prefix}/repos/acme/edge-hold/pulls/{number}/merge")
            .parse()
            .unwrap();
        let response = crate::web::surface::github_forward(
            State(state.clone()),
            None,
            axum::http::Method::PUT,
            axum::http::HeaderMap::new(),
            axum::extract::OriginalUri(uri),
            axum::body::Bytes::from("{}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            bare_ref(storage.path(), "acme", "edge-hold", "refs/heads/main"),
            base
        );
    }
    approve_as(&core, "edge-hold", number, "maintainer", &head);
    let response = crate::web::surface::github_forward(
        State(state),
        None,
        axum::http::Method::PUT,
        axum::http::HeaderMap::new(),
        axum::extract::OriginalUri(
            format!("/api/v3/repos/acme/edge-hold/pulls/{number}/merge")
                .parse()
                .unwrap(),
        ),
        axum::body::Bytes::from("{}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        bare_ref(storage.path(), "acme", "edge-hold", "refs/heads/main"),
        head
    );
}

#[test]
fn an_unmarked_import_namespace_still_holds_the_pull() {
    let storage = tempfile::tempdir().unwrap();
    let (core, _, number, _) = forge_with_approved_pull(storage.path(), "unmarked");
    let state = WebState::new_with_git_storage(core.clone(), storage.path().to_path_buf());
    let mut pr = core.get_pull_request("acme", "unmarked", number).unwrap();
    pr.head.ref_name = "contrib/gh-43".to_string();
    let posture = crate::web::pull_origin::posture(&state, &pr);
    assert!(posture.contributed());
    assert!(posture.blocks_merge());
    assert!(
        state
            .pull_origin
            .get("acme", "unmarked", number)
            .unwrap()
            .is_none()
    );
}

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

/// A maintainer says ok-to-test for the pull request's current head.
fn say_ok_to_test(state: &WebState, name: &str, number: u64) {
    let pr = state
        .github
        .core()
        .get_pull_request("acme", name, number)
        .unwrap();
    state.ok_to_test.record(&pr, "maintainer");
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
    say_ok_to_test(&state, "widget", number);

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
    let approval = blockers
        .iter()
        .find(|blocker| blocker["code"] == "passport_blocked_contributed_origin")
        .expect("the approval hold is listed");
    assert!(
        approval["details"]
            .as_str()
            .unwrap()
            .contains("JERYU_PULL_MAINTAINERS"),
        "{blockers:?}",
    );
}

async fn post_ok_to_test(
    state: &Arc<WebState>,
    id: &str,
    number: u64,
    login: &str,
    body: &str,
) -> AxumResponse {
    crate::web::pull_contrib::ok_to_test(
        State(state.clone()),
        authenticated_account(login),
        AxumPath((id.to_string(), number)),
        axum::body::Bytes::from(body.to_string()),
    )
    .await
}

/// The gate waits for a maintainer: only a login on the list can say
/// ok-to-test, the word is bound to the exact head, and until it is said the
/// forge neither signals the runner nor honours a re-gate.
#[tokio::test]
async fn only_a_maintainers_ok_to_test_starts_a_contributed_gate() {
    let storage = tempfile::tempdir().expect("git storage dir");
    let (core, repo, number, head) = forge_with_approved_pull(storage.path(), "gated");
    let state = Arc::new(
        WebState::new_with_git_storage(core.clone(), storage.path().to_path_buf())
            .with_pull_trust(&["importbot"], &["maintainer"]),
    );
    let id = repo.id.to_string();
    assert_eq!(
        mark_origin(
            &state,
            &id,
            number,
            authenticated_account("importbot"),
            "contributed"
        )
        .await
        .status(),
        StatusCode::OK
    );
    let pr = core.get_pull_request("acme", "gated", number).unwrap();
    assert!(!crate::web::pull_contrib::gate_may_start(&state, &pr));
    let detail = response_json(
        crate::web::pulls::detail(
            State(state.clone()),
            authenticated_account("bob"),
            AxumPath((id.clone(), number)),
        )
        .await,
    )
    .await;
    assert!(
        blocker_codes(&detail).contains(&"passport_blocked_contributed_not_ok_to_test".to_string()),
        "{detail:?}"
    );

    // A re-gate is refused while nobody has said ok-to-test.
    let regate = crate::web::regate::request(
        State(state.clone()),
        authenticated_admin_account("root"),
        AxumPath((id.clone(), number)),
    )
    .await;
    assert_eq!(regate.status(), StatusCode::CONFLICT);

    // The author, an admin and an ordinary reviewer are not maintainers.
    for login in ["outsider", "bob", "root"] {
        let refused = post_ok_to_test(&state, &id, number, login, "").await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{login}");
        assert_eq!(
            response_json(refused).await["code"],
            "pull_maintainer_required"
        );
    }
    assert!(!crate::web::pull_contrib::gate_may_start(&state, &pr));

    // A head the maintainer did not look at is refused.
    let moved = post_ok_to_test(
        &state,
        &id,
        number,
        "maintainer",
        &serde_json::json!({ "head_sha": "0".repeat(40) }).to_string(),
    )
    .await;
    assert_eq!(moved.status(), StatusCode::CONFLICT);
    assert_eq!(response_json(moved).await["code"], "head_moved");

    let seq = state.gate_wanted.seq();
    let allowed = post_ok_to_test(
        &state,
        &id,
        number,
        "maintainer",
        &serde_json::json!({ "head_sha": head }).to_string(),
    )
    .await;
    assert_eq!(allowed.status(), StatusCode::OK);
    let allowed = response_json(allowed).await;
    assert_eq!(allowed["ok_to_test_by"], "maintainer");
    assert_eq!(allowed["dco"], "pass");
    assert!(crate::web::pull_contrib::gate_may_start(&state, &pr));
    let (_, signals) = state.gate_wanted.since(seq);
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].source, "ok_to_test");
    assert_eq!(signals[0].sha, head);

    // A new head is a new question: the earlier word does not carry over.
    let mut moved_pr = pr.clone();
    moved_pr.head.sha = "1".repeat(40);
    assert!(!crate::web::pull_contrib::gate_may_start(&state, &moved_pr));

    // An internal pull request never waits.
    let mut internal = pr.clone();
    internal.head.ref_name = "feature".to_string();
    state
        .pull_origin
        .set(
            "acme",
            "gated",
            number,
            crate::web::pull_origin::PullOrigin::Internal,
            "root",
            1,
        )
        .unwrap();
    let denied = post_ok_to_test(&state, &id, number, "maintainer", "").await;
    assert_eq!(denied.status(), StatusCode::CONFLICT);
    assert!(crate::web::pull_contrib::gate_may_start(&state, &internal));
}

/// A push to a contribution branch does not wake the gate runner.
#[test]
fn a_push_to_a_contribution_branch_sends_no_gate_signal() {
    let storage = tempfile::tempdir().expect("git storage dir");
    let state = WebState::new_with_git_storage(ForgeCore::new(), storage.path().to_path_buf());
    let update = |branch: &str| crate::ci_bridge::RefUpdate {
        ref_name: format!("refs/heads/{branch}"),
        old_oid: crate::ci_bridge::ZERO_OID.to_string(),
        new_oid: "a".repeat(40),
    };
    crate::web::gate_wanted::record_push(
        &state,
        "acme",
        "gated.git",
        &[update("contrib/gh-7"), update("topic")],
    );
    let (_, signals) = state.gate_wanted.since(0);
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].ref_name.as_deref(), Some("refs/heads/topic"));
}

/// A contribution whose commit has no matching sign-off is held with a message
/// that names the commit and the fix; a matching sign-off clears it.
#[tokio::test]
async fn an_unsigned_contribution_fails_the_dco_check() {
    let storage = tempfile::tempdir().expect("git storage dir");
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "acme",
            CreateRepositoryRequest {
                name: "unsigned".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    let (base_sha, _) = build_bare_repo_with_main_and_feature(
        storage.path(),
        "acme",
        "unsigned",
        "src/a.rs",
        "pub fn a() {}\n",
    );
    // Two more commits on the bare repository's feature branch: one signed by
    // somebody other than its author, one signed by its author.
    let bare = storage.path().join("acme").join("unsigned.git");
    let work = storage.path().join("dco-work");
    git_in(
        storage.path(),
        &[
            "clone",
            "--quiet",
            bare.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    git_in(&work, &["checkout", "--quiet", "feature"]);
    write_file(&work, "src/b.rs", "pub fn b() {}\n");
    git_in(&work, &["add", "src/b.rs"]);
    git_in(
        &work,
        &[
            "commit",
            "--quiet",
            "-m",
            "add b\n\nSigned-off-by: Someone Else <else@example.test>",
        ],
    );
    let unsigned_head = git_in(&work, &["rev-parse", "HEAD"]);
    git_in(&work, &["push", "--quiet", "origin", "feature"]);
    let pr = core
        .create_pull_request(
            "acme",
            "unsigned",
            "outsider",
            CreatePullRequestRequest {
                title: "add b".to_string(),
                head: "contrib/gh-9".to_string(),
                base: "main".to_string(),
                head_sha: Some(unsigned_head.clone()),
                base_sha: Some(base_sha.clone()),
                changed_files: vec!["src/b.rs".to_string()],
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    let state = Arc::new(
        WebState::new_with_git_storage(core.clone(), storage.path().to_path_buf())
            .with_pull_trust(&["importbot"], &["maintainer"]),
    );
    let id = repo.id.to_string();
    let allowed =
        response_json(post_ok_to_test(&state, &id, pr.number, "maintainer", "").await).await;
    assert_eq!(allowed["dco"], "fail");
    let message = allowed["dco_detail"].as_str().unwrap();
    assert!(message.contains(&unsigned_head[..12]), "{message}");
    assert!(message.contains("git commit -s"), "{message}");

    let detail = response_json(
        crate::web::pulls::detail(
            State(state.clone()),
            authenticated_account("bob"),
            AxumPath((id.clone(), pr.number)),
        )
        .await,
    )
    .await;
    let dco = detail["merge_passport"]["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|blocker| blocker["code"] == "passport_blocked_contributed_dco")
        .expect("the DCO blocker is listed");
    assert!(
        dco["message"]
            .as_str()
            .unwrap()
            .contains("DCO sign-off missing")
    );

    // The mismatched sign-off is amended to the author's own: the check passes.
    git_in(
        &work,
        &["commit", "--quiet", "--amend", "-s", "-m", "add b"],
    );
    let signed_head = git_in(&work, &["rev-parse", "HEAD"]);
    git_in(&work, &["push", "--quiet", "--force", "origin", "feature"]);
    let mut signed = core
        .get_pull_request("acme", "unsigned", pr.number)
        .unwrap();
    signed.head.sha = signed_head;
    assert_eq!(
        crate::web::pull_contrib::dco_verdict(&state, &signed),
        crate::web::pull_contrib::DcoVerdict::Pass
    );
}

/// With no maintainers the importer does nothing at all.
#[test]
fn the_importer_is_off_without_maintainers() {
    let storage = tempfile::tempdir().expect("git storage dir");
    let state = WebState::new_with_git_storage(ForgeCore::new(), storage.path().to_path_buf())
        .with_pull_trust(&["importbot"], &[]);
    assert!(crate::web::pull_import::import_once(&state).is_empty());
}
