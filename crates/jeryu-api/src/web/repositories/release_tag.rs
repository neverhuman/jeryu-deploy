//! `GET /api/v1/repos/:id/release-tag?branch=`: the newest tag reachable from
//! a branch (default: the repository's default branch).
//!
//! Split repositories are released by tag rather than by a recorded
//! deployment. The unreleased-work view asks for the nearest tag on `main`,
//! then `compare?base=<tag>&head=main` for what has landed since. A merged
//! pull request is released exactly when its (fast-forward) merge commit is
//! an ancestor of that tag. `tag: null` means the branch carries no tag.

use super::compare::is_revision;
use super::*;

#[derive(Debug, Deserialize)]
pub(in crate::web) struct ReleaseTagQuery {
    branch: Option<String>,
}

#[derive(Debug, Serialize)]
pub(in crate::web) struct ReleaseTagResponse {
    branch: String,
    tag: Option<String>,
    sha: Option<String>,
    /// The tagged commit's committer date (`%cI`), not the tag's creation
    /// date: lightweight tags carry no date of their own.
    tagged_at: Option<String>,
}

/// Read access is enforced before this handler runs: `auth::gate` resolves
/// `:id` for every `/api/v1/repos/:id/...` path and refuses callers who
/// cannot read the repository (admins pass), exactly as for `refs`, `tree`
/// and `compare`. The route tests below pin that down.
pub(in crate::web) async fn repo_release_tag(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ReleaseTagQuery>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    let branch = query.branch.unwrap_or_else(|| repo.default_branch.clone());
    if !is_revision(&branch) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_branch",
            "branch must be a branch name or commit sha",
        );
    }
    let released = match nearest_release_tag(&state, &repo, &branch) {
        Ok(released) => released,
        Err(TagLookupMiss::NoGitData) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "not_found",
                "repository has no git data",
            );
        }
        Err(TagLookupMiss::NoBranch) => {
            return api_error(StatusCode::NOT_FOUND, "not_found", "branch not found");
        }
    };
    let (tag, sha, tagged_at) = match released {
        Some(released) => (Some(released.tag), released.sha, released.tagged_at),
        None => (None, None, None),
    };
    Json(ReleaseTagResponse {
        branch,
        tag,
        sha,
        tagged_at,
    })
    .into_response()
}

/// The newest tag reachable from a branch, as `describe --tags` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::web) struct ReleasedTag {
    pub(in crate::web) tag: String,
    pub(in crate::web) sha: Option<String>,
    pub(in crate::web) tagged_at: Option<String>,
}

/// Why no tag lookup could run at all (as opposed to "no tag reachable").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::web) enum TagLookupMiss {
    NoGitData,
    NoBranch,
}

