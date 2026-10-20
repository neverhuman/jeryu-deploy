//! Branches carrying work that no open pull request carries.
//!
//! This is broader than `pushed_after_merge`: every non-default branch is
//! compared with the repository's default branch by patch identity. If all of
//! its commits are patch-equivalent to default, it is already landed even when
//! the commit ids differ.

use chrono::{DateTime, Utc};
use jeryu_core::{PullRequest, PullRequestState, Repository};

use super::WebState;

const MAX_SCAN_REPOS: usize = 32;
const MAX_SCAN_BRANCHES: usize = 64;
const MAX_BRANCHES_PER_REPO: usize = 16;
const SCAN_WINDOW_SECONDS: i64 = 60;
/// Changed files compared against the default branch before a branch is
/// treated as not landed without looking further.
const MAX_COMPARED_FILES: usize = 200;
/// Site setting naming the branch prefixes that are custody refs, one per line.
pub(crate) const CUSTODY_PREFIXES_KEY: &str = "stranded_custody_prefixes";
/// Custody prefixes used when the site setting is unset.
pub(crate) const DEFAULT_CUSTODY_PREFIXES: &[&str] = &[
    "preserve/",
    "backup/",
    "mirror/",
    "todoq-kept/",
    "todoq-rescue/",
];
/// A branch whose tip commit starts with one of these keeps work on purpose.
const CUSTODY_SUBJECTS: &[&str] = &["GOV-005 custody commit", "custody:", "preserve("];
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

/// A commit on a branch that default does not have by patch identity.
#[derive(Clone, Debug)]
struct UniqueCommit {
    sha: String,
    subject: String,
    at: Option<DateTime<Utc>>,
    /// `Todo:` trailer values, so a landed todo can vouch for the commit.
    todos: Vec<String>,
}

/// The branch prefixes that mark custody refs: the site setting, one prefix
/// per line, or the defaults when it is unset, empty or unreadable.
pub(crate) fn custody_prefixes(state: &WebState) -> Vec<String> {
    let stored = state
        .site_settings
        .get(CUSTODY_PREFIXES_KEY)
        .ok()
        .flatten()
        .map(|setting| parse_prefixes(&setting.value))
        .unwrap_or_default();
    if stored.is_empty() {
        DEFAULT_CUSTODY_PREFIXES
            .iter()
            .map(|prefix| prefix.to_string())
            .collect()
    } else {
        stored
    }
}

