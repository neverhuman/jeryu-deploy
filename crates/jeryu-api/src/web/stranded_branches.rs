//! Branches carrying work that no open pull request carries.
//!
//! This is broader than `pushed_after_merge`: every non-default branch is
//! compared with the repository's default branch by patch identity. If all of
//! its commits are patch-equivalent to default, it is already landed even when
//! the commit ids differ.

use chrono::{DateTime, Utc};
use jeryu_core::{PullRequest, PullRequestState};

use super::WebState;

const MAX_SCAN_REPOS: usize = 32;
const MAX_SCAN_BRANCHES: usize = 64;
const MAX_BRANCHES_PER_REPO: usize = 16;
const SCAN_WINDOW_SECONDS: i64 = 60;
pub(crate) const COVERAGE_NOTICE: &str = "Branch discovery rotates through a bounded sample once per minute; a missing row does not establish that its work was resolved.";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StrandedBranch {
    /// `owner/name`.
    pub(crate) repo: String,
    pub(crate) branch: String,
    pub(crate) default_branch: String,
    pub(crate) commit_count: usize,
    pub(crate) newest_sha: String,
    pub(crate) newest_subject: String,
    pub(crate) newest_at: Option<DateTime<Utc>>,
    pub(crate) closed_pr: Option<u64>,
}

fn is_open(pull: &PullRequest) -> bool {
    !pull.merged
        && !matches!(
            pull.state,
            PullRequestState::Merged | PullRequestState::Closed
        )
}

fn is_same_repo_pull(pull: &PullRequest, full_name: &str) -> bool {
    pull.source_repository == full_name || pull.source_repository.trim().is_empty()
}

fn open_pull_carries_branch(pulls: &[PullRequest], full_name: &str, branch: &str) -> bool {
    pulls.iter().any(|pull| {
        is_open(pull) && is_same_repo_pull(pull, full_name) && pull.head.ref_name == branch
    })
}

fn latest_closed_pull(pulls: &[PullRequest], full_name: &str, branch: &str) -> Option<u64> {
    pulls
        .iter()
        .filter(|pull| {
            !is_open(pull) && is_same_repo_pull(pull, full_name) && pull.head.ref_name == branch
        })
        .max_by_key(|pull| pull.merged_at.unwrap_or(pull.updated_at))
        .map(|pull| pull.number)
}

fn patch_unique_commits(
    state: &WebState,
    owner: &str,
    name: &str,
    default_branch: &str,
    branch: &str,
) -> Vec<(String, String, Option<DateTime<Utc>>)> {
    let Ok(opened) = state.repo_manager.open_parts(owner, name) else {
        return Vec::new();
    };
    let git_bin = &state.repo_manager.config().git_bin;
    let range = format!("refs/heads/{default_branch}...refs/heads/{branch}");
    let Ok(out) = super::shift::run_git(
        git_bin,
        opened.path.as_path(),
        &[
            "log",
            "--cherry-pick",
            "--right-only",
            "--max-count=200",
            "--format=%H%x00%cI%x00%s",
            "--end-of-options",
            &range,
        ],
        &[],
        None,
    ) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\0');
            let sha = fields.next()?.trim();
            let committed = fields.next()?.trim();
            let subject = fields.next()?.trim();
            if sha.is_empty() || subject.is_empty() {
                return None;
            }
            let committed = DateTime::parse_from_rfc3339(committed)
                .ok()
                .map(|time| time.with_timezone(&Utc));
            Some((
                sha.to_string(),
                subject
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(500)
                    .collect(),
                committed,
            ))
        })
        .collect()
}

pub(crate) fn all_at_time(state: &WebState, now: DateTime<Utc>) -> Vec<StrandedBranch> {
    // A window's magnitude is always representable, including before the
    // Unix epoch. Keep it wide until an index is bounded by its collection.
    let round = now
        .timestamp()
        .div_euclid(SCAN_WINDOW_SECONDS)
        .unsigned_abs();
    all_at_round(state, round)
}

// Partial scans rotate so large forges remain responsive without permanently
// hiding branches beyond the budget. A skipped branch is never mutated.
fn all_at_round(state: &WebState, round: u64) -> Vec<StrandedBranch> {
    let mut stranded = Vec::new();
    let mut repos: Vec<_> = state
        .core
        .list_repositories(None)
        .into_iter()
        .filter(|repo| !repo.archived)
        .collect();
    repos.sort_by(|a, b| a.full_name.cmp(&b.full_name));
    let repo_count = repos.len();
    let repo_count_wide = repo_count as u64;
    let mut scanned_branches = 0;
    for offset in 0..repo_count.min(MAX_SCAN_REPOS) {
        let start = (round % repo_count_wide) as usize;
        let repo = &repos[(start + offset) % repo_count];
        if scanned_branches == MAX_SCAN_BRANCHES {
            break;
        }
        let Ok(resolved) = state.repo_manager.resolve_parts(&repo.owner, &repo.name) else {
            continue;
        };
        let Ok(refs) =
            jeryu_gitd::refs::RefService::new((*state.repo_manager).clone()).list_refs(&resolved)
        else {
            continue;
        };
        let Ok(pulls) = state.core.list_pull_requests(&repo.owner, &repo.name, None) else {
            continue;
        };
        let mut branches: Vec<_> = refs
            .iter()
            .filter_map(|git_ref| git_ref.name.strip_prefix("refs/heads/"))
            .filter(|branch| {
                *branch != repo.default_branch
                    && !open_pull_carries_branch(&pulls, &repo.full_name, branch)
            })
            .collect();
        branches.sort_unstable();
        let branch_count = branches.len();
        for offset in 0..branch_count
            .min(MAX_BRANCHES_PER_REPO)
            .min(MAX_SCAN_BRANCHES - scanned_branches)
        {
            // Advance even when a repository is revisited at a fixed period;
            // using round * budget would starve branches at matching periods.
            let start = ((round / repo_count_wide)
                .wrapping_mul(MAX_BRANCHES_PER_REPO as u64)
                .wrapping_add(round % repo_count_wide)
                % branch_count as u64) as usize;
            let branch = branches[(start + offset) % branch_count];
            scanned_branches += 1;
            let commits =
                patch_unique_commits(state, &repo.owner, &repo.name, &repo.default_branch, branch);
            let Some((newest_sha, newest_subject, newest_at)) = commits.first().cloned() else {
                continue;
            };
            stranded.push(StrandedBranch {
                repo: repo.full_name.clone(),
                branch: branch.to_string(),
                default_branch: repo.default_branch.clone(),
                commit_count: commits.len(),
                newest_sha,
                newest_subject,
                newest_at,
                closed_pr: latest_closed_pull(&pulls, &repo.full_name, branch),
            });
        }
    }
    stranded
}

#[cfg(test)]
#[path = "stranded_branches_tests.rs"]
mod tests;
