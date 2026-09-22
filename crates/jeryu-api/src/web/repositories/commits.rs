//! `GET /api/v1/repos/:id/commits?ref=&limit=&page=`: a branch's commit
//! history, newest first, read from the forge's bare repository.
//!
//! The web UI shows a branch's commits; this is the same list over the API.
//! `ref` defaults to the repository's default branch and accepts a branch,
//! tag or sha (see [`is_revision`]). Paging follows the other `/api/v1`
//! collections: `limit` / `per_page` and a 1-based `page`, echoed in `page`.

use super::compare::{CompareCommit, is_revision, parse_log};
use super::*;

#[derive(Debug, Default, Deserialize)]
pub(in crate::web) struct CommitsQuery {
    #[serde(rename = "ref")]
    ref_name: Option<String>,
    #[serde(flatten)]
    paging: PageParams,
}

#[derive(Debug, Serialize)]
pub(in crate::web) struct CommitsResponse {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    commits: Vec<CompareCommit>,
    page: PageInfo,
}

pub(in crate::web) async fn repo_commits(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<CommitsQuery>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    let page = match query.paging.page() {
        Ok(page) => page,
        Err(rejection) => return rejection.into_response(),
    };
    let ref_name = query
        .ref_name
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| repo.default_branch.clone());
    if !is_revision(&ref_name) {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_ref",
            "ref must be a commit sha or a branch/tag name",
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
    let Some(sha) = git(&[
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        &format!("{ref_name}^{{commit}}"),
    ]) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "ref not found");
    };
    let Some(total) = git(&["rev-list", "--count", "--end-of-options", &sha])
        .and_then(|n| n.parse::<usize>().ok())
    else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "git_error",
            "could not count commits",
        );
    };
    let skip = (page.page - 1).saturating_mul(page.limit).min(total);
    let Some(log) = git(&[
        "log",
        &format!("--max-count={}", page.limit),
        &format!("--skip={skip}"),
        "--format=%H%x1f%s%x1f%an%x1f%cI",
        "--end-of-options",
        &sha,
    ]) else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "git_error",
            "could not list commits",
        );
    };
    let commits = parse_log(&log);
    let page = PageInfo {
        limit: page.limit,
        page: page.page,
        total,
        has_more: skip + commits.len() < total,
    };
    Json(CommitsResponse {
        ref_name,
        sha,
        commits,
        page,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real bare repo: main has five commits, `topic` stops at the second.
    #[tokio::test]
    async fn commits_list_a_branch_history_on_both_edges() {
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
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q"]);
        let mut shas = Vec::new();
        for n in 1..=5 {
            run(&[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("feat: change {n}"),
            ]);
            shas.push(run(&["rev-parse", "HEAD"]));
        }
        let topic = format!("{}:refs/heads/topic", shas[1]);
        run(&["push", "-q", &bare.path.to_string_lossy(), "main", &topic]);

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
        let call = |ref_name: Option<&str>, limit: Option<&str>, page: Option<&str>| {
            let state = state.clone();
            let query = CommitsQuery {
                ref_name: ref_name.map(str::to_string),
                paging: PageParams {
                    limit: limit.map(str::to_string),
                    per_page: None,
                    page: page.map(str::to_string),
                },
            };
            async move {
                let response = repo_commits(
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
        let listed = |body: &serde_json::Value| -> Vec<String> {
            body["commits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["sha"].as_str().unwrap().to_string())
                .collect()
        };

        let (status, body) = call(None, None, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ref"], "main");
        assert_eq!(body["sha"], shas[4]);
        assert_eq!(body["page"]["total"], 5);
        assert_eq!(body["commits"][0]["summary"], "feat: change 5");
        assert_eq!(listed(&body)[4], shas[0], "newest first");

        let (_, second) = call(Some("main"), Some("2"), Some("2")).await;
        assert_eq!(listed(&second), vec![shas[2].clone(), shas[1].clone()]);
        assert_eq!(second["page"]["has_more"], true);
        let (_, last) = call(Some("main"), Some("2"), Some("3")).await;
        assert_eq!(listed(&last), vec![shas[0].clone()]);
        assert_eq!(last["page"]["has_more"], false);

        let (_, branch) = call(Some("topic"), None, None).await;
        assert_eq!(listed(&branch), vec![shas[1].clone(), shas[0].clone()]);

        assert_eq!(
            call(Some("nope"), None, None).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(Some("--output=/tmp/x"), None, None).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            call(None, Some("0"), None).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );

        // The GitHub-compatible edge answers the same history in GitHub's shape.
        let v3 = state.github.get("/repos/alice/svc/commits?per_page=2");
        assert_eq!(v3.status, 200, "{}", v3.body);
        let v3_body: serde_json::Value = serde_json::from_str(&v3.body).unwrap();
        assert_eq!(v3_body[0]["sha"], shas[4].as_str());
        assert_eq!(v3_body[0]["commit"]["message"], "feat: change 5");
        assert_eq!(v3_body[0]["parents"][0]["sha"], shas[3].as_str());
        let link = v3
            .headers
            .iter()
            .find(|(name, _)| name == "Link")
            .map(|(_, value)| value.clone())
            .expect("Link header");
        assert!(link.contains("page=3>; rel=\"last\""), "{link}");
        let v3_topic = state.github.get("/repos/alice/svc/commits?sha=topic");
        let v3_topic: serde_json::Value = serde_json::from_str(&v3_topic.body).unwrap();
        assert_eq!(v3_topic.as_array().unwrap().len(), 2);
        assert_eq!(
            state.github.get("/repos/alice/svc/commits?sha=nope").status,
            404
        );
    }
}
