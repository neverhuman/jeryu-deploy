//! Commits pushed to a branch whose pull request already closed. One item per
//! branch: each one is its own decision about its own commits, and the answer
//! is always a new pull request from that branch.
//!
//! The facts come from [`crate::web::pushed_after_merge`], which is also what
//! the push itself reads to print its remote message, so the terminal and the
//! inbox never disagree about a branch.

use chrono::{DateTime, Utc};

use super::{Draft, Item, Severity, pull_href};

/// A branch holding commits no pull request carries, as the rule needs it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PushedAfterMergeFacts {
    /// `owner/name`.
    pub repo: String,
    pub branch: String,
    /// The pull request that closed with this branch as its head.
    pub number: u64,
    pub title: String,
    pub base_ref: String,
    /// Whether that pull request merged; the other case is closed unmerged.
    pub merged: bool,
    /// Abbreviated commits on the branch and not on the base, newest first.
    pub commits: Vec<String>,
    /// How many there are, which can be more than were named.
    pub commit_count: usize,
    /// When the pull request closed.
    pub closed_at: Option<DateTime<Utc>>,
}

pub(crate) fn pushed_after_merge_items(facts: &[PushedAfterMergeFacts]) -> Vec<Item> {
    facts
        .iter()
        .map(|fact| {
            let one = fact.commit_count == 1;
            let named = fact.commits.join(", ");
            let more = fact.commit_count.saturating_sub(fact.commits.len());
            let mut item = Draft {
                id: format!("pushed-after-merge-{}-{}", fact.repo, fact.branch),
                kind: "pushed_after_merge",
                severity: Severity::Action,
                title: format!(
                    "{} {} on {} after #{} {}",
                    fact.commit_count,
                    if one { "commit" } else { "commits" },
                    fact.branch,
                    fact.number,
                    if fact.merged { "merged" } else { "closed" }
                ),
                reason: format!(
                    "{} #{} ({}) already {}, and {named}{} {} on {} and not on {}. No open pull \
                     request carries {}, so nothing gates {} and nothing lands {}. Opening a pull \
                     request from the branch -- or deleting the branch, if the work is not wanted \
                     -- takes this row away.",
                    fact.repo,
                    fact.number,
                    fact.title,
                    if fact.merged {
                        "merged"
                    } else {
                        "was closed without merging"
                    },
                    if more == 0 {
                        String::new()
                    } else {
                        format!(" and {more} more")
                    },
                    if one { "is" } else { "are" },
                    fact.branch,
                    fact.base_ref,
                    if one { "it" } else { "them" },
                    if one { "it" } else { "them" },
                    if one { "it" } else { "them" },
                ),
                href: pull_href(&fact.repo, fact.number),
                label: "Open a new pull request from the branch",
                api: None,
                command: None,
            }
            .build();
            item.since = fact
                .closed_at
                .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
            item.repo = Some(fact.repo.clone());
            item.pr = Some(fact.number);
            item.sha = fact.commits.first().cloned();
            item
        })
        .collect()
}
