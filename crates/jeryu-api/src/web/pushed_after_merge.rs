//! A commit pushed to a branch whose pull request already merged.
//!
//! Such a commit reaches nothing: the pull request that owned the branch is
//! closed, so no open pull request carries it, no gate runs for it and no
//! merge ever lands it. A 2026-10-08 sweep of the forge found four of them
//! stranded for days. Once a merge deletes the head branch, the same push
//! quietly recreates the branch with no pull request at all.
//!
//! New pushes to a branch whose pull request already merged are refused by the
//! receive path before the branch can be recreated or advanced. Older stranded
//! commits are surfaced by the broader stranded-branch sweep in Needs you.
//!
//! Both read the same current state, so there is nothing to clear by hand: a
//! new pull request from the branch, or deleting the branch, takes the item
//! away on the next collection.

use chrono::{DateTime, Duration, Utc};
use jeryu_core::{PullRequest, PullRequestState};

use super::WebState;

/// How long after a pull request closed a push to its head branch still names
/// it. Past that the branch is its own line of work again, and whoever pushes
/// to it is not being surprised by a merge they forgot.
pub(crate) const LOOKBACK_DAYS: i64 = 30;
/// Commits one message or item names. A longer run says the same thing, and
/// the count beside them says how long it really is.
const MAX_COMMITS: usize = 10;
/// Commits counted at all. A branch this far from its base is not one stray
/// push, and the rule says the same thing either way.
const MAX_COUNTED_COMMITS: &str = "200";

/// A branch holding commits no pull request carries, and the closed pull
/// request whose head branch it is.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StrandedPush {
    /// `owner/name`.
    pub(crate) repo: String,
    /// The short branch name, as a push spells it.
    pub(crate) branch: String,
    pub(crate) number: u64,
    pub(crate) title: String,
    /// The branch the closed pull request would have landed on.
    pub(crate) base_ref: String,
    /// Whether that pull request merged; the other case is closed unmerged.
    pub(crate) merged: bool,
    /// When it closed, which is when the branch stopped carrying anything.
    pub(crate) closed_at: Option<DateTime<Utc>>,
    /// Abbreviated commits on the branch and not on the base, newest first,
    /// at most [`MAX_COMMITS`] of them.
    pub(crate) commits: Vec<String>,
    /// How many there are, which can be more than were named.
    pub(crate) commit_count: usize,
}

/// A branch the receive path must refuse because its pull request already
/// merged and no open pull request from that branch exists now.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MergedPullRequestBranch {
    pub(crate) branch: String,
    pub(crate) number: u64,
    pub(crate) head_sha: String,
}

/// A pull request is open while it is neither merged nor closed: only an open
/// one carries what arrives on its head branch. Every other state
/// (`BlockedByChecks`, `Queued`, `Mergeable`, ...) is a live pull request in
/// the middle of the flow.
fn same_repo(pull: &PullRequest, full_name: &str) -> bool {
    pull.source_repository.trim().is_empty() || pull.source_repository == full_name
}

fn is_open(pull: &PullRequest) -> bool {
    !pull.merged
        && !matches!(
            pull.state,
            PullRequestState::Merged | PullRequestState::Closed
        )
}

/// When a pull request stopped carrying its branch. A merge dates itself; one
/// somebody closed unmerged is dated by the change that closed it, which is
/// the last thing that happened to it.
fn closed_at(pull: &PullRequest) -> DateTime<Utc> {
    pull.merged_at.unwrap_or(pull.updated_at)
}

/// The merged pull request that owns `branch`, if a push to that branch would
/// land nowhere. A new open pull request from the branch makes the push live
/// work again and clears the refusal.
pub(crate) fn merged_pull_request_branch(
    state: &WebState,
    owner: &str,
    name: &str,
    branch: &str,
) -> Option<MergedPullRequestBranch> {
    merged_pull_request_branch_at(state, owner, name, branch, Utc::now())
}

fn merged_pull_request_branch_at(
    state: &WebState,
    owner: &str,
    name: &str,
    branch: &str,
    now: DateTime<Utc>,
) -> Option<MergedPullRequestBranch> {
    if state.core.get_repository(owner, name).ok()?.default_branch == branch {
        return None;
    }
    let pulls = state.core.list_pull_requests(owner, name, None).ok()?;
    let full_name = format!("{owner}/{name}");
    let from_branch = || {
        pulls
            .iter()
            .filter(|pull| pull.head.ref_name == branch && same_repo(pull, &full_name))
    };
    if from_branch().any(is_open) {
        return None;
    }
    let pull = from_branch()
        .filter(|pull| {
            (pull.merged || matches!(pull.state, PullRequestState::Merged))
                && closed_at(pull) >= now - Duration::days(LOOKBACK_DAYS)
        })
        .max_by_key(|pull| closed_at(pull))?;
    Some(MergedPullRequestBranch {
        branch: branch.to_string(),
        number: pull.number,
        head_sha: pull.head.sha.clone(),
    })
}

pub(crate) fn receive_refusal_message(branch: &MergedPullRequestBranch) -> String {
    format!(
        "this branch's pull request #{} was already merged; push to a new branch and open a new pull request",
        branch.number
    )
}

