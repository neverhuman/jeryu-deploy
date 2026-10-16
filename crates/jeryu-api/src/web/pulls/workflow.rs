//! The head-bound workflow view of a pull request: one block, served inside
//! the native pull detail as `workflow`, that answers "where does this pull
//! request stand and what is next" without a second route or any admin
//! access.
//!
//! It adds no mergeability rule. The outcome is read off the merge passport
//! blockers the detail already computes, every check and review carries the
//! head it was recorded on, and the whole block is stamped with the head it
//! was observed at. When the pull request moved to another head while the
//! block was being built, the outcome is `head_changed` rather than a verdict
//! about a head that is no longer current.

use jeryu_core::PullRequest;
use jeryu_readmodel::contracts::{MergePassportStatus, PullRequestDetail, PullRequestState};
use serde_json::{Value, json};

use super::{PullRequestChecks, RequiredContextPosture, RequiredContextState, web_pr_state};

/// The version of the `workflow` block, bumped when a field changes meaning.
const SCHEMA: &str = "pr_workflow/v1";

/// Every outcome the block can report. A client switches on this string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Draft,
    ChecksPending,
    ChecksFailed,
    ReviewNeeded,
    Blocked,
    Ready,
    Closed,
    Merged,
    HeadChanged,
}

impl Outcome {
    fn wire(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::ChecksPending => "checks_pending",
            Self::ChecksFailed => "checks_failed",
            Self::ReviewNeeded => "review_needed",
            Self::Blocked => "blocked",
            Self::Ready => "ready",
            Self::Closed => "closed",
            Self::Merged => "merged",
            Self::HeadChanged => "head_changed",
        }
    }
}

const CHECK_FAILED: &str = "passport_blocked_checks";
const CHECK_WAITING: [&str; 2] = [
    "passport_blocked_checks_missing",
    "passport_blocked_pending_checks",
];
const REVIEW_BLOCKERS: [&str; 3] = [
    "passport_blocked_approvals",
    "passport_blocked_changes_requested",
    "passport_blocked_threads",
];

/// The outcome for the head that was observed, from the detail's own lifecycle
/// and passport. `current_head` is the head read again after the block was
/// built; a different one voids the observation.
pub(super) fn outcome(
    pr: &PullRequest,
    detail: &PullRequestDetail,
    current_head: Option<&str>,
) -> Outcome {
    if current_head.is_some_and(|head| head != pr.head.sha) {
        return Outcome::HeadChanged;
    }
    match web_pr_state(pr) {
        PullRequestState::Merged => return Outcome::Merged,
        PullRequestState::Closed => return Outcome::Closed,
        PullRequestState::Open => {}
    }
    if pr.draft {
        return Outcome::Draft;
    }
    if detail.merge_passport.status == MergePassportStatus::Pass
        && detail.merge_passport.head_sha == pr.head.sha
    {
        return Outcome::Ready;
    }
    let codes: Vec<&str> = detail
        .merge_passport
        .blockers
        .iter()
        .map(|blocker| blocker.code.as_str())
        .collect();
    let known = |code: &&str| {
        *code == CHECK_FAILED || CHECK_WAITING.contains(code) || REVIEW_BLOCKERS.contains(code)
    };
    // A hold that is neither a check nor a review (the trust marking, the
    // forge's own gate) is the first thing to clear, whatever else is open.
    if codes.is_empty() || !codes.iter().all(known) {
        return Outcome::Blocked;
    }
    if codes.contains(&CHECK_FAILED) {
        Outcome::ChecksFailed
    } else if codes.iter().any(|code| CHECK_WAITING.contains(code)) {
        Outcome::ChecksPending
    } else {
        Outcome::ReviewNeeded
    }
}

/// A link a reader of this forge can follow: a path on this forge. A check's
/// own `target_url` can point at a log on a gate host the reader has no
/// account on, so anything that is not a plain forge path is dropped.
fn forge_path(url: Option<&str>) -> Option<&str> {
    url.filter(|url| url.starts_with('/') && !url.starts_with("//") && !url.contains(['\\', '\n']))
}

