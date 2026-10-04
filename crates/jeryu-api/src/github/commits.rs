//! `GET /repos/{owner}/{repo}/commits?sha=&per_page=&page=`: a branch's commit
//! history, newest first, read from the bare repository the forge serves.
//!
//! The same git plumbing serves `GET /repos/{owner}/{repo}/pulls/{n}/commits`
//! (see [`GithubRouter::list_pull_commits`]), which walks the PR's own range
//! instead of a whole branch.
//!
//! `sha` names a branch, tag or commit and defaults to the repository's default
//! branch, as on GitHub. Pages are cut by git itself (`--skip`/`--max-count`),
//! so a long history is never read whole; `Link` carries the page relations.
//!
//! A commit list has one orderable field — the commit date the history is
//! walked by — so `?sort=` accepts only `created` and `?direction=` flips the
//! walk between newest-first (the default) and oldest-first. Any other value
//! of either is a 422 (see [`super::listing`]) instead of being ignored.

use serde_json::{Value, json};

use crate::routes::Response;

use super::GithubRouter;
use super::listing::{CommitListQuery, Direction};
use super::support::{Pagination, docs_url, error_response, json_response};

impl GithubRouter {
    pub(super) fn list_commits(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        page: Pagination,
        query: &str,
        list: CommitListQuery,
    ) -> Response {
        let repository = match self.core.get_repository(owner, repo) {
            Ok(repository) => repository,
            Err(err) => return error_response(err),
        };
        let requested = query_value(query, "sha").filter(|value| !value.is_empty());
        let reference = requested.unwrap_or_else(|| repository.default_branch.clone());
        if !is_revision(&reference) {
            return json_response(
                422,
                &json!({
                    "message": "sha must be a commit sha or a branch/tag name",
                    "documentation_url": docs_url(),
                }),
            );
        }
        // `path` is already the route plus the caller's own query (`?sha=`,
        // `?direction=`) with the page hints dropped, so every pagination link
        // keeps the filters without re-appending them.
        self.git_commits_page(owner, repo, &reference, path, page, list.direction)
    }

    /// The commits of one pull request, oldest first like GitHub: the range
    /// `merge-base(base, head)..head` of the shas RECORDED on the PR. Reading
    /// the recorded shas (never the live branch tips) keeps a merged PR's list
    /// exactly what the reviewer merged.
    pub(super) fn list_pull_commits(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
        path: &str,
        page: Pagination,
    ) -> Response {
        let number = match super::support::parse_number(number) {
            Ok(value) => value,
            Err(response) => return response,
        };
        // Same authorization as `GET /pulls/{number}`: a PR the caller cannot
        // read is a 404 here too, because that read is how we learn the range.
        let pr = match self.core.get_pull_request(owner, repo, number) {
            Ok(pr) => pr,
            Err(err) => return error_response(err),
        };
        self.git_range_page(owner, repo, &pr.base.sha, &pr.head.sha, path, page)
    }

    #[cfg(feature = "web")]
    fn git_commits_page(
        &self,
        owner: &str,
        repo: &str,
        reference: &str,
        base: &str,
        page: Pagination,
        direction: Direction,
    ) -> Response {
        let Some(git) = self.git_repo(owner, repo) else {
            return no_history();
        };
        let Some(head) = git.rev_parse(reference) else {
            return unknown_revision(reference);
        };
        let total = git.count(&[&head]);
        match direction {
            Direction::Desc => {
                let skip = page.page.saturating_sub(1).saturating_mul(page.per_page);
                let commits = git.log(
                    owner,
                    repo,
                    &[
                        &format!("--max-count={}", page.per_page),
                        &format!("--skip={skip}"),
                    ],
                    &[&head],
                );
                commits_response(base, page, total, commits)
            }
            // `?direction=asc` is the same history read oldest first, so the
            // page is cut from the newest end and only then reversed — the way
            // a pull request's own commit list is paged.
            Direction::Asc => self.oldest_first_page(owner, repo, &[&head], base, page, total),
        }
    }

    /// One page of `merge-base(base_sha, head_sha)..head_sha`, oldest first.
    ///
    /// `git log` limits (`--skip`/`--max-count`) are applied newest-first and
    /// only then reversed for output, so an oldest-first page is cut from the
    /// newest end: page 1 is the LAST `per_page` commits of the newest-first
    /// walk.
    #[cfg(feature = "web")]
    fn git_range_page(
        &self,
        owner: &str,
        repo: &str,
        base_sha: &str,
        head_sha: &str,
        link_base: &str,
        page: Pagination,
    ) -> Response {
        let Some(git) = self.git_repo(owner, repo) else {
            return commits_response(link_base, page, 0, Vec::new());
        };
        let Some(head) = git.rev_parse(head_sha) else {
            // A PR whose head is not (or no longer) in this repository has no
            // readable commits; an empty list beats inventing a 404 for a PR
            // the caller is allowed to see.
            return commits_response(link_base, page, 0, Vec::new());
        };
        // Without a resolvable base (a PR opened with a placeholder base sha)
        // the whole history of the head is the PR's range, as `git log head`.
        let range: Vec<String> = match git
            .rev_parse(base_sha)
            .and_then(|base| git.merge_base(&base, &head))
        {
            Some(merge_base) => vec![format!("^{merge_base}"), head.clone()],
            None => vec![head.clone()],
        };
        let range: Vec<&str> = range.iter().map(String::as_str).collect();
        let total = git.count(&range);
        self.oldest_first_page(owner, repo, &range, link_base, page, total)
    }

