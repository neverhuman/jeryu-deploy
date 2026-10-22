use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt;

const RULE: &str = r#"{"required_status_checks":["ci"],"required_approving_review_count":1,"enforce_admins":true}"#;

#[tokio::test]
async fn branch_protection_decodes_names_for_rules_and_refs() {
    let core = ForgeCore::new();
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
    let storage = tempfile::tempdir().unwrap();
    let state = Arc::new(WebState::new_with_git_storage(
        core,
        storage.path().to_path_buf(),
    ));
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
    git(&["update-ref", "refs/heads/main", &commit]);
    assert!(
        std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&bare.path)
            .args(["branch", "release/x", "main"])
            .status()
            .unwrap()
            .success()
    );
    let main = state
        .github
        .core()
        .get_branch_protection("alice", "branches", "main")
        .unwrap();
    let app = app((*state).clone(), std::path::Path::new("/nonexistent-spa"));
    for method in ["PUT", "GET"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/api/v3/repos/alice/branches/branches/release%2Fx/protection")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(RULE))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    for branch in ["main", "release%2fx"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/v3/repos/alice/branches/branches/{branch}/protection"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let rule = response_json(response).await;
        assert!(rule["url"].as_str().unwrap().ends_with(&format!(
            "/{}/protection",
            if branch == "main" {
                "main"
            } else {
                "release/x"
            }
        )));
    }
    assert_eq!(
        state
            .github
            .core()
            .get_branch_protection("alice", "branches", "release/x")
            .unwrap()
            .branch,
        "release/x"
    );
    assert!(
        state
            .github
            .core()
            .get_branch_protection("alice", "branches", "release%2Fx")
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(
            state
                .github
                .core()
                .get_branch_protection("alice", "branches", "main")
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(main).unwrap()
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/repos/alice%2Fbranches/refs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let refs = response_json(response).await;
    assert!(
        refs.as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "release/x" && item["protected"] == true)
    );
}

#[test]
fn branch_protection_rejects_invalid_decoded_refs() {
    let router = crate::github::GithubRouter::new();
    for branch in [
        "release%2F..%2Fx",
        "bad%20name",
        "bad%00name",
        "x%2F.lock",
        "x%2F",
        "x%40%7By",
        "%FF",
        "-x",
        "x..y",
        "x%5Cy",
    ] {
        for method in [Method::Get, Method::Put] {
            assert_eq!(
                router
                    .handle(
                        method,
                        &format!("/repos/alice/branches/branches/{branch}/protection"),
                        RULE
                    )
                    .status,
                422,
                "{branch}"
            );
        }
    }
}

#[tokio::test]
async fn encoded_branch_protection_requires_admin() {
    let core = ForgeCore::new();
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
    core.create_account("admin", "admin-password", UserRole::Admin)
        .unwrap();
    core.create_account("reader", "reader-password", UserRole::User)
        .unwrap();
    let token = core
        .create_personal_access_token("reader", "test", None)
        .unwrap()
        .secret;
    let app = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        std::path::Path::new("/nonexistent-spa"),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v3/repos/alice/branches/branches/release%2Fx/protection")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(RULE))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        core.get_branch_protection("alice", "branches", "release/x")
            .is_err()
    );
}
