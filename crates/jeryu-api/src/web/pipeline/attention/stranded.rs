//! Branches with patch-unique commits and no open pull request.

use chrono::{DateTime, Utc};

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
