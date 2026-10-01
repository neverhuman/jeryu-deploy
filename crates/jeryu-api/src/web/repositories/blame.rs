//! `GET /api/v1/repos/:id/blame?ref=&path=`: which commit last changed each
//! line of a file, from `git blame --porcelain`.
//!
//! The answer is grouped: `hunks` are runs of consecutive lines last touched
//! by one commit (1-based `start_line`, `line_count`), and `commits` holds each
//! of those commits once. A reader can lay notes beside a rendered document
//! (the wiki's margin) without receiving the file's text a second time. `ref`
//! defaults to the default branch; files over the blob preview limit answer
//! `blob_too_large`, as the blob route does.

use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};

use super::source::{
    git_lookup, git_object_size, git_output, missing_param_error, normalize_git_path,
    required_param, resolve_commit, source_ref,
};
use super::*;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(in crate::web) struct BlameCommit {
    sha: String,
    summary: String,
    author: String,
    authored_at: String,
    /// True for the repository's root commit (or a shallow boundary): the
    /// line has been there since the start of the history the server holds.
    boundary: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(in crate::web) struct BlameHunk {
    start_line: usize,
    line_count: usize,
    commit: String,
}

#[derive(Debug, Serialize)]
pub(in crate::web) struct BlameResponse {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    path: String,
    line_count: usize,
    hunks: Vec<BlameHunk>,
    commits: Vec<BlameCommit>,
}

pub(in crate::web) async fn repo_blame(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<SourceQuery>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    let Some(path) = required_param(query.path.as_deref()) else {
        return missing_param_error("path");
    };
    let ref_name = source_ref(&repo, &query).to_string();
    match blame(&state, &repo, &ref_name, path) {
        Ok(response) => Json(response).into_response(),
        Err(response) => *response,
    }
}

fn blame(
    state: &WebState,
    repo: &Repository,
    ref_name: &str,
    path: &str,
) -> SourceResult<BlameResponse> {
    let path = normalize_git_path(Some(path))?;
    let bare = state
        .repo_manager
        .open_parts(&repo.owner, &repo.name)
        .map_err(|_| {
            Box::new(api_error(
                StatusCode::NOT_FOUND,
                "not_found",
                "repository storage not found",
            ))
        })?;
    let commit = resolve_commit(state, &bare, ref_name)?;
    let spec = format!("{commit}:{path}");
    let kind = git_lookup(state, &bare.path, &["cat-file", "-t", &spec])?
        .map(|out| String::from_utf8_lossy(&out).trim().to_string());
    if kind.as_deref() != Some("blob") {
        return Err(Box::new(api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "file not found at this ref",
        )));
    }
    if git_object_size(state, &bare.path, &spec)? > MAX_BLOB_PREVIEW_BYTES {
        return Err(Box::new(api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "blob_too_large",
            "file is too large to blame",
        )));
    }
    let out = git_output(
        state,
        &bare.path,
        &["blame", "--porcelain", &commit, "--", &path],
    )?;
    let (hunks, commits) = parse_porcelain(&String::from_utf8_lossy(&out));
    Ok(BlameResponse {
        ref_name: ref_name.to_string(),
        sha: commit,
        path,
        line_count: hunks.iter().map(|hunk| hunk.line_count).sum(),
        hunks,
        commits,
    })
}

fn is_header(fields: &[&str]) -> bool {
    matches!(fields.len(), 3 | 4)
        && fields[0].len() >= 40
        && fields[0].chars().all(|c| c.is_ascii_hexdigit())
        && fields[1..].iter().all(|n| n.parse::<usize>().is_ok())
}

