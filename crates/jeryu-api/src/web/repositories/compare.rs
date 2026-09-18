//! `GET /api/v1/repos/:id/compare?base=&head=`: the commits reachable from
//! `head` but not from `base`, read from the forge's bare repository.
//!
//! The release views use it to say what a deployed commit is missing: with
//! `base` = the deployed sha and `head` = `main`, `ahead_by` is the unshipped
//! commit count and `commits` lists them (oldest first). Because the forge
//! enforces fast-forward merges, a merged pull request is unshipped exactly
//! when its head sha is among those commits.

use super::*;

/// Commits listed in one response; `ahead_by` still counts all of them.
const MAX_COMPARE_COMMITS: usize = 250;

#[derive(Debug, Deserialize)]
pub(in crate::web) struct CompareQuery {
    base: Option<String>,
    head: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(in crate::web) struct CompareCommit {
    sha: String,
    summary: String,
    author: String,
    committed_at: String,
}

#[derive(Debug, Serialize)]
pub(in crate::web) struct CompareResponse {
    base: String,
    head: String,
    base_sha: String,
    head_sha: String,
    ahead_by: usize,
    behind_by: usize,
    commits: Vec<CompareCommit>,
    truncated: bool,
}

pub(in crate::web) async fn repo_compare(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<CompareQuery>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    let (Some(base), Some(head)) = (query.base, query.head) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_compare",
            "compare needs both base and head",
        );
    };
    for (field, value) in [("base", &base), ("head", &head)] {
        if !is_revision(value) {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_compare",
                &format!("{field} must be a commit sha or a branch/tag name"),
            );
        }
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
    let resolve = |rev: &str| {
        git(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{rev}^{{commit}}"),
        ])
    };
    let (Some(base_sha), Some(head_sha)) = (resolve(&base), resolve(&head)) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "base or head not found");
    };
    let count = |range: String| {
        git(&["rev-list", "--count", "--end-of-options", &range]).and_then(|n| n.parse().ok())
    };
    let (Some(ahead_by), Some(behind_by)) = (
        count(format!("{base_sha}..{head_sha}")),
        count(format!("{head_sha}..{base_sha}")),
    ) else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "git_error",
            "could not count commits",
        );
    };
    let limit = format!("--max-count={MAX_COMPARE_COMMITS}");
    let Some(log) = git(&[
        "log",
        &limit,
        "--format=%H%x1f%s%x1f%an%x1f%cI",
        "--end-of-options",
        &format!("{base_sha}..{head_sha}"),
    ]) else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "git_error",
            "could not list commits",
        );
    };
    let mut commits = parse_log(&log);
    commits.reverse();
    Json(CompareResponse {
        base,
        head,
        base_sha,
        head_sha,
        ahead_by,
        behind_by,
        truncated: ahead_by > commits.len(),
        commits,
    })
    .into_response()
}

/// A full or abbreviated sha, or a branch/tag name: no option-looking values,
/// no range or reflog syntax, nothing git would read as more than one revision.
fn is_revision(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with(['-', '/', '.'])
        && !value.ends_with(['/', '.'])
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains("//")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

fn parse_log(log: &str) -> Vec<CompareCommit> {
    log.lines()
        .filter_map(|line| {
            let mut parts = line.split('\u{1f}');
            Some(CompareCommit {
                sha: parts.next()?.to_string(),
                summary: parts.next()?.to_string(),
                author: parts.next()?.to_string(),
                committed_at: parts.next()?.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_accept_shas_and_ref_names_only() {
        for good in [
            "main",
            "b2b8ea9",
            "feature/web-2",
            "v5.0.0-split.4",
            &"a".repeat(40),
        ] {
            assert!(is_revision(good), "{good:?}");
        }
        for bad in [
            "",
            "--output=/tmp/x",
            "-n1",
            "main..evil",
            "main@{1}",
            "HEAD~1",
            "main^",
            "a b",
            "/etc",
            "refs//x",
            "x/",
            ".hidden",
            "main:path",
        ] {
            assert!(!is_revision(bad), "{bad:?}");
        }
    }

    #[test]
    fn log_lines_parse_into_commits() {
        let log = "aaa\u{1f}feat: one\u{1f}alton\u{1f}2026-09-18T04:00:00+00:00\nbroken line";
        assert_eq!(
            parse_log(log),
            vec![CompareCommit {
                sha: "aaa".into(),
                summary: "feat: one".into(),
                author: "alton".into(),
                committed_at: "2026-09-18T04:00:00+00:00".into(),
            }]
        );
    }

    /// A real bare repo: main has four commits, `deployed` points at the second.
    #[tokio::test]
    async fn compare_counts_and_lists_the_commits_a_deployment_is_missing() {
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
        for n in 1..=4 {
            run(&[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("feat: change {n}"),
            ]);
            shas.push(run(&["rev-parse", "HEAD"]));
        }
        let deployed = format!("{}:refs/heads/deployed", shas[1]);
        run(&[
            "push",
            "-q",
            &bare.path.to_string_lossy(),
            "main",
            &deployed,
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
        let call = |base: &str, head: &str| {
            let state = state.clone();
            let query = CompareQuery {
                base: Some(base.to_string()),
                head: Some(head.to_string()),
            };
            async move {
                let response = repo_compare(
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

        let (status, body) = call(&shas[1], "main").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ahead_by"], 2);
        assert_eq!(body["behind_by"], 0);
        assert_eq!(body["base_sha"], shas[1]);
        assert_eq!(body["head_sha"], shas[3]);
        assert_eq!(body["truncated"], false);
        let listed: Vec<&str> = body["commits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["sha"].as_str().unwrap())
            .collect();
        assert_eq!(
            listed,
            vec![shas[2].as_str(), shas[3].as_str()],
            "oldest first"
        );
        assert_eq!(body["commits"][0]["summary"], "feat: change 3");

        let (_, current) = call("main", "deployed").await;
        assert_eq!(current["ahead_by"], 0);
        assert_eq!(current["behind_by"], 2);

        assert_eq!(
            call("--output=/tmp/x", "main").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(call("nope", "main").await.0, StatusCode::NOT_FOUND);
    }
}