pub(crate) fn parse_prefixes(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_custody_branch(prefixes: &[String], branch: &str) -> bool {
    prefixes
        .iter()
        .any(|prefix| branch.starts_with(prefix.as_str()))
}

fn is_custody_subject(subject: &str) -> bool {
    let subject = subject.trim_start();
    CUSTODY_SUBJECTS
        .iter()
        .any(|marker| subject.starts_with(marker))
}

fn git_output(state: &WebState, owner: &str, name: &str, args: &[&str]) -> Option<String> {
    let opened = state.repo_manager.open_parts(owner, name).ok()?;
    let git_bin = &state.repo_manager.config().git_bin;
    let out = super::shift::run_git(git_bin, opened.path.as_path(), args, &[], None).ok()?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn patch_unique_commits(
    state: &WebState,
    owner: &str,
    name: &str,
    default_branch: &str,
    branch: &str,
) -> Vec<UniqueCommit> {
    let range = format!("refs/heads/{default_branch}...refs/heads/{branch}");
    let Some(out) = git_output(
        state,
        owner,
        name,
        &[
            "log",
            "--cherry-pick",
            "--right-only",
            "--max-count=200",
            "--format=%x01%H%x00%cI%x00%s%x00%(trailers:key=Todo,valueonly,separator=%x20)",
            "--end-of-options",
            &range,
        ],
    ) else {
        return Vec::new();
    };
    out.split('\u{1}')
        .filter_map(|record| {
            let mut fields = record.splitn(4, '\0');
            let sha = fields.next()?.trim();
            let committed = fields.next()?.trim();
            let subject = fields.next()?.trim();
            let todos = fields.next().unwrap_or_default();
            if sha.is_empty() || subject.is_empty() {
                return None;
            }
            let at = DateTime::parse_from_rfc3339(committed)
                .ok()
                .map(|time| time.with_timezone(&Utc));
            Some(UniqueCommit {
                sha: sha.to_string(),
                subject: subject
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(500)
                    .collect(),
                at,
                todos: todos
                    .split_whitespace()
                    .filter(|id| valid_todo_id(id))
                    .take(8)
                    .map(str::to_string)
                    .collect(),
            })
        })
        .collect()
}

fn valid_todo_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

/// Whether the default branch carries a `Todo: <id>` trailer line.
fn todo_on_default(
    state: &WebState,
    owner: &str,
    name: &str,
    default_branch: &str,
    id: &str,
) -> bool {
    let pattern = format!(
        "--grep=^Todo:[[:space:]]+{}[[:space:]]*$",
        id.replace('.', "[.]")
    );
    let tip = format!("refs/heads/{default_branch}");
    git_output(
        state,
        owner,
        name,
        &[
            "log",
            "--max-count=1",
            "--extended-regexp",
            "--format=%H",
            &pattern,
            "--end-of-options",
            &tip,
        ],
    )
    .is_some_and(|out| !out.trim().is_empty())
}

/// Whether the branch's tip is a custody commit.
fn tip_is_custody(state: &WebState, owner: &str, name: &str, branch: &str) -> bool {
    let tip = format!("refs/heads/{branch}");
    git_output(
        state,
        owner,
        name,
        &[
            "log",
            "--max-count=1",
            "--format=%s",
            "--end-of-options",
            &tip,
        ],
    )
    .is_some_and(|subject| is_custody_subject(&subject))
}

/// A squash merge leaves different commits but the same content: true when
/// every file the branch changed since it left default is identical on both.
fn content_already_on_default(
    state: &WebState,
    owner: &str,
    name: &str,
    default_branch: &str,
    branch: &str,
) -> bool {
    let default_ref = format!("refs/heads/{default_branch}");
    let branch_ref = format!("refs/heads/{branch}");
    let Some(base) = git_output(
        state,
        owner,
        name,
        &["merge-base", "--end-of-options", &default_ref, &branch_ref],
    ) else {
        return false;
    };
    let base = base.trim();
    if base.is_empty() || !base.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return false;
    }
    let Some(changed) = git_output(
        state,
        owner,
        name,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            base,
            &branch_ref,
            "--",
        ],
    ) else {
        return false;
    };
    let files: Vec<&str> = changed
        .split('\0')
        .filter(|file| !file.is_empty())
        .collect();
    if files.len() > MAX_COMPARED_FILES {
        return false;
    }
    if files.is_empty() {
        return true;
    }
    let mut args = vec![
        "--literal-pathspecs",
        "diff",
        "--name-only",
        "--no-renames",
        &branch_ref,
        &default_ref,
        "--",
    ];
    args.extend(files);
    git_output(state, owner, name, &args).is_some_and(|differing| differing.trim().is_empty())
}

/// Whether the closed pull request on this branch was replaced by one that
/// merged, directly or down the recorded chain.
fn replacement_merged(
    state: &WebState,
    pulls: &[PullRequest],
    owner: &str,
    name: &str,
    closed_pr: u64,
) -> bool {
    let replacements = state.github.pull_replacements();
    let merged = |number: u64| {
        pulls.iter().any(|pull| {
            pull.number == number && (pull.merged || matches!(pull.state, PullRequestState::Merged))
        })
    };
    replacements
        .superseded_by(owner, name, closed_pr)
        .is_some_and(|link| merged(link.number))
        || replacements
            .chain_end(owner, name, closed_pr)
            .is_some_and(merged)
}

/// A pull request somebody closed unmerged, as of when it closed.
fn closed_unmerged_at(pulls: &[PullRequest], number: u64) -> Option<DateTime<Utc>> {
    pulls
        .iter()
        .find(|pull| pull.number == number)
        .filter(|pull| !pull.merged && !matches!(pull.state, PullRequestState::Merged))
        .map(|pull| pull.updated_at)
}

