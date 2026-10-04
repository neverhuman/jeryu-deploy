//! `GET /api/v1/repos/:id/commit/:sha`: one commit — its message, who wrote
//! it, its parents and its diff.
//!
//! The commits list says what changed in a repository; this says what one of
//! those commits did. The diff is the shape the pull request cockpit already
//! renders (`pulls::diff`), so the web viewer is the same component against
//! the same `files`/`hunks` payload. A merge commit is diffed against its
//! first parent, as `git show` does.

use super::compare::is_revision;
use super::source::{git_lookup, git_output, resolve_commit};
use super::*;
use crate::web::pulls::PullRequestDiffFile;
use crate::web::pulls::diff::{cap_lines, parse_unified_diff};

/// Field separator of the `git show -s` format below: never in a git field.
const UNIT: char = '\u{1f}';

#[derive(Debug, Serialize)]
pub(in crate::web) struct CommitDetail {
    sha: String,
    /// The first line of the message.
    summary: String,
    /// The whole message, subject line and body.
    message: String,
    author: String,
    author_email: String,
    authored_at: String,
    committed_at: String,
    parents: Vec<String>,
    files: Vec<PullRequestDiffFile>,
    truncated: bool,
}

pub(in crate::web) async fn repo_commit(
    State(state): State<std::sync::Arc<WebState>>,
    AxumPath((id, sha)): AxumPath<(String, String)>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    if !is_revision(&sha) {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_ref",
            "commit must be a commit sha or a branch/tag name",
        );
    }
    match commit_detail(&state, &repo, &sha) {
        Ok(detail) => Json(detail).into_response(),
        Err(response) => *response,
    }
}

fn commit_detail(state: &WebState, repo: &Repository, rev: &str) -> SourceResult<CommitDetail> {
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
    let sha = resolve_commit(state, &bare, rev)?;
    // `%B` is last: it is the only field that may hold newlines.
    let format = format!("--format=%s{UNIT}%an{UNIT}%ae{UNIT}%aI{UNIT}%cI{UNIT}%P{UNIT}%B");
    let meta = git_lookup(state, &bare.path, &["show", "-s", &format, &sha])?.ok_or_else(|| {
        Box::new(api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "commit not found",
        ))
    })?;
    let meta = String::from_utf8_lossy(&meta);
    let Some(fields) = parse_meta(&meta) else {
        return Err(Box::new(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "git_error",
            "could not read the commit",
        )));
    };
    // `-m --first-parent` gives a merge commit one diff, against the parent
    // the branch was merged into; a single-parent commit is unaffected.
    let diff = git_output(
        state,
        &bare.path,
        &[
            "-c",
            "core.quotePath=false",
            "show",
            "--format=",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            "--unified=3",
            "-m",
            "--first-parent",
            &sha,
        ],
    )?;
    let (files, truncated) = cap_lines(parse_unified_diff(&String::from_utf8_lossy(&diff)));
    Ok(CommitDetail {
        sha,
        files,
        truncated,
        ..fields
    })
}

/// The `git show -s` line above, split back into fields. The message keeps its
/// line breaks and loses the trailing newline git adds.
fn parse_meta(meta: &str) -> Option<CommitDetail> {
    let mut parts = meta.splitn(7, UNIT);
    let summary = parts.next()?.to_string();
    let author = parts.next()?.to_string();
    let author_email = parts.next()?.to_string();
    let authored_at = parts.next()?.to_string();
    let committed_at = parts.next()?.to_string();
    let parents = parts
        .next()?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let message = parts.next()?.trim_end().to_string();
    Some(CommitDetail {
        sha: String::new(),
        summary,
        message,
        author,
        author_email,
        authored_at,
        committed_at,
        parents,
        files: Vec::new(),
        truncated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real bare repo: one commit adds a file, the next edits it.
    #[tokio::test]
    async fn commit_detail_carries_the_message_and_the_diff() {
        use crate::web::catalog::SplitCatalog;
        use jeryu_core::{CreateRepositoryRequest, ForgeCore};
        use jeryu_gitd::{GitdConfig, RepoId, RepoManager};

        let storage = tempfile::tempdir().unwrap();
        let manager = RepoManager::new(GitdConfig::new(storage.path().to_path_buf()));
        let bare = manager
            .create_bare(&RepoId::new("acme", "widget").unwrap())
            .unwrap();
        let work = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = crate::test_git::git_command()
                .args(["-c", "user.name=Ada", "-c", "user.email=ada@acme.example"])
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
        std::fs::write(work.path().join("main.rs"), "fn main() {}\n").unwrap();
        run(&["add", "main.rs"]);
        run(&["commit", "-q", "-m", "feat: add the entry point"]);
        let first = run(&["rev-parse", "HEAD"]);
        std::fs::write(work.path().join("main.rs"), "fn main() {\n    run();\n}\n").unwrap();
        run(&[
            "commit",
            "-q",
            "-a",
            "-m",
            "feat: call run\n\nThe entry point did nothing.",
        ]);
        let second = run(&["rev-parse", "HEAD"]);
        run(&["push", "-q", &bare.path.to_string_lossy(), "main"]);

        let core = ForgeCore::new();
        core.create_repository(
            "acme",
            CreateRepositoryRequest {
                name: "widget".to_string(),
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
        let call = |rev: &str| {
            let state = state.clone();
            let rev = rev.to_string();
            async move {
                let response =
                    repo_commit(State(state), AxumPath(("acme/widget".to_string(), rev))).await;
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

        let (status, body) = call(&second).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["sha"], second);
        assert_eq!(body["summary"], "feat: call run");
        assert_eq!(
            body["message"],
            "feat: call run\n\nThe entry point did nothing."
        );
        assert_eq!(body["author"], "Ada");
        assert_eq!(body["author_email"], "ada@acme.example");
        assert_eq!(body["parents"], serde_json::json!([first]));
        assert_eq!(body["truncated"], false);
        assert_eq!(body["files"][0]["path"], "main.rs");
        assert_eq!(body["files"][0]["status"], "modified");
        assert_eq!(body["files"][0]["additions"], 3);
        assert_eq!(body["files"][0]["deletions"], 1);
        let lines: Vec<&str> = body["files"][0]["hunks"][0]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap())
            .collect();
        assert!(lines.contains(&"+    run();"), "{lines:?}");

        // The root commit has no parent and still answers its added file.
        let (_, root) = call(&first).await;
        assert_eq!(root["parents"], serde_json::json!([]));
        assert_eq!(root["files"][0]["status"], "added");

        // A branch name resolves like a sha; junk and ranges do not.
        let (_, by_ref) = call("main").await;
        assert_eq!(by_ref["sha"], second);
        assert_eq!(call("nope").await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            call("--output=/tmp/x").await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}