    /// One oldest-first page of `range`, whose `total` commits were already
    /// counted. `git log` applies its limits newest-first and only reverses for
    /// output, so the window is measured from the newest end: page 1 is the
    /// LAST `per_page` commits of the newest-first walk.
    #[cfg(feature = "web")]
    fn oldest_first_page(
        &self,
        owner: &str,
        repo: &str,
        range: &[&str],
        link_base: &str,
        page: Pagination,
        total: usize,
    ) -> Response {
        let Some(git) = self.git_repo(owner, repo) else {
            return commits_response(link_base, page, total, Vec::new());
        };
        let start = page.page.saturating_sub(1).saturating_mul(page.per_page);
        if start >= total {
            return commits_response(link_base, page, total, Vec::new());
        }
        let max_count = page.per_page.min(total - start);
        let skip = total - start - max_count;
        let commits = git.log(
            owner,
            repo,
            &[
                "--reverse",
                &format!("--max-count={max_count}"),
                &format!("--skip={skip}"),
            ],
            range,
        );
        commits_response(link_base, page, total, commits)
    }

    /// The count of commits in a pull request's range, for the `commits` field
    /// of the pull JSON. `None` when no git backend can answer.
    #[cfg(feature = "web")]
    pub(crate) fn pull_commit_count(&self, pr: &jeryu_core::PullRequest) -> Option<usize> {
        let git = self.git_repo(&pr.owner, &pr.repo)?;
        let head = git.rev_parse(&pr.head.sha)?;
        let range = match git
            .rev_parse(&pr.base.sha)
            .and_then(|base| git.merge_base(&base, &head))
        {
            Some(merge_base) => vec![format!("^{merge_base}"), head],
            None => vec![head],
        };
        Some(git.count(&range.iter().map(String::as_str).collect::<Vec<_>>()))
    }

    #[cfg(feature = "web")]
    fn git_repo(&self, owner: &str, repo: &str) -> Option<GitRepo> {
        let manager = self.repo_manager.as_ref()?;
        let resolved = manager.resolve_parts(owner, repo).ok()?;
        Some(GitRepo {
            git_bin: manager.config().git_bin.clone().into(),
            path: resolved.path,
        })
    }

    #[cfg(not(feature = "web"))]
    fn git_range_page(
        &self,
        _owner: &str,
        _repo: &str,
        _base_sha: &str,
        _head_sha: &str,
        link_base: &str,
        page: Pagination,
    ) -> Response {
        commits_response(link_base, page, 0, Vec::new())
    }

    #[cfg(not(feature = "web"))]
    fn git_commits_page(
        &self,
        _owner: &str,
        _repo: &str,
        _reference: &str,
        _base: &str,
        _page: Pagination,
        _direction: Direction,
    ) -> Response {
        no_history()
    }
}

/// The bare repository one page of history is read from.
#[cfg(feature = "web")]
struct GitRepo {
    git_bin: std::path::PathBuf,
    path: std::path::PathBuf,
}

#[cfg(feature = "web")]
impl GitRepo {
    /// Runs `git -C <repo> <args>`; `None` for a non-zero exit.
    fn run(&self, args: &[&str]) -> Option<String> {
        let out = std::process::Command::new(&self.git_bin)
            .arg("-C")
            .arg(&self.path)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn rev_parse(&self, reference: &str) -> Option<String> {
        self.run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ])
        .filter(|oid| !oid.is_empty())
    }

    fn merge_base(&self, base: &str, head: &str) -> Option<String> {
        self.run(&["merge-base", "--end-of-options", base, head])
            .filter(|oid| !oid.is_empty())
    }

    fn count(&self, range: &[&str]) -> usize {
        let mut args = vec!["rev-list", "--count", "--end-of-options"];
        args.extend_from_slice(range);
        self.run(&args)
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(0)
    }

    /// One `git log` window, rendered in the GitHub commit-list shape.
    fn log(&self, owner: &str, repo: &str, limits: &[&str], range: &[&str]) -> Vec<Value> {
        let mut args = vec!["log"];
        args.extend_from_slice(limits);
        args.push("--format=%H%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%cn%x1f%ce%x1f%cI%x1f%B%x1e");
        args.push("--end-of-options");
        args.extend_from_slice(range);
        self.run(&args)
            .unwrap_or_default()
            .split('\u{1e}')
            .filter_map(|record| commit_json(owner, repo, record.trim_start_matches('\n')))
            .collect()
    }
}

/// One page of commits plus the `Link` header the caller pages with.
fn commits_response(
    link_base: &str,
    page: Pagination,
    total: usize,
    commits: Vec<Value>,
) -> Response {
    let last_page = total.div_ceil(page.per_page).max(1);
    let mut response = json_response(200, &Value::Array(commits));
    if let Some(link) = super::support::link_header(link_base, page.per_page, page.page, last_page)
    {
        response.headers.push(("Link".to_owned(), link));
    }
    response
}

#[cfg_attr(not(feature = "web"), allow(dead_code))]
fn unknown_revision(reference: &str) -> Response {
    json_response(
        404,
        &json!({
            "message": format!("No commit found for SHA: {reference}"),
            "documentation_url": docs_url(),
        }),
    )
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
        "html_url": super::support::web_url(&format!("/repos/jeryu/{owner}/{repo}/commit/{sha}")),
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
