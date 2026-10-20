//! Branches with patch-unique commits and no open pull request.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use super::{Draft, Item, Severity, repo_href};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StrandedBranchFacts {
    /// `owner/name`.
    pub repo: String,
    pub branch: String,
    pub default_branch: String,
    pub commit_count: usize,
    pub newest_sha: String,
    pub newest_subject: String,
    pub newest_at: Option<DateTime<Utc>>,
    pub closed_pr: Option<u64>,
}

pub(crate) fn stranded_branch_items(facts: &[StrandedBranchFacts]) -> Vec<Item> {
    facts
        .iter()
        .map(|fact| {
            let one = fact.commit_count == 1;
            let count = if fact.commit_count >= 200 {
                format!("at least {}", fact.commit_count)
            } else {
                fact.commit_count.to_string()
            };
            let closed = fact.closed_pr.is_some();
            let label = if closed {
                "Recover the commits onto a fresh branch from main with cherry-pick -x and open a PR"
            } else {
                "Open a pull request for the branch"
            };
            let mut item = Draft {
                id: format!("stranded-branch-{}-{}", fact.repo, fact.branch),
                kind: "stranded_branch",
                severity: Severity::Action,
                title: format!(
                    "{} has {} unlanded {} on {}: {}",
                    fact.repo,
                    count,
                    if one { "commit" } else { "commits" },
                    fact.branch,
                    fact.newest_subject
                ),
                reason: format!(
                    "{} branch {} has {} patch-unique {} not on {}, and no open pull request \
                     carries the branch. The newest commit is {} ({}). {}.",
                    fact.repo,
                    fact.branch,
                    count,
                    if one { "commit" } else { "commits" },
                    fact.default_branch,
                    fact.newest_sha,
                    fact.newest_subject,
                    if closed {
                        "Recover the commits onto a fresh branch from main with cherry-pick -x, then open a pull request"
                    } else {
                        "Open a pull request from the branch, or delete it if the work is not wanted"
                    },
                ),
                href: repo_href(&fact.repo),
                label,
                api: None,
                command: None,
            }
            .build();
            item.since = fact
                .newest_at
                .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
            item.repo = Some(fact.repo.clone());
            item.sha = Some(fact.newest_sha.clone());
            item
        })
        .collect()
}

/// A branch whose newest commit is older than this is backlog, not news.
pub(crate) const BACKLOG_AGE_DAYS: i64 = 7;
/// Branches named in a backlog row; the list route has the rest.
const BACKLOG_NAMED: usize = 5;

type OldBranch = (String, Option<DateTime<Utc>>);

/// One repository's old branches that hold commits not on its default branch.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StrandedBacklog {
    /// `owner/name`.
    pub repo: String,
    pub count: usize,
    /// The first few branch names, sorted.
    pub branches: Vec<String>,
    pub oldest_at: Option<DateTime<Utc>>,
}

/// Split off each repository's old branches. A repository with two or more of
/// them gets one backlog entry instead of a row per branch, so a rule that
/// finds months of leftovers at once adds one row, not dozens. Fresh branches,
/// and a lone old one, stay individual facts.
pub(crate) fn split_backlog(
    facts: Vec<StrandedBranchFacts>,
    now: DateTime<Utc>,
) -> (Vec<StrandedBranchFacts>, Vec<StrandedBacklog>) {
    let cutoff = now - Duration::days(BACKLOG_AGE_DAYS);
    let is_old = |fact: &StrandedBranchFacts| fact.newest_at.is_some_and(|at| at < cutoff);
    let mut old: BTreeMap<String, Vec<OldBranch>> = BTreeMap::new();
    for fact in facts.iter().filter(|fact| is_old(fact)) {
        old.entry(fact.repo.clone())
            .or_default()
            .push((fact.branch.clone(), fact.newest_at));
    }
    old.retain(|_, branches| branches.len() >= 2);
    let backlogs: Vec<StrandedBacklog> = old
        .iter()
        .map(|(repo, branches)| {
            let mut names: Vec<String> =
                branches.iter().map(|(branch, _)| branch.clone()).collect();
            names.sort_unstable();
            StrandedBacklog {
                repo: repo.clone(),
                count: names.len(),
                branches: names.into_iter().take(BACKLOG_NAMED).collect(),
                oldest_at: branches.iter().filter_map(|(_, at)| *at).min(),
            }
        })
        .collect();
    let rest = facts
        .into_iter()
        .filter(|fact| !(is_old(fact) && old.contains_key(&fact.repo)))
        .collect();
    (rest, backlogs)
}

/// The route that lists a repository's stranded branches.
pub(crate) fn backlog_list_href(repo: &str) -> String {
    format!("/api/v1/repos/{repo}/stranded-branches")
}

pub(crate) fn stranded_backlog_items(backlogs: &[StrandedBacklog]) -> Vec<Item> {
    backlogs
        .iter()
        .map(|backlog| {
            let named = backlog.branches.join(", ");
            let more = backlog.count.saturating_sub(backlog.branches.len());
            let mut item = Draft {
                id: format!("stranded-branches-{}", backlog.repo),
                kind: "stranded_branch",
                severity: Severity::Action,
                title: format!(
                    "{}: {} old branches hold commits not on main: review / delete",
                    backlog.repo, backlog.count
                ),
                reason: format!(
                    "{} has {} branches older than {BACKLOG_AGE_DAYS} days whose commits are not \
                     on the default branch and that no open pull request carries, such as {named}{}. \
                     Review each and delete the ones that are not wanted, or open a pull request \
                     for the ones that are. The full list is at {}.",
                    backlog.repo,
                    backlog.count,
                    if more == 0 {
                        String::new()
                    } else {
                        format!(" and {more} more")
                    },
                    backlog_list_href(&backlog.repo),
                ),
                href: backlog_list_href(&backlog.repo),
                label: "Review the old branches, delete the ones not wanted",
                api: None,
                command: None,
            }
            .build();
            item.since = backlog
                .oldest_at
                .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
            item.repo = Some(backlog.repo.clone());
            item
        })
        .collect()
}
