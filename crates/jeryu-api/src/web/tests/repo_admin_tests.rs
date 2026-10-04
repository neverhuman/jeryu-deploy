use super::*;

/// PATCH /api/v1/repos/:id applies only the keys present in the body:
/// a string sets the family, an explicit null clears it, junk is rejected,
/// and the families facet reflects the live values.
#[tokio::test]
async fn repo_update_sets_and_clears_family() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "jeryu",
            CreateRepositoryRequest {
                name: "veox-nht".to_string(),
                private: true,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    let state = Arc::new(WebState::new(core));
    let id = repo.id.to_string();

    let updated = response_json(
        repo_update(
            State(state.clone()),
            authenticated_admin_account("alice"),
            AxumPath(id.clone()),
            axum::body::Bytes::from_static(br#"{"family": "veox-split"}"#),
        )
        .await,
    )
    .await;
    // Either spelling assigns the one canonical key.
    assert_eq!(updated["family"], "veox");
    let list = repo_list_response(&state);
    assert_eq!(list.facets.families, vec!["veox".to_string()]);

    let cleared = response_json(
        repo_update(
            State(state.clone()),
            authenticated_admin_account("alice"),
            AxumPath("jeryu/veox-nht".to_string()),
            axum::body::Bytes::from_static(br#"{"family": null}"#),
        )
        .await,
    )
    .await;
    assert_eq!(cleared["family"], serde_json::Value::Null);
    assert!(repo_list_response(&state).facets.families.is_empty());

    // Unknown fields, non-string family, and blank family are 422s.
    for body in [
        br#"{"description": "nope"}"#.as_slice(),
        br#"{"family": 7}"#.as_slice(),
        br#"{"family": "  "}"#.as_slice(),
    ] {
        let response = repo_update(
            State(state.clone()),
            authenticated_admin_account("alice"),
            AxumPath(id.clone()),
            axum::body::Bytes::copy_from_slice(body),
        )
        .await;
        assert_eq!(
            response.into_response().status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    let missing = repo_update(
        State(state),
        authenticated_admin_account("alice"),
        AxumPath("jeryu/missing".to_string()),
        axum::body::Bytes::from_static(br#"{"family": "x"}"#),
    )
    .await;
    assert_eq!(
        missing.into_response().status(),
        axum::http::StatusCode::NOT_FOUND
    );
}

/// PATCH /api/v1/repos/:id moves the default branch: an admin may point it at
/// any branch git actually has, the new default is protected like the old one
/// was, a non-admin writer may not, and an unknown branch is a 422.
#[tokio::test]
async fn repo_update_moves_the_default_branch_for_admins_only() {
    let dir = tempfile::tempdir().unwrap();
    crate::web::shift::tests::fixture(dir.path());
    let core = ForgeCore::new();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "jeryu-deploy".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new_with_git_storage(
        core.clone(),
        dir.path().to_path_buf(),
    ));

    let moved = response_json(
        repo_update(
            State(state.clone()),
            authenticated_admin_account("alice"),
            AxumPath("jeryu/jeryu-deploy".to_string()),
            axum::body::Bytes::from_static(br#"{"default_branch": "nightshift/2026-09-18"}"#),
        )
        .await,
    )
    .await;
    assert_eq!(moved["default_branch"], "nightshift/2026-09-18");
    assert_eq!(
        core.get_repository("jeryu", "jeryu-deploy")
            .unwrap()
            .default_branch,
        "nightshift/2026-09-18"
    );
    assert!(
        core.get_branch_protection("jeryu", "jeryu-deploy", "nightshift/2026-09-18")
            .is_ok(),
        "the new default branch is protected like the old one"
    );

    // Repository write access is not enough.
    let refused = repo_update(
        State(state.clone()),
        authenticated_account("bob"),
        AxumPath("jeryu/jeryu-deploy".to_string()),
        axum::body::Bytes::from_static(br#"{"default_branch": "main"}"#),
    )
    .await;
    assert_eq!(
        refused.into_response().status(),
        axum::http::StatusCode::FORBIDDEN
    );
    assert_eq!(
        core.get_repository("jeryu", "jeryu-deploy")
            .unwrap()
            .default_branch,
        "nightshift/2026-09-18"
    );

    // A branch git does not have, and a non-string, are both 422s.
    for body in [
        br#"{"default_branch": "no-such-branch"}"#.as_slice(),
        br#"{"default_branch": 7}"#.as_slice(),
    ] {
        let response = repo_update(
            State(state.clone()),
            authenticated_admin_account("alice"),
            AxumPath("jeryu/jeryu-deploy".to_string()),
            axum::body::Bytes::copy_from_slice(body),
        )
        .await;
        assert_eq!(
            response.into_response().status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}

/// PATCH /api/v1/repos/:id archives and unarchives: an admin may flip the
/// flag either way, a repository writer may not, the change is reversible and
/// deletes nothing, a non-boolean is a 422 and an unknown repository is a 404.
#[tokio::test]
async fn repo_update_archives_and_unarchives_for_admins_only() {
    let core = ForgeCore::new();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "jeryu-deploy".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new(core.clone()));

    assert!(
        !core
            .get_repository("jeryu", "jeryu-deploy")
            .unwrap()
            .archived,
        "a new repository is not archived"
    );

    // An admin archives it.
    let archived = repo_update(
        State(state.clone()),
        authenticated_admin_account("alice"),
        AxumPath("jeryu/jeryu-deploy".to_string()),
        axum::body::Bytes::from_static(br#"{"archived": true}"#),
    )
    .await;
    assert_eq!(
        archived.into_response().status(),
        axum::http::StatusCode::OK
    );
    assert!(
        core.get_repository("jeryu", "jeryu-deploy")
            .unwrap()
            .archived
    );

    // Repository write access is not enough to unarchive it.
    let refused = repo_update(
        State(state.clone()),
        authenticated_account("bob"),
        AxumPath("jeryu/jeryu-deploy".to_string()),
        axum::body::Bytes::from_static(br#"{"archived": false}"#),
    )
    .await;
    assert_eq!(
        refused.into_response().status(),
        axum::http::StatusCode::FORBIDDEN
    );
    assert!(
        core.get_repository("jeryu", "jeryu-deploy")
            .unwrap()
            .archived,
        "the refused request changed nothing"
    );

    // Reversible: the admin unarchives it and the repository is back.
    let unarchived = repo_update(
        State(state.clone()),
        authenticated_admin_account("alice"),
        AxumPath("jeryu/jeryu-deploy".to_string()),
        axum::body::Bytes::from_static(br#"{"archived": false}"#),
    )
    .await;
    assert_eq!(
        unarchived.into_response().status(),
        axum::http::StatusCode::OK
    );
    assert!(
        !core
            .get_repository("jeryu", "jeryu-deploy")
            .unwrap()
            .archived
    );
    assert!(
        core.get_repository("jeryu", "jeryu-deploy").is_ok(),
        "unarchiving deletes nothing"
    );

    // A non-boolean is about the body: 422.
    let bad = repo_update(
        State(state.clone()),
        authenticated_admin_account("alice"),
        AxumPath("jeryu/jeryu-deploy".to_string()),
        axum::body::Bytes::from_static(br#"{"archived": "yes"}"#),
    )
    .await;
    assert_eq!(
        bad.into_response().status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );

    // An unknown repository is a 404, not a 403, even for an admin.
    let missing = repo_update(
        State(state.clone()),
        authenticated_admin_account("alice"),
        AxumPath("jeryu/missing".to_string()),
        axum::body::Bytes::from_static(br#"{"archived": true}"#),
    )
    .await;
    assert_eq!(
        missing.into_response().status(),
        axum::http::StatusCode::NOT_FOUND
    );
}

/// DELETE /api/v1/repos/:id — an unknown repository is a structured 404.
#[tokio::test]
async fn repo_delete_unknown_repo_is_404() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let response = repo_admin::repo_delete(
        State(state),
        AxumPath("jeryu/missing".to_string()),
        axum::body::Bytes::from_static(br#"{"confirm_full_name": "jeryu/missing"}"#),
    )
    .await;
    assert_eq!(
        response.into_response().status(),
        axum::http::StatusCode::NOT_FOUND
    );
}

/// The confirmation must byte-match the repository's full name; anything else
/// (case drift, malformed body) is a 422 and the repository stays registered.
#[tokio::test]
async fn repo_delete_requires_byte_exact_confirmation() {
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
    let state = Arc::new(WebState::new(core));
    for body in [
        br#"{"confirm_full_name": "alice/Jeryu"}"#.as_slice(),
        br#"{"confirm_full_name": "alice/*"}"#.as_slice(),
        br#"{"confirm_full_name": ""}"#.as_slice(),
        br#"not json"#.as_slice(),
    ] {
        let response = repo_admin::repo_delete(
            State(state.clone()),
            AxumPath(repo.id.to_string()),
            axum::body::Bytes::copy_from_slice(body),
        )
        .await;
        assert_eq!(
            response.into_response().status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(repo_list_response(&state).repositories.len(), 1);
}

/// Happy-path registry deletion: a 200 receipt with per-collection counts and
/// an audit id, and the repository disappears from the list response. With
/// `delete_storage` unset nothing on disk is touched.
#[tokio::test]
async fn repo_delete_registry_returns_receipt_and_unlists_repo() {
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
    core.create_label(
        "alice",
        "jeryu",
        jeryu_core::CreateLabelRequest {
            name: "bug".to_string(),
            color: "ff0000".to_string(),
            description: None,
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new(core));

    let response = repo_admin::repo_delete(
        State(state.clone()),
        AxumPath(repo.id.to_string()),
        axum::body::Bytes::from_static(br#"{"confirm_full_name": "alice/jeryu"}"#),
    )
    .await
    .into_response();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let receipt = response_json(response).await;
    assert_eq!(receipt["registry_deleted"], true);
    assert_eq!(receipt["storage_deleted"], false);
    assert_eq!(receipt["storage_path"], Value::Null);
    assert_eq!(receipt["repo"]["owner"], "alice");
    assert_eq!(receipt["repo"]["name"], "jeryu");
    assert!(
        !receipt["audit_id"].as_str().unwrap_or_default().is_empty(),
        "the receipt must carry the audit entry id"
    );
    let counts = receipt["deleted_counts"].as_array().expect("counts array");
    let removed = |collection: &str| {
        counts
            .iter()
            .find(|count| count["collection"] == collection)
            .map(|count| count["removed"].as_u64().unwrap_or_default())
            .unwrap_or_else(|| panic!("missing collection {collection}"))
    };
    assert_eq!(removed("labels"), 1);
    assert_eq!(removed("branch_protections"), 1);
    assert_eq!(removed("counters"), 1);
    assert_eq!(removed("pulls"), 0);

    assert!(
        repo_list_response(&state).repositories.is_empty(),
        "the deleted repository must vanish from the list"
    );
}

/// `delete_storage: true` against a real managed bare repository removes the
/// directory and reports its path in the receipt.
#[tokio::test]
async fn repo_delete_storage_removes_managed_bare_dir() {
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
    let storage = tempdir().expect("git storage dir");
    let state = Arc::new(WebState::new_with_git_storage(
        core,
        storage.path().to_path_buf(),
    ));
    let bare = state
        .repo_manager
        .create_bare(&jeryu_gitd::RepoId::new("alice", "jeryu").expect("repo id"))
        .expect("create bare repo");
    assert!(bare.path.join("HEAD").is_file());

    let response = repo_admin::repo_delete(
        State(state.clone()),
        AxumPath("alice/jeryu".to_string()),
        axum::body::Bytes::from_static(
            br#"{"confirm_full_name": "alice/jeryu", "delete_storage": true}"#,
        ),
    )
    .await
    .into_response();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let receipt = response_json(response).await;
    assert_eq!(receipt["registry_deleted"], true);
    assert_eq!(receipt["storage_deleted"], true);
    assert!(
        !receipt["storage_path"]
            .as_str()
            .unwrap_or_default()
            .is_empty()
    );
    assert!(!bare.path.exists(), "the bare dir must be removed");
    assert!(repo_list_response(&state).repositories.is_empty());
}

/// A symlinked `name.git` under the storage root is refused with a 422 and
/// the symlink target stays untouched (registry tier already committed).
#[cfg(unix)]
#[tokio::test]
async fn repo_delete_storage_refuses_symlinked_bare_dir() {
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
    let storage = tempdir().expect("git storage dir");
    let victim_root = tempdir().expect("victim dir");
    let victim = victim_root.path().join("victim.git");
    std::fs::create_dir_all(victim.join("objects")).expect("victim objects");
    std::fs::create_dir_all(victim.join("refs")).expect("victim refs");
    std::fs::write(victim.join("HEAD"), "ref: refs/heads/main\n").expect("victim HEAD");
    std::fs::create_dir_all(storage.path().join("alice")).expect("owner dir");
    std::os::unix::fs::symlink(&victim, storage.path().join("alice").join("jeryu.git"))
        .expect("symlink bare dir");
    let state = Arc::new(WebState::new_with_git_storage(
        core,
        storage.path().to_path_buf(),
    ));

    let response = repo_admin::repo_delete(
        State(state),
        AxumPath("alice/jeryu".to_string()),
        axum::body::Bytes::from_static(
            br#"{"confirm_full_name": "alice/jeryu", "delete_storage": true}"#,
        ),
    )
    .await
    .into_response();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert!(
        victim.join("HEAD").is_file(),
        "the symlink target must be untouched"
    );
}

/// Live work blocks deletion: a running repo-scoped agent run yields a 409
/// and the repository stays registered.
#[tokio::test]
async fn repo_delete_conflicts_with_live_agent_run() {
    let core = ForgeCore::new();
    // seed_test_run pins its owning repo to "owner/repo".
    core.create_repository(
        "owner",
        CreateRepositoryRequest {
            name: "repo".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new(core));
    state.agent_runs.seed_test_run("run-live-409", 4);

    let response = repo_admin::repo_delete(
        State(state.clone()),
        AxumPath("owner/repo".to_string()),
        axum::body::Bytes::from_static(br#"{"confirm_full_name": "owner/repo"}"#),
    )
    .await;
    assert_eq!(
        response.into_response().status(),
        axum::http::StatusCode::CONFLICT
    );
    assert_eq!(repo_list_response(&state).repositories.len(), 1);
}

/// Negative authorization / data-isolation proof for the DELETE surface:
/// a confirmation naming ANOTHER owner's repository never deletes anything
/// (the confirm is bound to the addressed resource, so a non-owner name is
/// refused), and deleting one owner's repository leaves the other owner's
/// same-named repository and its data fully intact.
#[tokio::test]
async fn repo_delete_cannot_cross_owner_boundaries() {
    let core = ForgeCore::new();
    let alice = core
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
    // Bob owns a repository with the SAME name: the sharpest isolation probe
    // for the (owner, name)-keyed state maps.
    let bob = core
        .create_repository(
            "bob",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    core.create_label(
        "bob",
        "jeryu",
        jeryu_core::CreateLabelRequest {
            name: "keep".to_string(),
            color: "00ff00".to_string(),
            description: None,
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new(core));

    // Non-owner confirmation: addressing bob's repo while confirming alice's
    // full name (and vice versa) is refused and deletes nothing.
    for (target, wrong_confirm) in [
        (
            bob.id.to_string(),
            br#"{"confirm_full_name": "alice/jeryu"}"#.as_slice(),
        ),
        (
            alice.id.to_string(),
            br#"{"confirm_full_name": "bob/jeryu"}"#.as_slice(),
        ),
    ] {
        let response = repo_admin::repo_delete(
            State(state.clone()),
            AxumPath(target),
            axum::body::Bytes::from_static(wrong_confirm),
        )
        .await;
        assert_eq!(
            response.into_response().status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "a non-owner confirmation must never authorize a delete"
        );
    }
    assert_eq!(repo_list_response(&state).repositories.len(), 2);

    // Deleting alice's repo must not touch bob's same-named repo or its data.
    let response = repo_admin::repo_delete(
        State(state.clone()),
        AxumPath(alice.id.to_string()),
        axum::body::Bytes::from_static(br#"{"confirm_full_name": "alice/jeryu"}"#),
    )
    .await
    .into_response();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let remaining = repo_list_response(&state);
    assert_eq!(remaining.repositories.len(), 1);
    assert_eq!(remaining.repositories[0].id.owner, "bob");
    assert_eq!(remaining.repositories[0].id.name, "jeryu");
    let bob_labels = state.github.core().list_labels("bob", "jeryu").unwrap();
    assert_eq!(
        bob_labels.len(),
        1,
        "bob's data must survive alice's delete"
    );
    assert_eq!(bob_labels[0].name, "keep");
}
