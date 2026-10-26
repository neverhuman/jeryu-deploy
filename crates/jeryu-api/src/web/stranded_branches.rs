//! Branches carrying work that no open pull request carries.
//!
//! This is broader than `pushed_after_merge`: every non-default branch is
//! compared with the repository's default branch by patch identity. If all of
//! its commits are patch-equivalent to default, it is already landed even when
//! the commit ids differ.

use std::collections::{BTreeMap, BTreeSet};

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
    /// When the scan that produced this row looked at the branch.
    pub(crate) scanned_at: Option<DateTime<Utc>>,
}

/// One repository's kept scan result: the stranded rows by branch name.
#[derive(Debug, Default)]
pub(crate) struct RepoScan {
    rows: BTreeMap<String, StrandedBranch>,
}

/// The last result per repository (`owner/name`). A scan pass reaches only a
/// rotating subset, so a repository outside this pass keeps what its last pass
/// found; the budget decides how fresh a row is, not whether it shows.
pub(crate) type ScanCache = BTreeMap<String, RepoScan>;

/// What one pass learned about one repository.
struct RepoPass {
    full_name: String,
    /// Every branch that could be stranded: not default, custody or carried by
    /// an open pull request. A kept row for any other branch is stale.
    candidates: BTreeSet<String>,
    /// The branches this pass assessed, with the verdict for each.
    assessed: Vec<(String, Option<StrandedBranch>)>,
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

/// Commits of the default branch read for `Todo:` trailers. One bounded walk
/// per repository serves every branch assessed with it; an id older than the
/// walk counts as not landed, so the branch shows rather than hides.
const MAX_DEFAULT_COMMITS_FOR_TODOS: usize = 20_000;

/// Every `Todo:` trailer id on the default branch, read in one `git log`.
fn landed_todo_ids(
    state: &WebState,
    owner: &str,
    name: &str,
    default_branch: &str,
) -> BTreeSet<String> {
    let tip = format!("refs/heads/{default_branch}");
    let max_count = format!("--max-count={MAX_DEFAULT_COMMITS_FOR_TODOS}");
    let Some(out) = git_output(
        state,
        owner,
        name,
        &[
            "log",
            &max_count,
            "--format=%(trailers:key=Todo,valueonly,separator=%x20)",
            "--end-of-options",
            &tip,
        ],
    ) else {
        return BTreeSet::new();
    };
    out.split_whitespace()
        .filter(|id| valid_todo_id(id))
        .map(str::to_string)
        .collect()
}

/// What every branch of one repository is assessed against, read once.
pub(crate) struct RepoContext<'a> {
    repo: &'a Repository,
    pulls: Vec<PullRequest>,
    landed_todos: std::cell::OnceCell<BTreeSet<String>>,
}

impl<'a> RepoContext<'a> {
    fn new(repo: &'a Repository, pulls: Vec<PullRequest>) -> Self {
        Self {
            repo,
            pulls,
            landed_todos: std::cell::OnceCell::new(),
        }
    }