/// Everything that decides whether one branch is stranded work.
fn assess_branch(
    state: &WebState,
    repo: &Repository,
    pulls: &[PullRequest],
    branch: &str,
) -> Option<StrandedBranch> {
    let mut commits =
        patch_unique_commits(state, &repo.owner, &repo.name, &repo.default_branch, branch);
    if commits.is_empty() {
        return None;
    }
    let closed_pr = latest_closed_pull(pulls, &repo.full_name, branch);
    if closed_pr
        .is_some_and(|number| replacement_merged(state, pulls, &repo.owner, &repo.name, number))
    {
        return None;
    }
    // A commit whose todo already landed on default is landed work.
    let mut landed_todos = std::collections::BTreeMap::new();
    commits.retain(|commit| {
        let landed = !commit.todos.is_empty()
            && commit.todos.iter().all(|id| {
                *landed_todos.entry(id.clone()).or_insert_with(|| {
                    todo_on_default(state, &repo.owner, &repo.name, &repo.default_branch, id)
                })
            });
        !landed
    });
    let newest = commits.first().cloned()?;
    if tip_is_custody(state, &repo.owner, &repo.name, branch)
        || content_already_on_default(state, &repo.owner, &repo.name, &repo.default_branch, branch)
    {
        return None;
    }
    // A person who closed a pull request meant it: the branch is only
    // work again once something was pushed after the close.
    if let (Some(closed_at), Some(pushed_at)) = (
        closed_pr.and_then(|number| closed_unmerged_at(pulls, number)),
        newest.at,
    ) && pushed_at <= closed_at
    {
        return None;
    }
    Some(StrandedBranch {
        repo: repo.full_name.clone(),
        branch: branch.to_string(),
        default_branch: repo.default_branch.clone(),
        commit_count: commits.len(),
        newest_sha: newest.sha,
        newest_subject: newest.subject,
        newest_at: newest.at,
        closed_pr,
    })
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
        let custody = custody_prefixes(state);
        let mut branches: Vec<_> = refs
            .iter()
            .filter_map(|git_ref| git_ref.name.strip_prefix("refs/heads/"))
            .filter(|branch| {
                *branch != repo.default_branch
                    && !is_custody_branch(&custody, branch)
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
            if let Some(row) = assess_branch(state, repo, &pulls, branch) {
                stranded.push(row);
            }
        }
    }
    stranded
}

/// Branches listed for one repository; the sweep is bounded, so is the list.
const MAX_LISTED_BRANCHES: usize = 200;

/// Every stranded branch of one repository, oldest first, up to
/// [`MAX_LISTED_BRANCHES`] examined in name order.
pub(crate) fn for_repository(state: &WebState, repo: &Repository) -> Option<Vec<StrandedBranch>> {
    let resolved = state
        .repo_manager
        .resolve_parts(&repo.owner, &repo.name)
        .ok()?;
    let refs = jeryu_gitd::refs::RefService::new((*state.repo_manager).clone())
        .list_refs(&resolved)
        .ok()?;
    let pulls = state
        .core
        .list_pull_requests(&repo.owner, &repo.name, None)
        .ok()?;
    let custody = custody_prefixes(state);
    let mut branches: Vec<_> = refs
        .iter()
        .filter_map(|git_ref| git_ref.name.strip_prefix("refs/heads/"))
        .filter(|branch| {
            *branch != repo.default_branch
                && !is_custody_branch(&custody, branch)
                && !open_pull_carries_branch(&pulls, &repo.full_name, branch)
        })
        .collect();
    branches.sort_unstable();
    let mut rows: Vec<_> = branches
        .into_iter()
        .take(MAX_LISTED_BRANCHES)
        .filter_map(|branch| assess_branch(state, repo, &pulls, branch))
        .collect();
    rows.sort_by_key(|row| row.newest_at);
    Some(rows)
}

#[derive(serde::Serialize)]
struct ListedBranch {
    branch: String,
    commit_count: usize,
    newest_sha: String,
    newest_subject: String,
    newest_at: Option<String>,
    closed_pr: Option<u64>,
}

/// `GET /api/v1/repos/:id/stranded-branches`: the branches behind a Needs you
/// row, for a caller who may read the repository.
pub(crate) async fn list(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<WebState>>,
    axum::extract::Extension(account): axum::extract::Extension<jeryu_core::AccountSummary>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let readable = super::repositories::find_repo(&state, &id).filter(|repo| {
        account.role == jeryu_core::UserRole::Admin
            || !repo.private
            || state
                .core
                .user_can_read_repo(&account.login, &repo.owner, &repo.name)
    });
    let Some(repo) = readable else {
        return super::api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    let Some(rows) = for_repository(&state, &repo) else {
        return super::api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            "could not read the repository's branches",
        );
    };
    let items: Vec<_> = rows
        .into_iter()
        .map(|row| ListedBranch {
            branch: row.branch,
            commit_count: row.commit_count,
            newest_sha: row.newest_sha,
            newest_subject: row.newest_subject,
            newest_at: row
                .newest_at
                .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            closed_pr: row.closed_pr,
        })
        .collect();
    axum::Json(serde_json::json!({
        "repo": repo.full_name,
        "default_branch": repo.default_branch,
        "items": items,
        "total": items.len(),
    }))
    .into_response()
}

#[cfg(test)]
#[path = "stranded_branches_tests.rs"]
mod tests;