/// Resolve the newest tag reachable from `branch`. `Ok(None)` means the
/// branch exists but carries no tag: nothing has been released from it.
/// `branch` must already have passed `is_revision`.
pub(in crate::web) fn nearest_release_tag(
    state: &WebState,
    repo: &Repository,
    branch: &str,
) -> Result<Option<ReleasedTag>, TagLookupMiss> {
    let Ok(resolved) = state.repo_manager.resolve_parts(&repo.owner, &repo.name) else {
        return Err(TagLookupMiss::NoGitData);
    };
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new(&state.repo_manager.config().git_bin)
            .arg("-C")
            .arg(&resolved.path)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let branch_rev = format!("{branch}^{{commit}}");
    if git(&[
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        &branch_rev,
    ])
    .is_none()
    {
        return Err(TagLookupMiss::NoBranch);
    }
    // `describe` fails when no tag is reachable: that is "never released".
    let tag = git(&[
        "describe",
        "--tags",
        "--abbrev=0",
        "--end-of-options",
        &branch_rev,
    ])
    .filter(|tag| !tag.is_empty());
    let (sha, tagged_at) = match tag.as_deref() {
        Some(tag) => {
            let rev = format!("refs/tags/{tag}^{{commit}}");
            let sha = git(&["rev-parse", "--verify", "--quiet", "--end-of-options", &rev]);
            let at = git(&["log", "-1", "--format=%cI", "--end-of-options", &rev]);
            (sha, at)
        }
        None => (None, None),
    };
    Ok(tag.map(|tag| ReleasedTag {
        tag,
        sha,
        tagged_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real bare repo: main has three commits, `v1.0.0` tags the second.
    #[tokio::test]
    async fn release_tag_is_the_newest_tag_reachable_from_the_branch() {
        use crate::web::catalog::SplitCatalog;
        use jeryu_core::{CreateRepositoryRequest, ForgeCore};
        use jeryu_gitd::{GitdConfig, RepoId, RepoManager};

        let storage = tempfile::tempdir().unwrap();
        let manager = RepoManager::new(GitdConfig::new(storage.path().to_path_buf()));
        let bare = manager
            .create_bare(&RepoId::new("alice", "svc").unwrap())
            .unwrap();
        let work = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = crate::test_git::git_command()
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "init.defaultBranch=main"])
                .args(args)
                .current_dir(work.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q"]);
        let mut shas = Vec::new();
        for n in 1..=3 {
            run(&["commit", "-q", "--allow-empty", "-m", &format!("feat: {n}")]);
            shas.push(run(&["rev-parse", "HEAD"]));
        }
        run(&["tag", "v0.9.0", &shas[0]]);
        run(&["tag", "v1.0.0", &shas[1]]);
        run(&["branch", "early", &shas[0]]);
        run(&["checkout", "-q", "--orphan", "untagged"]);
        run(&["commit", "-q", "--allow-empty", "-m", "feat: unrelated"]);
        run(&[
            "push",
            "-q",
            &bare.path.to_string_lossy(),
            "main",
            "early",
            "untagged",
            "--tags",
        ]);

        let core = ForgeCore::new();
        core.create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "svc".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
        let state = std::sync::Arc::new(WebState::with_repo_manager(
            core,
            std::sync::Arc::new(manager),
            std::path::PathBuf::from("/tmp/jeryu-no-spa"),
            std::env::temp_dir(),
            SplitCatalog::builtin(),
        ));
        let call = |branch: Option<&str>| {
            let state = state.clone();
            let query = ReleaseTagQuery {
                branch: branch.map(str::to_string),
            };
            async move {
                let response = repo_release_tag(
                    State(state),
                    AxumPath("alice/svc".to_string()),
                    Query(query),
                )
                .await;
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (
                    status,
                    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                )
            }
        };

        let (status, body) = call(None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["branch"], "main");
        assert_eq!(body["tag"], "v1.0.0");
        assert_eq!(body["sha"], shas[1]);
        assert!(body["tagged_at"].is_string(), "{body}");

        let (_, early) = call(Some("early")).await;
        assert_eq!(early["tag"], "v0.9.0");

        // No tag reachable: nothing has been released from this branch.
        let (status, none) = call(Some("untagged")).await;
        assert_eq!(status, StatusCode::OK, "{none}");
        assert!(none["tag"].is_null(), "{none}");
        assert!(none["sha"].is_null(), "{none}");

        assert_eq!(call(Some("--output=/x")).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(call(Some("nope")).await.0, StatusCode::NOT_FOUND);
    }

    /// Through the full router with auth on: a private repo's tag is only
    /// visible to users who can read it, and a refusal matches `/refs`,
    /// `/tree` and `/compare` without leaking the tag, sha or date.
    #[tokio::test]
    async fn release_tag_route_enforces_repository_read_access() {
        use crate::web::catalog::SplitCatalog;
        use axum::body::Body;
        use axum::http::{Request, header};
        use jeryu_core::{CreateRepositoryRequest, ForgeCore, RepoAccessLevel, UserRole};
        use jeryu_gitd::{GitdConfig, RepoId, RepoManager};
        use tower::ServiceExt;

        let storage = tempfile::tempdir().unwrap();
        let manager = RepoManager::new(GitdConfig::new(storage.path().to_path_buf()));
        let bare = manager
            .create_bare(&RepoId::new("alice", "vault").unwrap())
            .unwrap();
        let work = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = crate::test_git::git_command()
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "init.defaultBranch=main"])
                .args(args)
                .current_dir(work.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q"]);
        run(&["commit", "-q", "--allow-empty", "-m", "feat: one"]);
        let sha = run(&["rev-parse", "HEAD"]);
        run(&["tag", "v7.7.7-secret"]);
        run(&["push", "-q", &bare.path.to_string_lossy(), "main", "--tags"]);

        let core = ForgeCore::new();
        let repo = core
            .create_repository(
                "alice",
                CreateRepositoryRequest {
                    name: "vault".to_string(),
                    private: true,
                    description: None,
                    default_branch: Some("main".to_string()),
                },
            )
            .unwrap();
        core.create_account("jeryu-admin", "admin-password", UserRole::Admin)
            .unwrap();
        core.create_account("reader", "reader-password", UserRole::User)
            .unwrap();
        core.create_account("outsider", "outsider-password", UserRole::User)
            .unwrap();
        core.grant_repo_access(
            "jeryu-admin",
            "reader",
            "alice",
            "vault",
            RepoAccessLevel::Read,
        )
        .unwrap();
        let token = |login: &str| {
            core.create_personal_access_token(login, "test", None)
                .unwrap()
                .secret
        };
        let (admin, reader, outsider) = (token("jeryu-admin"), token("reader"), token("outsider"));
        let state = WebState::with_repo_manager(
            core,
            std::sync::Arc::new(manager),
            std::path::PathBuf::from("/tmp/jeryu-no-spa"),
            std::env::temp_dir(),
            SplitCatalog::builtin(),
        )
        .with_auth(true, false, false);
        let app = crate::web::app(state, std::path::Path::new("/tmp/jeryu-no-spa"));
        let get = |path: String, token: String| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::builder()
                            .uri(path)
                            .header(header::AUTHORIZATION, format!("Bearer {token}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (status, String::from_utf8_lossy(&bytes).to_string())
            }
        };
        let id = repo.id.to_string();
        let release_tag = format!("/api/v1/repos/{id}/release-tag");

        // Unauthorized: the same refusal as the sibling per-repo routes.
        let (denied, body) = get(release_tag.clone(), outsider.clone()).await;
        assert_eq!(denied, StatusCode::FORBIDDEN, "{body}");
        for leak in ["v7.7.7-secret", sha.as_str(), "tagged_at"] {
            assert!(!body.contains(leak), "refusal leaked {leak}: {body}");
        }
        for sibling in ["refs", "tree", "compare?base=main&head=main"] {
            let (status, _) = get(format!("/api/v1/repos/{id}/{sibling}"), outsider.clone()).await;
            assert_eq!(
                status, denied,
                "{sibling} and release-tag must refuse alike"
            );
        }
        let (by_name, body) = get(
            "/api/v1/repos/alice%2Fvault/release-tag".to_string(),
            outsider,
        )
        .await;
        assert_eq!(by_name, StatusCode::FORBIDDEN, "{body}");
        assert!(!body.contains("v7.7.7-secret"), "{body}");

        // A granted reader and an admin both see the tag.
        for caller in [reader, admin] {
            let (status, body) = get(release_tag.clone(), caller).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["tag"], "v7.7.7-secret");
            assert_eq!(body["sha"], sha);
        }
    }
}