    fn todo_landed(&self, state: &WebState, id: &str) -> bool {
        self.landed_todos
            .get_or_init(|| {
                landed_todo_ids(
                    state,
                    &self.repo.owner,
                    &self.repo.name,
                    &self.repo.default_branch,
                )
            })
            .contains(id)
    }
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

/// A pull request somebody closed unmerged: its head as the forge recorded it.
/// The forge stops moving a pull request's head once it is closed, so this is
/// what the person who closed it saw.
fn closed_unmerged_head(pulls: &[PullRequest], number: u64) -> Option<&PullRequest> {
    pulls
        .iter()
        .find(|pull| pull.number == number)
        .filter(|pull| !pull.merged && !matches!(pull.state, PullRequestState::Merged))
}

/// Whether anything was pushed to the branch after its pull request closed:
/// a commit on the branch that the closed head does not hold. The recorded
/// head decides, not commit dates, which whoever pushes can set. When the
/// closed head is no longer in the repository, fall back to dates.
fn pushed_after_close(
    state: &WebState,
    repo: &Repository,
    branch: &str,
    closed: &PullRequest,
    newest_at: Option<DateTime<Utc>>,
) -> bool {
    let head = closed.head.sha.trim();
    if !head.is_empty() && head.chars().all(|ch| ch.is_ascii_hexdigit()) {
        let range = format!("{head}..refs/heads/{branch}");
        if let Some(count) = git_output(
            state,
            &repo.owner,
            &repo.name,
            &["rev-list", "--count", "--end-of-options", &range],
        )
        .and_then(|out| out.trim().parse::<u64>().ok())
        {
            return count > 0;
        }
    }
    newest_at.is_none_or(|pushed_at| pushed_at > closed.updated_at)
}

/// Everything that decides whether one branch is stranded work.
fn assess_branch(
    state: &WebState,
    context: &RepoContext<'_>,
    branch: &str,
) -> Option<StrandedBranch> {
    let repo = context.repo;
    let pulls = context.pulls.as_slice();
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
    commits.retain(|commit| {
        let landed = !commit.todos.is_empty()
            && commit.todos.iter().all(|id| context.todo_landed(state, id));
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
    if let Some(closed) = closed_pr.and_then(|number| closed_unmerged_head(pulls, number))
        && !pushed_after_close(state, repo, branch, closed, newest.at)
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
        scanned_at: None,
    })
}

/// The stranded rows of every repository: this pass's findings for the
/// repositories it reached, and each other repository's last findings.
pub(crate) fn all_at_time(state: &WebState, now: DateTime<Utc>) -> Vec<StrandedBranch> {
    // A window's magnitude is always representable, including before the
    // Unix epoch. Keep it wide until an index is bounded by its collection.
    let round = now
        .timestamp()
        .div_euclid(SCAN_WINDOW_SECONDS)
        .unsigned_abs();
    // Stamp the window, not the instant, so a window's answer repeats.
    let window_start = DateTime::from_timestamp(
        now.timestamp().div_euclid(SCAN_WINDOW_SECONDS) * SCAN_WINDOW_SECONDS,
        0,
    )
    .unwrap_or(now);
    let passes = scan_round(state, round);
    let mut cache = state
        .stranded_scans
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    merge_passes(&mut cache, passes, window_start);
    // Forget repositories that are gone or archived so the cache stays bounded.
    let live: BTreeSet<String> = state
        .core
        .list_repositories(None)
        .into_iter()
        .filter(|repo| !repo.archived)
        .map(|repo| repo.full_name)
        .collect();
    cache.retain(|name, _| live.contains(name));
    cache
        .values()
        .flat_map(|scan| scan.rows.values().cloned())
        .collect()
}

fn merge_passes(cache: &mut ScanCache, passes: Vec<RepoPass>, now: DateTime<Utc>) {
    for pass in passes {
        let kept = cache.entry(pass.full_name).or_default();
        kept.rows
            .retain(|branch, _| pass.candidates.contains(branch));
        for (branch, verdict) in pass.assessed {
            match verdict {
                Some(mut row) => {
                    row.scanned_at = Some(now);
                    kept.rows.insert(branch, row);
                }
                None => {
                    kept.rows.remove(&branch);
                }
            }
        }
    }
}

#[cfg(test)]
fn all_at_round(state: &WebState, round: u64) -> Vec<StrandedBranch> {
    scan_round(state, round)
        .into_iter()
        .flat_map(|pass| pass.assessed)
        .filter_map(|(_, row)| row)
        .collect()
}

// Partial scans rotate so large forges remain responsive without permanently
// hiding branches beyond the budget. A skipped branch is never mutated.
fn scan_round(state: &WebState, round: u64) -> Vec<RepoPass> {
    let mut passes = Vec::new();
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
    let custody = custody_prefixes(state);
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
        let branches = candidate_branches(&refs, repo, &pulls, &custody);
        let context = RepoContext::new(repo, pulls);
        let branch_count = branches.len();
        let mut pass = RepoPass {
            full_name: repo.full_name.clone(),
            candidates: branches.iter().map(|branch| branch.to_string()).collect(),
            assessed: Vec::new(),
        };
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
            pass.assessed
                .push((branch.to_string(), assess_branch(state, &context, branch)));
        }
        passes.push(pass);
    }
    passes
}

/// The branches of one repository that could be stranded: not the default
/// branch, not a custody ref, and not carried by an open pull request.
fn candidate_branches<'r>(
    refs: &'r [jeryu_gitd::refs::GitRef],
    repo: &Repository,
    pulls: &[PullRequest],
    custody: &[String],
) -> Vec<&'r str> {
    let mut branches: Vec<_> = refs
        .iter()
        .filter_map(|git_ref| git_ref.name.strip_prefix("refs/heads/"))
        .filter(|branch| {
            *branch != repo.default_branch
                && !is_custody_branch(custody, branch)
                && !open_pull_carries_branch(pulls, &repo.full_name, branch)
        })
        .collect();
    branches.sort_unstable();
    branches
}

/// Branches one listing assesses at most, in name order.
const MAX_LISTED_BRANCHES: usize = 100;
/// Wall time one listing may spend assessing branches before it answers with
/// what it has.
const LIST_TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
/// How long a listing is reused before the next request recomputes it.
const LIST_CACHE_FOR: std::time::Duration = std::time::Duration::from_secs(60);
/// Repositories whose listing is kept at once; the stalest is dropped first.
const LIST_CACHE_REPOS: usize = 256;
/// Listings computed at once across the forge; further requests wait.
pub(crate) const LIST_CONCURRENCY: usize = 2;

