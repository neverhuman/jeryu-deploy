use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt;

fn repository(core: &ForgeCore) {
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "branches".into(),
            private: false,
            description: None,
            default_branch: Some("main".into()),
        },
    )
    .unwrap();
}

fn protect(core: &ForgeCore, branch: &str) {
    core.set_branch_protection(
        "alice", "branches", branch,
        serde_json::from_str(r#"{"required_status_checks":[],"required_approving_review_count":1,"enforce_admins":true}"#).unwrap(),
    ).unwrap();
}

#[tokio::test]
async fn branch_protection_delete_is_admin_only_and_decodes_branch() {
    let data = tempfile::tempdir().unwrap();
    let core = ForgeCore::open_sqlite(data.path().join("forge.sqlite")).unwrap();
    repository(&core);
    protect(&core, "release/x");
    for (login, role) in [("admin", UserRole::Admin), ("writer", UserRole::User)] {
        core.create_account(login, "test-password", role).unwrap();
    }
    core.grant_repo_access(
        "admin",
        "writer",
        "alice",
        "branches",
        jeryu_core::RepoAccessLevel::Write,
    )
    .unwrap();
    let app = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        std::path::Path::new("/nonexistent-spa"),
    );
    for (login, status) in [
        ("writer", StatusCode::FORBIDDEN),
        ("admin", StatusCode::NO_CONTENT),
        ("admin", StatusCode::NOT_FOUND),
    ] {
        let token = core
            .create_personal_access_token(login, "test", None)
            .unwrap()
            .secret;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v3/repos/alice/branches/branches/release%2Fx/protection")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(
            core.get_branch_protection("alice", "branches", "release/x")
                .is_ok(),
            login == "writer"
        );
    }
    let audit = core.list_audit("alice/branches").unwrap();
    assert!(
        audit
            .iter()
            .any(|entry| entry.action == "branch_protection.deleted" && entry.actor == "admin")
    );
    assert!(
        core.get_branch_protection("alice", "branches", "main")
            .is_ok()
    );
}

#[tokio::test]
async fn branch_protection_startup_rekeys_once_and_restart_is_idempotent() {
    let data = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let db = data.path().join("forge.sqlite");
    let core = ForgeCore::open_sqlite(&db).unwrap();
    repository(&core);
    protect(&core, "release%2Fx");
    let state = WebState::new_with_git_storage(core.clone(), storage.path().to_path_buf());
    let bare = state
        .repo_manager
        .create_bare(&jeryu_gitd::RepoId::new("alice", "branches").unwrap())
        .unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&bare.path)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    let tree = git(&["mktree"]);
    let commit = git(&["commit-tree", &tree, "-m", "initial"]);
    git(&["update-ref", "refs/heads/release/x", &commit]);
    super::super::bootstrap::bootstrap_public_accounts(&state, data.path()).unwrap();
    assert!(
        core.get_branch_protection("alice", "branches", "release%2Fx")
            .is_err()
    );
    assert!(
        core.get_branch_protection("alice", "branches", "release/x")
            .is_ok()
    );
    let audit_count = || {
        core.list_audit("alice/branches")
            .unwrap()
            .iter()
            .filter(|entry| entry.action == "branch_protection.encoded_rekey")
            .count()
    };
    assert_eq!(audit_count(), 1);
    // A request does not rerun startup repair, even if another encoded rule appears.
    protect(&core, "release%2Fx");
    let app = app(state.clone(), std::path::Path::new("/nonexistent-spa"));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v3/repos/alice/branches/branches/release%2Fx/protection")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        core.get_branch_protection("alice", "branches", "release%2Fx")
            .is_ok()
    );
    super::super::bootstrap::bootstrap_public_accounts(&state, data.path()).unwrap();
    assert!(
        core.get_branch_protection("alice", "branches", "release%2Fx")
            .is_err()
    );
    assert_eq!(audit_count(), 2);
    drop(state);
    drop(core);
    let reopened = ForgeCore::open_sqlite(&db).unwrap();
    let state = WebState::new_with_git_storage(reopened.clone(), storage.path().to_path_buf());
    super::super::bootstrap::bootstrap_public_accounts(&state, data.path()).unwrap();
    assert!(
        reopened
            .get_branch_protection("alice", "branches", "release/x")
            .is_ok()
    );
    assert_eq!(
        reopened
            .list_audit("alice/branches")
            .unwrap()
            .iter()
            .filter(|entry| entry.action == "branch_protection.encoded_rekey")
            .count(),
        2
    );
}
