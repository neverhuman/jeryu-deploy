//! `GET /api/v1/repos/:id/pulls/:number/owner-paths` — which merge-passport
//! blockers the viewer can clear, and how.
//!
//! The PR page shows the Merge button disabled with one line per blocker. That
//! answers "why not", and for an owner looking at a head they have already
//! decided about it stopped there: a repository admin reading "Blocked: 3
//! things need doing" had no way to tell which of the three were theirs to
//! clear, and the only routes anybody found were switching branch protection
//! off and back on, or pushing an empty commit to force a re-review.
//!
//! So this route answers "what can I do about it" for the account asking.
//! Three rules hold it honest:
//!
//!   * every blocker of the passport appears here, in the passport's own
//!     order, whether or not the viewer can clear it. A surface that lists
//!     only the clearable ones hides the gate;
//!   * `owner` is true only where the base branch's protection leaves
//!     administrators out (`enforce_admins: false`) AND the viewer is an
//!     administrator of this repository. Both are facts, read per request;
//!   * each step is one the forge actually serves today. Nothing here
//!     suggests disabling protection or rewriting somebody's branch.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path as AxumPath, State};
use axum::response::{IntoResponse, Response as AxumResponse};
use jeryu_core::{AccountSummary, PullRequest, UserRole};
use jeryu_readmodel::contracts::{MergePassportBlocker, MergePassportStatus};
use serde::Serialize;

use super::super::WebState;
use super::{detail_for_pr, not_found, ready_route, resolve_pr};

/// What one blocker of the passport asks of this viewer.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(in crate::web) struct OwnerBlockerPath {
    /// The passport blocker's canonical code, unchanged.
    pub code: String,
    /// The passport's own message, unchanged: the two surfaces never disagree.
    pub message: String,
    /// Whether a repository administrator can clear this one themselves.
    pub owner_can_clear: bool,
    /// The step that clears it, in one sentence. Always present: a blocker
    /// nobody can clear from here still says who the question belongs to.
    pub how: String,
}

/// The viewer's standing with a blocked pull request.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(in crate::web) struct OwnerMergePaths {
    pub head_sha: String,
    pub base_ref: String,
    /// The passport hash the rows below were read from, so a client acting on
    /// them can tell whether the gate moved underneath it.
    pub passport_hash: Option<String>,
    /// Whether the passport is blocked at all. False means the rows are empty
    /// and the Merge button is live.
    pub blocked: bool,
    /// `enforce_admins` on the protected base, visible only to repository
    /// administrators. `None` also covers a base without a protection rule.
    pub enforce_admins: Option<bool>,
    pub viewer_is_admin: bool,
    /// True when the viewer is a repository administrator and the base's
    /// protection leaves administrators out. The blockers an owner can clear
    /// are listed either way; this says whether the viewer is that owner.
    pub viewer_may_clear_as_owner: bool,
    /// One row per passport blocker, in the passport's order.
    pub blockers: Vec<OwnerBlockerPath>,
    /// The one sentence to put above the rows.
    pub headline: String,
}

pub(in crate::web) async fn show(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
) -> AxumResponse {
    let Some((repo, pr)) = resolve_pr(&state, &id, number) else {
        return not_found("read the owner merge paths", "pull request not found");
    };
    let detail = detail_for_pr(&state, &pr, Some(&account.login));
    let viewer_is_admin = viewer_is_repo_admin(&state, &account, &repo.owner, &repo.name);
    // Protection settings are administrative data. Readers receive the
    // passport blockers without learning whether administrators are exempt.
    let enforce_admins = viewer_is_admin
        .then(|| {
            state
                .github
                .core()
                .get_branch_protection(&pr.owner, &pr.repo, &pr.base.ref_name)
                .ok()
                .map(|rule| rule.enforce_admins)
        })
        .flatten();
    // No protection rule leaves nobody out: there is no administrator bypass
    // to claim, and the blockers that remain are not branch protection's.
    let viewer_may_clear_as_owner = viewer_is_admin && enforce_admins == Some(false);
    let blocked = detail.merge_passport.status != MergePassportStatus::Pass;
    let blockers: Vec<OwnerBlockerPath> = detail
        .merge_passport
        .blockers
        .iter()
        .map(|blocker| path_for(blocker, &pr))
        .collect();
    Json(OwnerMergePaths {
        head_sha: detail.merge_passport.head_sha.clone(),
        base_ref: pr.base.ref_name.clone(),
        passport_hash: detail.passport_hash.clone(),
        blocked,
        enforce_admins,
        viewer_is_admin,
        viewer_may_clear_as_owner,
        headline: headline(blocked, viewer_may_clear_as_owner, &blockers),
        blockers,
    })
    .into_response()
}