/// One repository's stranded branches, as far as one bounded pass got.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Listing {
    /// Oldest first.
    pub(crate) rows: Vec<StrandedBranch>,
    /// Branches that could be stranded.
    pub(crate) candidates: usize,
    /// Branches this pass assessed, the first ones in name order.
    pub(crate) examined: usize,
}

impl Listing {
    fn complete(&self) -> bool {
        self.examined == self.candidates
    }
}

/// Kept listings by repository (`owner/name`), each with when it was made.
pub(crate) type ListCache = BTreeMap<String, (std::time::Instant, std::sync::Arc<Listing>)>;

/// Stranded branches of one repository: at most [`MAX_LISTED_BRANCHES`]
/// assessed in name order, stopping early once [`LIST_TIME_BUDGET`] is spent.
/// Blocking: it runs git for every branch assessed.
pub(crate) fn listing(state: &WebState, repo: &Repository) -> Option<Listing> {
    let started = std::time::Instant::now();
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
    let branches = candidate_branches(&refs, repo, &pulls, &custody);
    let context = RepoContext::new(repo, pulls);
    let mut rows = Vec::new();
    let mut examined = 0;
    for branch in branches.iter().take(MAX_LISTED_BRANCHES) {
        if started.elapsed() >= LIST_TIME_BUDGET {
            break;
        }
        examined += 1;
        rows.extend(assess_branch(state, &context, branch));
    }
    rows.sort_by_key(|row| row.newest_at);
    Some(Listing {
        rows,
        candidates: branches.len(),
        examined,
    })
}

#[cfg(test)]
pub(crate) fn for_repository(state: &WebState, repo: &Repository) -> Option<Vec<StrandedBranch>> {
    listing(state, repo).map(|listing| listing.rows)
}

fn kept_listing(state: &WebState, full_name: &str) -> Option<std::sync::Arc<Listing>> {
    state
        .stranded_lists
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(full_name)
        .filter(|(at, _)| at.elapsed() < LIST_CACHE_FOR)
        .map(|(_, listing)| listing.clone())
}

fn keep_listing(state: &WebState, full_name: &str, listing: std::sync::Arc<Listing>) {
    let mut cache = state
        .stranded_lists
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|_, (at, _)| at.elapsed() < LIST_CACHE_FOR);
    while cache.len() >= LIST_CACHE_REPOS {
        let Some(stalest) = cache
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(name, _)| name.clone())
        else {
            break;
        };
        cache.remove(&stalest);
    }
    cache.insert(full_name.to_string(), (std::time::Instant::now(), listing));
}

/// The kept listing while it is fresh, else a new one made off the async
/// workers, at most [`LIST_CONCURRENCY`] at a time across the forge.
async fn cached_listing(
    state: &std::sync::Arc<WebState>,
    repo: Repository,
) -> Result<Option<std::sync::Arc<Listing>>, String> {
    if let Some(kept) = kept_listing(state, &repo.full_name) {
        return Ok(Some(kept));
    }
    let _permit = state
        .stranded_list_permits
        .clone()
        .acquire_owned()
        .await
        .map_err(|error| error.to_string())?;
    // Another request may have made it while this one waited.
    if let Some(kept) = kept_listing(state, &repo.full_name) {
        return Ok(Some(kept));
    }
    let worker = state.clone();
    let made = tokio::task::spawn_blocking(move || {
        let made = listing(&worker, &repo).map(std::sync::Arc::new);
        if let Some(made) = &made {
            keep_listing(&worker, &repo.full_name, made.clone());
        }
        made
    })
    .await
    .map_err(|error| error.to_string())?;
    Ok(made)
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
/// row, for a caller who may read the repository. The answer is bounded and
/// kept for a minute; `complete` says whether every candidate was assessed.
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
    let (full_name, default_branch) = (repo.full_name.clone(), repo.default_branch.clone());
    let Ok(Some(listing)) = cached_listing(&state, repo).await else {
        return super::api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            "could not read the repository's branches",
        );
    };
    let items: Vec<_> = listing
        .rows
        .iter()
        .map(|row| ListedBranch {
            branch: row.branch.clone(),
            commit_count: row.commit_count,
            newest_sha: row.newest_sha.clone(),
            newest_subject: row.newest_subject.clone(),
            newest_at: row
                .newest_at
                .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            closed_pr: row.closed_pr,
        })
        .collect();
    axum::Json(serde_json::json!({
        "repo": full_name,
        "default_branch": default_branch,
        "total": items.len(),
        "items": items,
        "candidates": listing.candidates,
        "examined": listing.examined,
        "complete": listing.complete(),
    }))
    .into_response()
}

#[cfg(test)]
#[path = "stranded_branches_tests.rs"]
mod tests;
