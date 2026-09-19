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
    tagged_at: Option<String>,
}

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
    let Ok(resolved) = state.repo_manager.resolve_parts(&repo.owner, &repo.name) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "repository has no git data",
        );
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
        return api_error(StatusCode::NOT_FOUND, "not_found", "branch not found");
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
    Json(ReleaseTagResponse {
        branch,
        tag,
        sha,
        tagged_at,
    })
    .into_response()
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
            let out = Command::new("git")
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
}