fn next_action(outcome: Outcome, pr: &PullRequest, detail: &PullRequestDetail) -> Value {
    let action = |code: &str, label: &str, method: Option<&str>, href: Option<String>| json!({ "code": code, "label": label, "method": method, "href": href });
    match outcome {
        Outcome::Draft => action(
            "pull.ready_for_review",
            "Mark the pull request ready for review",
            Some("POST"),
            Some(super::ready_route(pr)),
        ),
        Outcome::ChecksPending => action(
            "wait_for_checks",
            "Wait for the required checks on this head to finish",
            None,
            None,
        ),
        Outcome::ChecksFailed => action(
            "fix_failing_checks",
            "Push a fix; a required check failed on this head",
            None,
            None,
        ),
        Outcome::ReviewNeeded => action(
            "pull.review",
            "Get the required review on this head",
            None,
            None,
        ),
        Outcome::Blocked => action(
            "resolve_blockers",
            "Resolve the merge passport blockers",
            None,
            None,
        ),
        Outcome::Ready => json!({
            "code": "pull.merge",
            "label": "Merge with the exact head and passport hash",
            "method": "POST",
            "href": format!("/api/v1/repos/{}/{}/pulls/{}/merge", pr.owner, pr.repo, pr.number),
            "expected_head_sha": pr.head.sha,
            "expected_passport_hash": detail.passport_hash,
        }),
        Outcome::HeadChanged => action(
            "refresh_workflow",
            "The head moved while this was read; fetch the pull request again",
            Some("GET"),
            Some(crate::links::pull_api_url(&pr.owner, &pr.repo, pr.number)),
        ),
        Outcome::Closed | Outcome::Merged => action("none", "Nothing left to do", None, None),
    }
}

pub(super) fn view(
    pr: &PullRequest,
    detail: &PullRequestDetail,
    checks: &PullRequestChecks,
    required: &[RequiredContextPosture],
    current_head: Option<&str>,
) -> Value {
    let outcome = outcome(pr, detail, current_head);
    let head = pr.head.sha.as_str();
    let required_checks: Vec<Value> = required
        .iter()
        .map(|context| {
            let row = checks.checks.iter().find(|row| row.name == context.name);
            json!({
                "name": context.name,
                "state": match context.state {
                    RequiredContextState::Passing => "passing",
                    RequiredContextState::Failing => "failing",
                    RequiredContextState::Pending => "pending",
                    RequiredContextState::Missing => "missing",
                },
                "head_sha": head,
                "evidence_url": forge_path(row.and_then(|row| row.web_url.as_deref())),
            })
        })
        .collect();
    // Observed on the head, not required: shown so a client sees them, never
    // counted. A test a developer ran locally is not a row here at all.
    let other_checks: Vec<Value> = checks
        .checks
        .iter()
        .filter(|row| !required.iter().any(|context| context.name == row.name))
        .map(|row| {
            json!({
                "name": row.name,
                "status": row.status,
                "conclusion": row.conclusion,
                "head_sha": head,
                "required": false,
                "evidence_url": forge_path(row.web_url.as_deref()),
            })
        })
        .collect();
    let reviews: Vec<Value> = detail
        .reviews
        .iter()
        .map(|review| {
            json!({
                "id": review.id,
                "author": review.author,
                "state": review.state,
                "head_sha": review.head_sha,
                "counts_for_head": review.effective
                    && review.head_sha.as_deref() == Some(head),
            })
        })
        .collect();
    let review = &detail.summary.review;
    let blockers: Vec<Value> = detail
        .merge_passport
        .blockers
        .iter()
        .map(|blocker| json!({ "code": blocker.code, "message": blocker.message }))
        .collect();
    json!({
        "schema": SCHEMA,
        "outcome": outcome.wire(),
        "head_sha": head,
        "current_head_sha": current_head.unwrap_or(head),
        "base_ref": pr.base.ref_name,
        "lifecycle": match web_pr_state(pr) {
            PullRequestState::Open => "open",
            PullRequestState::Closed => "closed",
            PullRequestState::Merged => "merged",
        },
        "draft": pr.draft,
        "can_merge": outcome == Outcome::Ready,
        "required_checks": required_checks,
        "other_checks": other_checks,
        "review": {
            "required_approvals": review.required_approvals,
            "approvals": review.approvals,
            "changes_requested": review.changes_requested,
            "unresolved_threads": review.unresolved_threads,
            "reviews": reviews,
        },
        "passport": {
            "status": detail.merge_passport.status,
            "head_sha": detail.merge_passport.head_sha,
            "hash": detail.passport_hash,
            "blockers": blockers,
        },
        "next_action": next_action(outcome, pr, detail),
    })
}
