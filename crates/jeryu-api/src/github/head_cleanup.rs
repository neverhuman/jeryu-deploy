//! Deleting a pull request's head branch once it has merged
//! (`delete_branch_on_merge`, see [`super::merge_settings`]).
//!
//! Every path that records a real merge — a direct merge, a head already
//! contained in the base, a base seeded at the head, and a merge-queue landing
//! — ends here after the merge is recorded. The branch is deleted only when
//! all of these hold, and otherwise kept with the reason:
//!
//! - the head lives in this repository (a fork's branch is not ours to delete);
//! - the repository has the setting on (the default);
//! - the head is not the base or the default branch, and has no branch
//!   protection of its own;
//! - no pull request in this repository has ever merged INTO it: a branch that
//!   has been a base is a long-lived line (`release/5.0`, `develop`), and a
//!   back-merge out of it must not delete it;
//! - no other open pull request uses it as its head;
//! - the merged head is pinned first at `refs/pull/<n>/head`, the ref GitHub
//!   keeps for every pull request. A merge-queue landing or a linear-history
//!   merge lands a REPLAY of the head, so the gated head commit is not in the
//!   base's history; without the pin, deleting the branch would leave the
//!   commit the checks ran on, and the pull request's diff, unreachable;
//! - it still points at the commit that merged. The delete is a
//!   compare-and-swap against that commit, so a push that landed after the
//!   merge is never thrown away, and it goes through the same ref policy as a
//!   `git push --delete`, so a branch the server's protected-ref rules cover
//!   is kept.
//!
//! The merge itself is already decided when this runs: a branch that is kept
//! is reported in the merge response, never turned into a failed merge.

#![cfg(feature = "web")]

use jeryu_core::{PullRequest, PullRequestState};
use jeryu_gitd::refs::RefService;
use serde_json::{Value, json};

use super::GithubRouter;

/// What happened to a merged pull request's head branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HeadBranch {
    Deleted { name: String },
    Kept { name: String, reason: String },
}

impl HeadBranch {
    /// The `head_branch` field of the merge response.
    pub(super) fn json(&self) -> Value {
        match self {
            Self::Deleted { name } => json!({ "name": name, "deleted": true }),
            Self::Kept { name, reason } => {
                json!({ "name": name, "deleted": false, "reason": reason })
            }
        }
    }
}

/// The pull request that just merged, as the cleanup needs it.
pub(super) struct MergedHead<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    pub number: u64,
    pub base_ref: &'a str,
    /// The head as recorded, possibly in GitHub's `owner:branch` spelling.
    pub head_ref: &'a str,
    /// Repository full name the head lives in, as recorded on the pull request.
    pub source_repository: &'a str,
    /// The head commit that merged: the branch is deleted only while it still
    /// points here.
    pub merged_head: &'a str,
}

impl GithubRouter {
    pub(super) fn delete_merged_head(
        &self,
        rm: &std::sync::Arc<jeryu_gitd::RepoManager>,
        resolved: &jeryu_gitd::repo::Repository,
        merged: &MergedHead<'_>,
    ) -> HeadBranch {
        use jeryu_gitd::refs::ZERO_OID;

        let branch = super::pulls::head_branch(merged.head_ref);
        let name = branch.to_string();
        let kept = |reason: String| HeadBranch::Kept {
            name: name.clone(),
            reason,
        };
        if let Some(reason) = self.keep_reason(merged, branch) {
            return kept(reason);
        }
        let refs = RefService::new((**rm).clone());
        if let Err(reason) = pin_pull_head(&refs, resolved, merged) {
            return kept(reason);
        }
        match refs.update_ref(
            resolved,
            "system:pr-merge",
            &format!("refs/heads/{branch}"),
            ZERO_OID,
            Some(merged.merged_head),
        ) {
            Ok(()) => HeadBranch::Deleted { name },
            Err(err) => kept(format!(
                "git refused to delete it ({err}); it may have moved since the merge or be \
                 covered by a protected-ref rule"
            )),
        }
    }