/// Parse `git blame --porcelain` into merged hunks and the commits they name,
/// newest commit first.
fn parse_porcelain(text: &str) -> (Vec<BlameHunk>, Vec<BlameCommit>) {
    let mut hunks: Vec<BlameHunk> = Vec::new();
    let mut commits: BTreeMap<String, (BlameCommit, i64)> = BTreeMap::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.starts_with('\t') {
            continue;
        }
        let fields: Vec<&str> = line.split(' ').collect();
        if is_header(&fields) {
            current = fields[0].to_string();
            let final_line: usize = fields[2].parse().unwrap_or(0);
            commits.entry(current.clone()).or_insert_with(|| {
                (
                    BlameCommit {
                        sha: current.clone(),
                        ..BlameCommit::default()
                    },
                    0,
                )
            });
            match hunks.last_mut() {
                Some(last)
                    if last.commit == current
                        && last.start_line + last.line_count == final_line =>
                {
                    last.line_count += 1;
                }
                _ => hunks.push(BlameHunk {
                    start_line: final_line,
                    line_count: 1,
                    commit: current.clone(),
                }),
            }
            continue;
        }
        let Some((commit, time)) = commits.get_mut(&current) else {
            continue;
        };
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "author" => commit.author = value.to_string(),
            "summary" => commit.summary = value.to_string(),
            "boundary" => commit.boundary = true,
            "author-time" => {
                *time = value.parse().unwrap_or(0);
                commit.authored_at = Utc
                    .timestamp_opt(*time, 0)
                    .single()
                    .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                    .unwrap_or_default();
            }
            _ => {}
        }
    }
    let mut commits: Vec<(BlameCommit, i64)> = commits.into_values().collect();
    commits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.sha.cmp(&b.0.sha)));
    (
        hunks,
        commits.into_iter().map(|(commit, _)| commit).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn porcelain_groups_runs_and_lists_each_commit_once() {
        let porcelain = format!(
            "{A} 1 1 2\nauthor Ada\nauthor-mail <a@example.test>\nauthor-time 1700000000\n\
             author-tz +0000\nsummary docs: first page\nboundary\nfilename page.md\n\t# Title\n\
             {A} 2 2\n\t\n\
             {B} 3 3 1\nauthor Bea\nauthor-time 1700086400\nsummary docs: add setup\n\
             previous {A} page.md\nfilename page.md\n\tSetup line\n\
             {A} 3 4 1\n\tOld tail\n"
        );
        let (hunks, commits) = parse_porcelain(&porcelain);
        assert_eq!(
            hunks,
            vec![
                BlameHunk {
                    start_line: 1,
                    line_count: 2,
                    commit: A.into()
                },
                BlameHunk {
                    start_line: 3,
                    line_count: 1,
                    commit: B.into()
                },
                BlameHunk {
                    start_line: 4,
                    line_count: 1,
                    commit: A.into()
                },
            ]
        );
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, B, "newest first");
        assert_eq!(commits[0].author, "Bea");
        assert_eq!(commits[0].summary, "docs: add setup");
        assert_eq!(commits[0].authored_at, "2023-11-15T22:13:20Z");
        assert!(!commits[0].boundary);
        assert!(commits[1].boundary);
    }

    /// A real bare repo: two commits shape one page; blame, the page list and
    /// the path-filtered history all read it back.
    #[tokio::test]
    async fn blame_pages_and_path_history_on_a_real_repo() {
        use crate::web::catalog::SplitCatalog;
        use jeryu_core::{CreateRepositoryRequest, ForgeCore};
        use jeryu_gitd::{GitdConfig, RepoId, RepoManager};

        let storage = tempfile::tempdir().unwrap();
        let manager = RepoManager::new(GitdConfig::new(storage.path().to_path_buf()));
        let bare = manager
            .create_bare(&RepoId::new("acme", "handbook").unwrap())
            .unwrap();
        let work = tempfile::tempdir().unwrap();
        let run = |author: &str, args: &[&str]| {
            let out = Command::new("git")
                .args([
                    "-c",
                    &format!("user.name={author}"),
                    "-c",
                    "user.email=t@example.test",
                ])
                .args(["-c", "init.defaultBranch=main"])
                .args(args)
                .current_dir(work.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run("Ada", &["init", "-q"]);
        std::fs::create_dir_all(work.path().join("guides")).unwrap();
        std::fs::write(work.path().join("guides/setup.md"), "# Setup\n\nStep one\n").unwrap();
        std::fs::write(work.path().join("index.md"), "# Index\n").unwrap();
        std::fs::write(work.path().join("lint.py"), "print()\n").unwrap();
        run("Ada", &["add", "."]);
        run(
            "Ada",
            &[
                "commit",
                "-q",
                "--date=2026-01-01T00:00:00Z",
                "-m",
                "docs: first pages",
            ],
        );
        let first = run("Ada", &["rev-parse", "HEAD"]);
        std::fs::write(
            work.path().join("guides/setup.md"),
            "# Setup\n\nStep one\nStep two\n",
        )
        .unwrap();
        run(
            "Bea",
            &[
                "commit",
                "-q",
                "--date=2026-02-01T00:00:00Z",
                "-am",
                "docs: add step two",
            ],
        );
        let second = run("Bea", &["rev-parse", "HEAD"]);
        run("Ada", &["push", "-q", &bare.path.to_string_lossy(), "main"]);

        let core = ForgeCore::new();
        core.create_repository(
            "acme",
            CreateRepositoryRequest {
                name: "handbook".to_string(),
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
        let body = |response: AxumResponse| async move {
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            )
        };
        let source = |path: Option<&str>| SourceQuery {
            ref_name: None,
            path: path.map(str::to_string),
            render: None,
        };

        let (status, blamed) = body(
            repo_blame(
                State(state.clone()),
                AxumPath("acme/handbook".to_string()),
                Query(source(Some("guides/setup.md"))),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{blamed}");
        assert_eq!(blamed["sha"], second.as_str());
        assert_eq!(blamed["line_count"], 4);
        assert_eq!(blamed["hunks"][0]["start_line"], 1);
        assert_eq!(blamed["hunks"][0]["line_count"], 3);
        assert_eq!(blamed["hunks"][0]["commit"], first.as_str());
        assert_eq!(blamed["hunks"][1]["start_line"], 4);
        assert_eq!(blamed["hunks"][1]["commit"], second.as_str());
        assert_eq!(blamed["commits"][0]["author"], "Bea");
        assert_eq!(blamed["commits"][0]["summary"], "docs: add step two");
        assert_eq!(blamed["commits"][0]["authored_at"], "2026-02-01T00:00:00Z");

        for (path, want) in [
            (Some("missing.md"), StatusCode::NOT_FOUND),
            (Some("guides"), StatusCode::NOT_FOUND),
            (Some("../etc/passwd"), StatusCode::UNPROCESSABLE_ENTITY),
            (None, StatusCode::UNPROCESSABLE_ENTITY),
        ] {
            let response = repo_blame(
                State(state.clone()),
                AxumPath("acme/handbook".to_string()),
                Query(source(path)),
            )
            .await;
            assert_eq!(response.status(), want, "{path:?}");
        }

        let (status, pages) = body(
            repo_pages(
                State(state.clone()),
                AxumPath("acme/handbook".to_string()),
                Query(source(None)),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{pages}");
        assert_eq!(pages["ref"], "main");
        assert_eq!(pages["pages"][0]["path"], "guides/setup.md");
        assert_eq!(pages["pages"][1]["path"], "index.md");
        assert_eq!(pages["pages"].as_array().unwrap().len(), 2);

        let (status, history) = body(
            repo_commits(
                State(state.clone()),
                AxumPath("acme/handbook".to_string()),
                Query(super::super::commits::CommitsQuery::for_path("index.md")),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{history}");
        assert_eq!(history["page"]["total"], 1, "index.md changed once");
        assert_eq!(history["commits"][0]["sha"], first.as_str());
    }
}
