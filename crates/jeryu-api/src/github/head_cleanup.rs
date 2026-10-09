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
//! - no other open pull request uses it, as its head or as its base (a stacked
//!   pull request would lose its base);
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
        use jeryu_gitd::refs::{RefService, ZERO_OID};

        let name = merged.head_ref.to_string();
        let kept = |reason: String| HeadBranch::Kept {
            name: name.clone(),
            reason,
        };
        if let Some(reason) = self.keep_reason(merged) {
            return kept(reason);
        }
        let refs = RefService::new((**rm).clone());
        match refs.update_ref(
            resolved,
            "system:pr-merge",
            &format!("refs/heads/{}", merged.head_ref),
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
    fn keep_reason(&self, merged: &MergedHead<'_>) -> Option<String> {
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
        if merged.head_ref == merged.base_ref || merged.head_ref == repository.default_branch {
            return Some("it is the base or the default branch".to_string());
        }
        if self
            .core
            .get_branch_protection(merged.owner, merged.repo, merged.head_ref)
            .is_ok()
        {
            return Some("it has branch protection".to_string());
        }
        self.open_pull_using(merged)
            .map(|(full_name, number)| format!("open pull request {full_name}#{number} uses it"))
    }

    /// Another open pull request whose head is this branch of this repository,
    /// or whose base is this branch.
    fn open_pull_using(&self, merged: &MergedHead<'_>) -> Option<(String, u64)> {
        let this_repo = format!("{}/{}", merged.owner, merged.repo);
        for repository in self.core.list_repositories(None) {
            let Ok(pulls) = self
                .core
                .list_pull_requests(&repository.owner, &repository.name, None)
            else {
                continue;
            };
            for pr in pulls {
                if !is_open(&pr) {
                    continue;
                }
                let same_repo = pr.owner == merged.owner && pr.repo == merged.repo;
                if same_repo && pr.number == merged.number {
                    continue;
                }
                let head_here = if super::pulls::is_fork(&pr.owner, &pr.repo, &pr.source_repository)
                {
                    pr.source_repository.trim_end_matches(".git") == this_repo
                } else {
                    same_repo
                };
                let uses_as_head = head_here && pr.head.ref_name == merged.head_ref;
                let uses_as_base = same_repo && pr.base.ref_name == merged.head_ref;
                if uses_as_head || uses_as_base {
                    return Some((format!("{}/{}", pr.owner, pr.repo), pr.number));
                }
            }
        }
        None
    }
}

fn is_open(pr: &PullRequest) -> bool {
    !pr.merged
        && !matches!(
            pr.state,
            PullRequestState::Closed | PullRequestState::Merged
        )
}