/// Whether this account administers this repository: a forge administrator, or
/// an explicit `admin` grant on it.
fn viewer_is_repo_admin(
    state: &WebState,
    account: &AccountSummary,
    owner: &str,
    name: &str,
) -> bool {
    if account.role == UserRole::Admin {
        return true;
    }
    state
        .github
        .core()
        .repo_access_for(&account.login, owner, name)
        .is_some_and(|level| level.allows_admin())
}

/// The step that clears one blocker. Every sentence here names something the
/// forge serves: a review to post, a thread to resolve, a setting an admin
/// owns. Turning protection off is not a step, and is not offered.
fn path_for(blocker: &MergePassportBlocker, pr: &PullRequest) -> OwnerBlockerPath {
    let (owner_can_clear, how): (bool, String) = match blocker.code.as_str() {
        "passport_blocked_changes_requested" => (
            false,
            "Address the current review findings and ask the reviewer that requested changes \
             to review this exact head again. An owner's separate approval does not clear \
             another reviewer's hold."
                .to_string(),
        ),
        "passport_blocked_approvals" => (
            true,
            "Approve this exact head. An author cannot approve their own pull request, so on \
             your own change this one needs somebody else."
                .to_string(),
        ),
        "passport_blocked_threads" => (
            true,
            "Answer and resolve the open review threads on the Conversation tab.".to_string(),
        ),
        "passport_blocked_draft" => (
            true,
            format!(
                "Mark it ready for review (POST {}); the author or an admin can.",
                ready_route(pr)
            ),
        ),
        "passport_blocked_checks" | "passport_blocked_checks_missing" => (
            false,
            "Repair the failure and run the required context on this exact head. \
             Only a passing required run clears this blocker; retain branch protection."
                .to_string(),
        ),
        "passport_blocked_pending_checks" => (
            false,
            "It is still running. Nobody clears this one; it clears itself.".to_string(),
        ),
        "passport_blocked_contributed_origin" => (
            false,
            "A login on the configured maintainer list has to approve this exact head. \
             Being an administrator of the repository is not that list."
                .to_string(),
        ),
        "passport_blocked_contributed_not_ok_to_test" => (
            false,
            "A login on the configured maintainer list has to say ok-to-test for this exact \
             head (POST .../ok-to-test). Being an administrator of the repository is not that \
             list."
                .to_string(),
        ),
        "passport_blocked_contributed_dco" => (
            false,
            "The contributor has to sign off every commit (`git commit -s`) and push again. \
             No approval changes it."
                .to_string(),
        ),
        "passport_blocked_mergeability" => (
            false,
            format!(
                "The forge itself will not merge this head ({}). Update the branch against `{}` \
                 and push; no approval changes it.",
                pr.mergeable_state, pr.base.ref_name
            ),
        ),
        _ => (
            false,
            "Read the blocker's own message: this one has no owner step on this page.".to_string(),
        ),
    };
    OwnerBlockerPath {
        code: blocker.code.clone(),
        message: blocker.message.clone(),
        owner_can_clear,
        how,
    }
}

/// The sentence above the rows. It says how many of the blockers are the
/// viewer's to clear, so an owner never reads "blocked" with no path again.
fn headline(blocked: bool, owner: bool, blockers: &[OwnerBlockerPath]) -> String {
    if !blocked {
        return "The merge passport passes on this head.".to_string();
    }
    let clearable = blockers.iter().filter(|row| row.owner_can_clear).count();
    let total = blockers.len();
    if clearable == 0 {
        return format!(
            "{total} {} this merge, and none of them clears from this page.",
            if total == 1 {
                "blocker holds"
            } else {
                "blockers hold"
            }
        );
    }
    let who = if owner {
        "you can clear"
    } else {
        "a repository admin can clear"
    };
    format!(
        "{clearable} of {total} {} {who}, each with its step below. The rest stay as they are.",
        if clearable == 1 {
            "blocker"
        } else {
            "blockers"
        }
    )
}