/// Commits on `branch` that `base_ref` does not have, newest first, and how
/// many there are. Bounded: one `rev-list` per branch that got this far.
fn commits_not_on_base(
    state: &WebState,
    owner: &str,
    name: &str,
    base_ref: &str,
    branch: &str,
) -> (Vec<String>, usize) {
    let Ok(opened) = state.repo_manager.open_parts(owner, name) else {
        return (Vec::new(), 0);
    };
    let git_bin = &state.repo_manager.config().git_bin;
    let range = format!("refs/heads/{base_ref}..refs/heads/{branch}");
    let Ok(out) = super::shift::run_git(
        git_bin,
        opened.path.as_path(),
        &[
            "rev-list",
            "--max-count",
            MAX_COUNTED_COMMITS,
            "--abbrev-commit",
            "--end-of-options",
            &range,
        ],
        &[],
        None,
    ) else {
        // A base the repository no longer has, or a branch deleted between the
        // listing and here: nothing to say either way.
        return (Vec::new(), 0);
    };
    let all: Vec<String> = String::from_utf8_lossy(&out)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    let total = all.len();
    (all.into_iter().take(MAX_COMMITS).collect(), total)
}

/// The pull request a push to `branch` is stranded behind, if it is stranded:
/// the newest one that closed within [`LOOKBACK_DAYS`] with the branch as its
/// head, while no pull request from the branch is open.
pub(crate) fn stranded(
    state: &WebState,
    owner: &str,
    name: &str,
    branch: &str,
    now: DateTime<Utc>,
) -> Option<StrandedPush> {
    let pulls = state.core.list_pull_requests(owner, name, None).ok()?;
    stranded_among(state, owner, name, &pulls, branch, now)
}

/// As [`stranded`], over pull requests already read: the inbox walks every
/// branch of a repository and reads its pull requests once.
fn stranded_among(
    state: &WebState,
    owner: &str,
    name: &str,
    pulls: &[PullRequest],
    branch: &str,
    now: DateTime<Utc>,
) -> Option<StrandedPush> {
    let full_name = format!("{owner}/{name}");
    let from_branch = || {
        pulls
            .iter()
            .filter(|pull| pull.head.ref_name == branch && same_repo(pull, &full_name))
    };
    // An open pull request from the branch carries whatever is on it: that is
    // what clears this, and it is asked first because it costs no git call.
    if from_branch().any(is_open) {
        return None;
    }
    let since = now - Duration::days(LOOKBACK_DAYS);
    let pull = from_branch()
        .filter(|pull| closed_at(pull) >= since)
        .max_by_key(|pull| closed_at(pull))?;
    let (commits, commit_count) =
        commits_not_on_base(state, owner, name, &pull.base.ref_name, branch);
    // The branch is on its base: the merge took everything, and a push that
    // only moved the branch onto what already landed stranded nothing.
    if commits.is_empty() {
        return None;
    }
    Some(StrandedPush {
        repo: format!("{owner}/{name}"),
        branch: branch.to_string(),
        number: pull.number,
        title: pull.title.clone(),
        base_ref: pull.base.ref_name.clone(),
        merged: pull.merged,
        closed_at: Some(closed_at(pull)),
        commits,
        commit_count,
    })
}

/// Every branch of the forge holding commits a closed pull request left
/// behind. Kept for receive-path tests; the production inbox uses the broader
/// stranded-branch sweep.
#[cfg(test)]
pub(crate) fn all(state: &WebState, now: DateTime<Utc>) -> Vec<StrandedPush> {
    let mut stranded_pushes = Vec::new();
    for repo in state.core.list_repositories(None) {
        // An archived repository is read-only: nothing is pushed to it and no
        // new pull request can be opened on it.
        if repo.archived {
            continue;
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
        for branch in refs
            .iter()
            .filter_map(|git_ref| git_ref.name.strip_prefix("refs/heads/"))
        {
            if let Some(row) = stranded_among(state, &repo.owner, &repo.name, &pulls, branch, now) {
                stranded_pushes.push(row);
            }
        }
    }
    stranded_pushes
}

/// What git prints back to the pushing client, one `remote:` line each. It
/// names the pull request, says it is already closed, and says what to do, so
/// an agent that reads only its own push output still knows.
pub(crate) fn remote_message(stranded: &StrandedPush) -> Vec<String> {
    let named = stranded.commits.join(", ");
    let commits = if stranded.commit_count > stranded.commits.len() {
        format!(
            "{named} and {} more",
            stranded.commit_count - stranded.commits.len()
        )
    } else {
        named
    };
    let one = stranded.commit_count == 1;
    vec![
        format!(
            "{} #{} from {} already {}.",
            stranded.repo,
            stranded.number,
            stranded.branch,
            if stranded.merged {
                "merged"
            } else {
                "closed without merging"
            }
        ),
        format!(
            "{} {} on this branch {} not on {}: {commits}",
            stranded.commit_count,
            if one { "commit" } else { "commits" },
            if one { "is" } else { "are" },
            stranded.base_ref,
        ),
        format!(
            "Nothing carries them: open a new pull request from {} so they can land.",
            stranded.branch
        ),
    ]
}

#[cfg(test)]
#[path = "pushed_after_merge_tests.rs"]
mod tests;