    /// Why the head branch stays, or `None` when it may be deleted.
    fn keep_reason(&self, merged: &MergedHead<'_>, branch: &str) -> Option<String> {
        if super::pulls::is_fork(merged.owner, merged.repo, merged.source_repository) {
            return Some(format!(
                "it lives in {}, not in this repository",
                merged.source_repository
            ));
        }
        let repository = match self.core.get_repository(merged.owner, merged.repo) {
            Ok(repository) => repository,
            Err(err) => return Some(format!("the repository could not be read ({err})")),
        };
        if !self
            .merge_settings
            .delete_branch_on_merge(&repository.id.to_string())
        {
            return Some("delete_branch_on_merge is off for this repository".to_string());
        }
        if branch == merged.base_ref || branch == repository.default_branch {
            return Some("it is the base or the default branch".to_string());
        }
        if self
            .core
            .get_branch_protection(merged.owner, merged.repo, branch)
            .is_ok()
        {
            return Some("it has branch protection".to_string());
        }
        if let Some(number) = self.pull_based_on(merged, branch) {
            return Some(format!(
                "pull request #{number} merges into it, so it is a long-lived branch"
            ));
        }
        self.open_pull_using(merged, branch)
    }

    /// A pull request in this repository, in any state, whose base is
    /// `branch`. Its number is safe to name: it is in the repository being
    /// merged.
    fn pull_based_on(&self, merged: &MergedHead<'_>, branch: &str) -> Option<u64> {
        self.core
            .list_pull_requests(merged.owner, merged.repo, None)
            .ok()?
            .into_iter()
            .filter(|pr| pr.number != merged.number)
            .find(|pr| pr.base.ref_name == branch)
            .map(|pr| pr.number)
    }

    /// Another open pull request whose head is this branch of this repository.
    /// One in another repository is reported without its name: the caller may
    /// not be able to read that repository.
    fn open_pull_using(&self, merged: &MergedHead<'_>, branch: &str) -> Option<String> {
        let this_repo = format!("{}/{}", merged.owner, merged.repo);
        for repository in self.core.list_repositories(None) {
            let Ok(pulls) = self
                .core
                .list_pull_requests(&repository.owner, &repository.name, None)
            else {
                continue;
            };
            for pr in pulls {
                if !is_open(&pr) || super::pulls::head_branch(&pr.head.ref_name) != branch {
                    continue;
                }
                let same_repo = pr.owner == merged.owner && pr.repo == merged.repo;
                if same_repo && pr.number == merged.number {
                    continue;
                }
                if super::pulls::is_fork(&pr.owner, &pr.repo, &pr.source_repository) {
                    if pr.source_repository.trim_end_matches(".git") == this_repo {
                        return Some(
                            "an open pull request in another repository uses it as its head"
                                .to_string(),
                        );
                    }
                } else if same_repo {
                    return Some(format!("open pull request #{} uses it", pr.number));
                }
            }
        }
        None
    }
}

/// Point `refs/pull/<n>/head` at the merged head, so the commit the checks ran
/// on stays reachable once the branch is gone. An existing ref is moved by
/// compare-and-swap; a pin that cannot be written keeps the branch.
fn pin_pull_head(
    refs: &RefService,
    resolved: &jeryu_gitd::repo::Repository,
    merged: &MergedHead<'_>,
) -> Result<(), String> {
    let pull_ref = format!("refs/pull/{}/head", merged.number);
    let current = refs
        .resolve_commit(resolved, &pull_ref)
        .map_err(|err| format!("{pull_ref} could not be read ({err}), so the branch is kept"))?;
    if current.as_deref() == Some(merged.merged_head) {
        return Ok(());
    }
    refs.update_ref(
        resolved,
        "system:pr-merge",
        &pull_ref,
        merged.merged_head,
        current.as_deref(),
    )
    .map_err(|err| {
        format!("the merged head could not be kept at {pull_ref} ({err}), so the branch is kept")
    })
}

fn is_open(pr: &PullRequest) -> bool {
    !pr.merged
        && !matches!(
            pr.state,
            PullRequestState::Closed | PullRequestState::Merged
        )
}
