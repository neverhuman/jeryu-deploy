//! `GET /repos/{owner}/{repo}/commits?sha=&per_page=&page=`: a branch's commit
//! history, newest first, read from the bare repository the forge serves.
//!
//! `sha` names a branch, tag or commit and defaults to the repository's default
//! branch, as on GitHub. Pages are cut by git itself (`--skip`/`--max-count`),
//! so a long history is never read whole; `Link` carries the page relations.

use serde_json::{Value, json};

use crate::routes::Response;

use super::GithubRouter;
use super::support::{Pagination, docs_url, error_response, json_response};

impl GithubRouter {
    pub(super) fn list_commits(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        page: Pagination,
        query: &str,
    ) -> Response {
        let repository = match self.core.get_repository(owner, repo) {
            Ok(repository) => repository,
            Err(err) => return error_response(err),
        };
        let requested = query_value(query, "sha").filter(|value| !value.is_empty());
        let reference = requested
            .clone()
            .unwrap_or_else(|| repository.default_branch.clone());
        if !is_revision(&reference) {
            return json_response(
                422,
                &json!({
                    "message": "sha must be a commit sha or a branch/tag name",
                    "documentation_url": docs_url(),
                }),
            );
        }
        let base = match &requested {
            Some(sha) => format!("{path}?sha={sha}"),
            None => path.to_owned(),
        };
        self.git_commits_page(owner, repo, &reference, &base, page)
    }

    #[cfg(feature = "web")]
    fn git_commits_page(
        &self,
        owner: &str,
        repo: &str,
        reference: &str,
        base: &str,
        page: Pagination,
    ) -> Response {
        let Some(manager) = &self.repo_manager else {
            return no_history();
        };
        let Ok(resolved) = manager.resolve_parts(owner, repo) else {
            return no_history();
        };
        let git = |args: &[&str]| -> Option<String> {
            let out = std::process::Command::new(&manager.config().git_bin)
                .arg("-C")
                .arg(&resolved.path)
                .args(args)
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        let Some(head) = git(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ]) else {
            return json_response(
                404,
                &json!({
                    "message": format!("No commit found for SHA: {reference}"),
                    "documentation_url": docs_url(),
                }),
            );
        };
        let total = git(&["rev-list", "--count", "--end-of-options", &head])
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(0);
        let skip = page.page.saturating_sub(1).saturating_mul(page.per_page);
        let log = git(&[
            "log",
            &format!("--max-count={}", page.per_page),
            &format!("--skip={skip}"),
            "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%cn%x1f%ce%x1f%cI%x1f%B%x1e",
            "--end-of-options",
            &head,
        ])
        .unwrap_or_default();
        let commits: Vec<Value> = log
            .split('\u{1e}')
            .filter_map(|record| commit_json(owner, repo, record.trim_start_matches('\n')))
            .collect();
        let last_page = total.div_ceil(page.per_page).max(1);
        let mut response = json_response(200, &Value::Array(commits));
        if let Some(link) = super::support::link_header(base, page.per_page, page.page, last_page) {
            response.headers.push(("Link".to_owned(), link));
        }
        response
    }

    #[cfg(not(feature = "web"))]
    fn git_commits_page(
        &self,
        _owner: &str,
        _repo: &str,
        _reference: &str,
        _base: &str,
        _page: Pagination,
    ) -> Response {
        no_history()
    }
}

fn no_history() -> Response {
    json_response(
        409,
        &json!({
            "message": "Git Repository is empty.",
            "documentation_url": docs_url(),
        }),
    )
}

/// One `git log` record in the GitHub commit-list shape.
#[cfg_attr(not(feature = "web"), allow(dead_code))]
fn commit_json(owner: &str, repo: &str, record: &str) -> Option<Value> {
    let mut parts = record.split('\u{1f}');
    let sha = parts.next()?.trim();
    if sha.is_empty() {
        return None;
    }
    let parents = parts.next()?;
    let (author_name, author_email, author_date) = (parts.next()?, parts.next()?, parts.next()?);
    let (committer_name, committer_email, committer_date) =
        (parts.next()?, parts.next()?, parts.next()?);
    let message = parts.next()?.trim_end();
    Some(json!({
        "sha": sha,
        "url": format!("/repos/{owner}/{repo}/commits/{sha}"),
        "commit": {
            "message": message,
            "author": {"name": author_name, "email": author_email, "date": author_date},
            "committer": {"name": committer_name, "email": committer_email, "date": committer_date},
        },
        "parents": parents
            .split_whitespace()
            .map(|parent| json!({"sha": parent}))
            .collect::<Vec<_>>(),
    }))
}

fn query_value(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.replace("%2F", "/").replace("%2f", "/"))
}

/// A sha or a branch/tag name: nothing git would read as an option, a range
/// or more than one revision.
fn is_revision(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with(['-', '/', '.'])
        && !value.ends_with(['/', '.'])
        && !value.contains("..")
        && !value.contains("//")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_records_render_in_the_github_commit_shape() {
        let record = "abc\u{1f}p1 p2\u{1f}Ann\u{1f}a@x\u{1f}2026-09-01T00:00:00+00:00\u{1f}Cid\u{1f}c@x\u{1f}2026-09-02T00:00:00+00:00\u{1f}feat: one\n\nbody\n";
        let commit = commit_json("alice", "svc", record).expect("commit");
        assert_eq!(commit["sha"], "abc");
        assert_eq!(commit["commit"]["message"], "feat: one\n\nbody");
        assert_eq!(commit["commit"]["author"]["name"], "Ann");
        assert_eq!(
            commit["commit"]["committer"]["date"],
            "2026-09-02T00:00:00+00:00"
        );
        assert_eq!(commit["parents"][1]["sha"], "p2");
        assert!(commit_json("alice", "svc", "").is_none());
    }

    #[test]
    fn sha_query_is_read_and_checked() {
        assert_eq!(
            query_value("per_page=2&sha=feature%2Fx", "sha").as_deref(),
            Some("feature/x")
        );
        assert!(is_revision("feature/x"));
        assert!(!is_revision("--output=/tmp/x"));
        assert!(!is_revision("main..evil"));
    }
}
