//! Pull request BFF routes for the SPA's W-FE-11 surface.
//!
//! These routes translate the local forge's authoritative pull request,
//! review, and check-run state into the typed web contracts consumed by the
//! React cockpit. Missing diff hunks or review threads are explicit empty
//! payloads derived from the PR metadata, never synthetic review content.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use jeryu_core::{
    AccountSummary, CheckConclusion, CheckRun, CheckRunStatus, CommitStatus, CommitStatusState,
    CreateReviewRequest, ForgeError, MergeBlocker,
    MergePullRequestRequest as CoreMergePullRequestRequest, PullRequest, ReviewCommentInput,
    ReviewState, UpdatePullRequestRequest, UserRole, check_conclusion_wire_value,
    effective_reviews_for_head,
};
use jeryu_readmodel::contracts::{
    AgentPosture, AvailableAction, CheckPosture, CreateReviewCommentRequest, EntityHandle,
    MergePassport, MergePassportBlocker, MergePassportStatus, Mergeability, PullRequestDetail,
    PullRequestReview, PullRequestState as WebPullRequestState, PullRequestSummary,
    ReviewComment as WebReviewComment, ReviewPosture, ReviewThread, ReviewVerdict,
    SubmitReviewRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::paging::{PageInfo, PageParams};
use super::repositories::{find_repo, repo_id};
use super::{WebState, server_time};

const DOCS_URL: &str = "docs/errors.md";
const PROOF_LANE: &str = "rerun cargo test -p jeryu-api --features web --jobs 40 pulls";

#[derive(Debug, Clone, Deserialize)]
pub(super) struct PullListQuery {
    pub state: Option<String>,
    #[serde(flatten)]
    pub paging: PageParams,
}

/// The `state` filter values `GET /api/v1/repos/:id/pulls` understands.
const PULL_STATES: &[&str] = &["open", "closed", "merged", "all"];

#[derive(Debug, Clone, Serialize)]
struct PullRequestListResponse {
    items: Vec<PullRequestSummary>,
    /// Pull requests matching the filter before paging.
    total: usize,
    page: PageInfo,
}

#[derive(Debug, Clone, Serialize)]
struct PullRequestDiff {
    head_sha: String,
    base_sha: String,
    files: Vec<PullRequestDiffFile>,
    truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(in crate::web) struct PullRequestDiffFile {
    pub(in crate::web) path: String,
    pub(in crate::web) old_path: Option<String>,
    pub(in crate::web) status: &'static str,
    pub(in crate::web) additions: u32,
    pub(in crate::web) deletions: u32,
    pub(in crate::web) risk: Option<&'static str>,
    pub(in crate::web) is_binary: bool,
    pub(in crate::web) hunks: Vec<PullRequestDiffHunk>,
}

#[derive(Debug, Clone, Serialize)]
pub(in crate::web) struct PullRequestDiffHunk {
    pub(in crate::web) header: String,
    pub(in crate::web) old_start: u32,
    pub(in crate::web) old_lines: u32,
    pub(in crate::web) new_start: u32,
    pub(in crate::web) new_lines: u32,
    pub(in crate::web) lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PullRequestChecks {
    total: u32,
    passing: u32,
    failing: u32,
    pending: u32,
    skipped: u32,
    checks: Vec<PullRequestCheck>,
}

#[derive(Debug, Clone, Serialize)]
struct PullRequestCheck {
    id: String,
    name: String,
    /// `check_run` or `status` (a commit status such as `<repo>/required`).
    kind: &'static str,
    status: String,
    conclusion: Option<String>,
    details_url: Option<String>,
    /// The check run's `output.title`; `None` for commit statuses.
    title: Option<String>,
    /// The check run's `output.summary`, or the commit status description.
    description: Option<String>,
    /// The check run's `output.text`: for a failing `jankurai/proof`, the
    /// findings behind the score, each with its `path:line`. The panel renders
    /// it under the row so a reader never has to open the report to start.
    details_text: Option<String>,
    /// The label of [`Self::web_url`] on the row. A link a reader cannot see is
    /// a link they do not follow.
    web_url_label: Option<&'static str>,
    /// The human page that explains this check: the Quality gate head view for
    /// `jankurai/proof`, the gate run log (`target_url`) for a status.
    web_url: Option<String>,
    /// Whether the base branch requires this context before a merge.
    required: bool,
    /// Why a check is not required, said on its row (`None` when required).
    advisory: Option<CheckAdvisory>,
    started_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CheckAdvisory {
    label: String,
    reason: String,
    /// A web path that says more; `None` when the reader should look at the
    /// branch protection of the repository.
    url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RequiredContextState {
    Missing,
    Failing,
    Pending,
    Passing,
}

impl RequiredContextState {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Failing => "failing",
            Self::Pending => "pending",
            Self::Passing => "passing",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct RequiredContextPosture {
    name: String,
    state: RequiredContextState,
    details: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PullRequestThreadList {
    threads: Vec<ReviewThread>,
}

#[derive(Debug, Clone, Deserialize)]
struct PullApproveRequest {
    expected_head_sha: String,
    #[serde(default)]
    body_markdown: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct MergeRequest {
    expected_head_sha: String,
    #[serde(default)]
    expected_passport_hash: Option<String>,
    #[serde(default = "default_merge_method")]
    merge_method: String,
    #[serde(default)]
    commit_title: Option<String>,
    #[serde(default)]
    commit_message: Option<String>,
}

fn default_merge_method() -> String {
    "merge".to_string()
}

pub(super) async fn list(
    State(state): State<Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<PullListQuery>,
) -> AxumResponse {
    let page = match query.paging.page() {
        Ok(page) => page,
        Err(rejection) => return rejection.into_response(),
    };
    let filter = query
        .state
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(filter) = filter
        && !PULL_STATES.contains(&filter)
    {
        return repair_error(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "load repository pull requests",
            &format!("state must be one of {PULL_STATES:?}, got {filter:?}"),
            &["send state=open, closed, merged or all, or omit it"],
            PROOF_LANE,
            None,
        );
    }
    let Some(repo) = find_repo(&state, &id) else {
        return not_found("load repository pull requests", "repository not found");
    };
    let pulls = match state
        .github
        .core()
        .list_pull_requests(&repo.owner, &repo.name, None)
    {
        Ok(pulls) => pulls,
        Err(error) => return core_error(error, "load repository pull requests"),
    };
    let mut items: Vec<_> = pulls
        .iter()
        .filter(|pr| state_matches(pr, filter))
        .map(|pr| summary(&state, pr))
        .collect();
    items.sort_by_key(|pr| pr.number);
    let (items, page) = page.apply(items);
    Json(PullRequestListResponse {
        total: page.total,
        items,
        page,
    })
    .into_response()
}

pub(super) async fn detail(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
) -> AxumResponse {
    let Some((_, pr)) = resolve_pr(&state, &id, number) else {
        return not_found("load pull request detail", "pull request not found");
    };
    Json(detail_for_pr(&state, &pr, Some(&account.login))).into_response()
}

pub(super) async fn diff(
    State(state): State<Arc<WebState>>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
) -> AxumResponse {
    let Some((_, pr)) = resolve_pr(&state, &id, number) else {
        return not_found("load pull request diff", "pull request not found");
    };
    Json(diff::pull_request_diff(&state, &pr)).into_response()
}

pub(super) async fn checks(
    State(state): State<Arc<WebState>>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
) -> AxumResponse {
    let Some((_, pr)) = resolve_pr(&state, &id, number) else {
        return not_found("load pull request checks", "pull request not found");
    };
    Json(checks_for_pr(&state, &pr)).into_response()
}

pub(super) async fn threads(
    State(state): State<Arc<WebState>>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
) -> AxumResponse {
    let Some((_, pr)) = resolve_pr(&state, &id, number) else {
        return not_found("load pull request threads", "pull request not found");
    };
    Json(PullRequestThreadList {
        threads: threads_for_pr(&state, &pr),
    })
    .into_response()
}

pub(super) async fn review(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
    body: Bytes,
) -> AxumResponse {
    let request: SubmitReviewRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return repair_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "pull_review_invalid_request",
                "submit pull request review",
                &format!("review body failed validation: {error}"),
                &[
                    "send SubmitReviewRequest JSON with verdict and expected_head_sha",
                    "refresh the PR detail before retrying the review submission",
                ],
                PROOF_LANE,
                None,
            );
        }
    };
    let Some((repo, pr)) = resolve_pr(&state, &id, number) else {
        return not_found("submit pull request review", "pull request not found");
    };
    if request.expected_head_sha != pr.head.sha {
        return stale_sha(&request.expected_head_sha, &pr.head.sha);
    }
    let comments = request
        .thread_comments
        .into_iter()
        .filter_map(comment_input)
        .collect();
    let event = review_state(request.verdict);
    let verdict_name = match event {
        ReviewState::Approved => "approve",
        ReviewState::ChangesRequested => "request_changes",
        _ => "comment",
    };
    let review_body = request.body_markdown.clone();
    if event == ReviewState::Approved
        && let Some(response) = self_approval_forbidden(&pr, &account.login)
    {
        return response;
    }
    let review = CreateReviewRequest {
        body: request.body_markdown,
        event,
        comments,
        expected_head_sha: Some(request.expected_head_sha),
    };
    match state.github.core().create_review(
        &repo.owner,
        &repo.name,
        pr.number,
        &account.login,
        review,
    ) {
        Ok(_) => match state
            .github
            .core()
            .get_pull_request(&repo.owner, &repo.name, pr.number)
        {
            Ok(updated) => {
                super::pipeline::emit::pull_reviewed(
                    &state,
                    &updated,
                    &account.login,
                    verdict_name,
                    review_body.as_deref(),
                );
                Json(detail_for_pr(&state, &updated, Some(&account.login))).into_response()
            }
            Err(error) => core_error(error, "reload pull request after review"),
        },
        Err(error) => core_error(error, "submit pull request review"),
    }
}

pub(super) async fn comment(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
    body: Bytes,
) -> AxumResponse {
    let request: CreateReviewCommentRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return repair_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "pull_comment_invalid_request",
                "submit pull request comment",
                &format!("comment body failed validation: {error}"),
                &[
                    "send CreateReviewCommentRequest JSON",
                    "refresh the PR detail before retrying the comment submission",
                ],
                PROOF_LANE,
                None,
            );
        }
    };
    if request.body_markdown.trim().is_empty() {
        return repair_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "pull_comment_body_required",
            "submit pull request comment",
            "comment body_markdown must be non-empty",
            &[
                "enter a review comment body before submitting",
                "retry with the same anchor after refreshing the diff",
            ],
            PROOF_LANE,
            None,
        );
    }
    let Some((repo, pr)) = resolve_pr(&state, &id, number) else {
        return not_found("submit pull request comment", "pull request not found");
    };
    let comments = comment_input(request).into_iter().collect();
    match state.github.core().create_review(
        &repo.owner,
        &repo.name,
        pr.number,
        &account.login,
        CreateReviewRequest {
            body: None,
            event: ReviewState::Commented,
            comments,
            expected_head_sha: Some(pr.head.sha.clone()),
        },
    ) {
        Ok(_) => Json(PullRequestThreadList {
            threads: threads_for_pr(&state, &pr),
        })
        .into_response(),
        Err(error) => core_error(error, "submit pull request comment"),
    }
}

pub(super) async fn approve(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
    body: Bytes,
) -> AxumResponse {
    let request: PullApproveRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return repair_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "pull_approve_invalid_request",
                "approve pull request",
                &format!("approval body failed validation: {error}"),
                &[
                    "send expected_head_sha from the current PR detail",
                    "refresh the PR detail before approving again",
                ],
                PROOF_LANE,
                None,
            );
        }
    };
    let Some((repo, pr)) = resolve_pr(&state, &id, number) else {
        return not_found("approve pull request", "pull request not found");
    };
    if request.expected_head_sha != pr.head.sha {
        return stale_sha(&request.expected_head_sha, &pr.head.sha);
    }
    if let Some(response) = self_approval_forbidden(&pr, &account.login) {
        return response;
    }
    // The gate, on the exact head: nothing approves a head whose
    // `jankurai/proof` fails, is missing, or has not finished. It is refused
    // here rather than left to the merge passport, because an approval already
    // on a red head reads as a human having accepted it.
    if let Some(verdict) = posture::jankurai_gate_blocker(&state, &pr) {
        return jankurai_gate_refusal(&pr, &verdict);
    }
    match state.github.core().create_review(
        &repo.owner,
        &repo.name,
        pr.number,
        &account.login,
        CreateReviewRequest {
            body: request.body_markdown,
            event: ReviewState::Approved,
            comments: Vec::new(),
            expected_head_sha: Some(request.expected_head_sha),
        },
    ) {
        Ok(_) => match state
            .github
            .core()
            .get_pull_request(&repo.owner, &repo.name, pr.number)
        {
            Ok(updated) => {
                super::pipeline::emit::pull_approved(&state, &updated, &account.login);
                Json(detail_for_pr(&state, &updated, Some(&account.login))).into_response()
            }
            Err(error) => core_error(error, "reload pull request after approval"),
        },
        Err(error) => core_error(error, "approve pull request"),
    }
}

pub(super) async fn merge(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
    body: Bytes,
) -> AxumResponse {
    let request: MergeRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return repair_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "pull_merge_invalid_request",
                "merge pull request",
                &format!("merge body failed validation: {error}"),
                &[
                    "send expected_head_sha and expected_passport_hash from the current PR detail",
                    "refresh the PR detail before retrying merge",
                ],
                PROOF_LANE,
                None,
            );
        }
    };
    let Some((repo, pr)) = resolve_pr(&state, &id, number) else {
        return not_found("merge pull request", "pull request not found");
    };
    if request.expected_head_sha != pr.head.sha {
        return stale_sha(&request.expected_head_sha, &pr.head.sha);
    }
    let current = detail_for_pr(&state, &pr, None);
    if request.expected_passport_hash.as_deref() != current.passport_hash.as_deref() {
        return repair_error(
            StatusCode::CONFLICT,
            "merge_passport_stale",
            "merge pull request",
            "merge passport hash changed since the reviewer loaded the PR",
            &[
                "refresh the PR detail and re-check the merge passport",
                "rerun the mapped proof lane before retrying merge",
            ],
            PROOF_LANE,
            Some(json!({
                "expected_head_sha": request.expected_head_sha,
                "current_head_sha": pr.head.sha,
            })),
        );
    }
    if current.merge_passport.status != MergePassportStatus::Pass {
        return repair_error(
            StatusCode::CONFLICT,
            "merge_blocked",
            "merge pull request",
            "the current exact-head merge passport is blocked",
            &[
                "refresh the PR detail and resolve every merge-passport blocker",
                "rerun the mapped proof lane before retrying merge",
            ],
            PROOF_LANE,
            Some(json!({
                "expected_head_sha": request.expected_head_sha,
                "passport_status": current.merge_passport.status,
                "blockers": current.merge_passport.blockers,
            })),
        );
    }
    let merge_payload = CoreMergePullRequestRequest {
        commit_title: request.commit_title,
        commit_message: request.commit_message,
        sha: Some(request.expected_head_sha),
        merge_method: request.merge_method,
    };
    let merge_body = match serde_json::to_string(&merge_payload) {
        Ok(body) => body,
        Err(error) => {
            return repair_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "pull_request_serialize_failed",
                "merge pull request",
                &format!("could not serialize merge request: {error}"),
                &["retry the merge after refreshing the PR detail"],
                PROOF_LANE,
                Some(json!({ "head_sha": pr.head.sha })),
            );
        }
    };
    let merged = state.github.put(
        &format!(
            "/repos/{}/{}/pulls/{}/merge",
            repo.owner, repo.name, pr.number
        ),
        &merge_body,
    );
    if merged.status != 200 {
        return github_merge_error(merged, &pr);
    }
    match state
        .github
        .core()
        .get_pull_request(&repo.owner, &repo.name, pr.number)
    {
        Ok(updated) => {
            super::pipeline::emit::pull_merged(&state, &updated, &account.login, "merge");
            Json(detail_for_pr(&state, &updated, None)).into_response()
        }
        Err(error) => core_error(error, "reload pull request after merge"),
    }
}

/// `POST /api/v1/repos/:id/pulls/:number/ready`: the draft is ready for review.
pub(super) async fn ready_for_review(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
) -> AxumResponse {
    set_draft(&state, &account, &id, number, false)
}

/// `POST /api/v1/repos/:id/pulls/:number/draft`: back to a draft.
pub(super) async fn convert_to_draft(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
) -> AxumResponse {
    set_draft(&state, &account, &id, number, true)
}

/// Both draft transitions. They are idempotent: a pull request already in the
/// asked-for state answers 200 with its current detail and records nothing, so
/// a double click neither fails nor writes a second audit row.
///
/// `PATCH /api/v3/repos/{owner}/{repo}/pulls/{number}` with `{"draft": …}`
/// stays the `gh`-compatible way in; it lands on the same core transition and
/// the same timeline event through [`super::pipeline::emit::github_edge`].
fn set_draft(
    state: &WebState,
    account: &AccountSummary,
    id: &str,
    number: u64,
    draft: bool,
) -> AxumResponse {
    let purpose = if draft {
        "convert pull request to draft"
    } else {
        "mark pull request ready for review"
    };
    let Some((repo, pr)) = resolve_pr(state, id, number) else {
        return not_found(purpose, "pull request not found");
    };
    // A merged or closed pull request has no draft question left.
    if !matches!(web_pr_state(&pr), WebPullRequestState::Open) {
        return repair_error(
            StatusCode::CONFLICT,
            "pull_draft_not_open",
            purpose,
            "only an open pull request can change its draft state",
            &[
                "reopen the pull request before changing its draft state",
                "open a new pull request for the same head",
            ],
            PROOF_LANE,
            Some(json!({ "state": web_pr_state(&pr) })),
        );
    }
    // The author owns their own draft; an admin can move anyone's, which is
    // how a stranded draft gets unstuck when its author is away.
    if pr.author != account.login && account.role != UserRole::Admin {
        return repair_error(
            StatusCode::FORBIDDEN,
            "pull_draft_forbidden",
            purpose,
            "only the pull request author or an admin can change its draft state",
            &[
                "ask the pull request author to mark it ready for review",
                "act as an admin account to move someone else's draft",
            ],
            PROOF_LANE,
            Some(json!({ "author": pr.author, "actor": account.login })),
        );
    }
    if pr.draft == draft {
        return Json(detail_for_pr(state, &pr, Some(&account.login))).into_response();
    }
    let updated = match state.github.core().update_pull_request(
        &repo.owner,
        &repo.name,
        pr.number,
        UpdatePullRequestRequest {
            draft: Some(draft),
            ..UpdatePullRequestRequest::default()
        },
    ) {
        Ok(updated) => updated,
        Err(error) => return core_error(error, purpose),
    };
    // The audit row is the transition's own record, separate from the pipeline
    // event the PR timeline renders; neither failing can fail the request.
    let _ = state.github.core().append_audit_as(
        &account.login,
        if draft {
            "pull_request.convert_to_draft"
        } else {
            "pull_request.ready_for_review"
        },
        &format!("{}/{}#{}", repo.owner, repo.name, pr.number),
        "completed",
        json!({
            "draft": draft,
            "head_sha": updated.head.sha,
            "author": updated.author,
        }),
    );
    super::pipeline::emit::pull_draft_changed(state, &updated, &account.login);
    Json(detail_for_pr(state, &updated, Some(&account.login))).into_response()
}

fn resolve_pr(
    state: &WebState,
    id: &str,
    number: u64,
) -> Option<(jeryu_core::Repository, PullRequest)> {
    let repo = find_repo(state, id)?;
    let pr = state
        .github
        .core()
        .get_pull_request(&repo.owner, &repo.name, number)
        .ok()?;
    Some((repo, pr))
}

/// Why an approval was refused by the quality gate, with the proof's own words
/// and a link to the report, so the pull request shows why without a hunt.
fn jankurai_gate_refusal(pr: &PullRequest, verdict: &posture::JankuraiGateVerdict) -> AxumResponse {
    repair_error(
        StatusCode::CONFLICT,
        "approval_blocked_jankurai_proof",
        "approve pull request",
        &format!(
            "`jankurai/proof` {}: {}",
            verdict.state_phrase(),
            verdict.reason
        ),
        &[
            "open the Quality gate page of this head and clear what the audit lists",
            "push the fix; the new head is scored and can then be approved",
        ],
        PROOF_LANE,
        Some(json!({
            "head_sha": pr.head.sha,
            "check": "jankurai/proof",
            "check_state": verdict.state.wire_name(),
            "reason": verdict.reason,
            "report_url": verdict.details_url,
        })),
    )
}

fn self_approval_forbidden(pr: &PullRequest, reviewer: &str) -> Option<AxumResponse> {
    if pr.author != reviewer {
        return None;
    }
    Some(repair_error(
        StatusCode::FORBIDDEN,
        "pull_self_approval_forbidden",
        "approve pull request",
        "pull request authors cannot approve their own changes",
        &[
            "request approval from an authenticated reviewer distinct from the pull request author",
            "retry with the same expected head after the independent reviewer signs in",
        ],
        PROOF_LANE,
        Some(json!({
            "pull_number": pr.number,
            "author": pr.author,
            "reviewer": reviewer,
        })),
    ))
}

fn state_matches(pr: &PullRequest, filter: Option<&str>) -> bool {
    match filter.unwrap_or("all") {
        "open" => {
            !pr.merged
                && !matches!(
                    pr.state,
                    jeryu_core::PullRequestState::Closed | jeryu_core::PullRequestState::Merged
                )
        }
        "closed" => matches!(pr.state, jeryu_core::PullRequestState::Closed),
        "merged" => pr.merged || matches!(pr.state, jeryu_core::PullRequestState::Merged),
        // `list` refuses any other value before filtering.
        _ => true,
    }
}

fn detail_for_pr(
    state: &WebState,
    pr: &PullRequest,
    authenticated_login: Option<&str>,
) -> PullRequestDetail {
    let required_contexts = required_contexts(state, pr);
    detail_for_pr_with_required_contexts(state, pr, &required_contexts, authenticated_login)
}

/// The merge gate as the merge queue sees it: whether the PR's exact-head
/// merge passport passes (with the blocker messages when it does not), and the
/// names of the required contexts, which the queue also requires on the
/// queued commit before landing it.
pub(super) fn queue_gate(state: &WebState, pr: &PullRequest) -> (bool, Vec<String>, Vec<String>) {
    let required = required_contexts(state, pr);
    let detail = detail_for_pr_with_required_contexts(state, pr, &required, None);
    let pass = detail.merge_passport.status == MergePassportStatus::Pass;
    let blockers = detail
        .merge_passport
        .blockers
        .iter()
        .map(|blocker| blocker.message.clone())
        .collect();
    (
        pass,
        blockers,
        required.into_iter().map(|context| context.name).collect(),
    )
}

/// The refusal an approval would get from the gate, with the enforcement
/// decision injected: the process environment is never mutated by a test.
#[cfg(test)]
pub(super) fn jankurai_gate_refusal_with(
    state: &WebState,
    pr: &PullRequest,
    enforced: bool,
) -> Option<AxumResponse> {
    posture::jankurai_gate_blocker_with(state, pr, enforced)
        .map(|verdict| jankurai_gate_refusal(pr, &verdict))
}

#[cfg(test)]
pub(super) fn detail_for_pr_with_audit_enforcement(
    state: &WebState,
    pr: &PullRequest,
    audit_enforce_merge: bool,
) -> PullRequestDetail {
    let required_contexts = required_contexts_with_enforcement(state, pr, audit_enforce_merge);
    detail_for_pr_with_required_contexts(state, pr, &required_contexts, None)
}

fn detail_for_pr_with_required_contexts(
    state: &WebState,
    pr: &PullRequest,
    required_contexts: &[RequiredContextPosture],
    authenticated_login: Option<&str>,
) -> PullRequestDetail {
    let mut summary = summary_with_required_contexts(state, pr, required_contexts);
    let merge_passport = passport(&summary, pr, required_contexts);
    let reviews = reviews_for_pr(state, pr);
    summary.review.user_review_state = authenticated_login.and_then(|login| {
        reviews
            .iter()
            .find(|review| review.effective && review.author == login)
            .map(|review| review.state.clone())
    });
    PullRequestDetail {
        passport_hash: summary.passport_hash.clone(),
        summary,
        description: pr.body.clone(),
        head_tree_sha: commit_tree_sha(state, pr, &pr.head.sha),
        base_tree_sha: commit_tree_sha(state, pr, &pr.base.sha),
        reviews,
        merge_passport,
    }
}

fn summary(state: &WebState, pr: &PullRequest) -> PullRequestSummary {
    let required_contexts = required_contexts(state, pr);
    summary_with_required_contexts(state, pr, &required_contexts)
}

fn summary_with_required_contexts(
    state: &WebState,
    pr: &PullRequest,
    required_contexts: &[RequiredContextPosture],
) -> PullRequestSummary {
    let repo = find_repo(state, &format!("{}/{}", pr.owner, pr.repo))
        .expect("PR owner/repo must resolve to a repository");
    let checks = checks_for_pr(state, pr);
    let review = review_posture(state, pr);
    let web_state = web_pr_state(pr);
    let mergeable = !pr.draft
        && !pr.merged
        && matches!(web_state, WebPullRequestState::Open)
        && pr.mergeable
        && required_contexts
            .iter()
            .all(|context| context.state == RequiredContextState::Passing)
        && review.approvals >= review.required_approvals
        && review.changes_requested == 0
        && review.unresolved_threads == 0;
    let reason = if mergeable {
        None
    } else if pr.draft {
        Some("draft pull request".to_string())
    } else if let Some(context) = required_contexts
        .iter()
        .find(|context| context.state != RequiredContextState::Passing)
    {
        Some(format!(
            "required context {} is {}",
            context.name,
            context.state.wire_name()
        ))
    } else if review.changes_requested > 0 {
        Some("changes requested on the current head".to_string())
    } else if review.approvals < review.required_approvals {
        Some("required approvals missing".to_string())
    } else if review.unresolved_threads > 0 {
        Some("unresolved review threads".to_string())
    } else if !pr.mergeable {
        Some(pr.mergeable_state.clone())
    } else {
        None
    };
    let status = if mergeable {
        MergePassportStatus::Pass
    } else {
        MergePassportStatus::Blocked
    };
    let blockers = passport_blockers(required_contexts, &review, pr);
    let passport_hash = passport_hash(
        state,
        pr,
        status.clone(),
        &blockers,
        &review,
        required_contexts,
    );
    PullRequestSummary {
        repo: repo_id(&repo),
        number: pr.number as u32,
        entity: EntityHandle {
            kind: "pull_request".to_string(),
            id: format!("{}#{}", repo.id, pr.number),
        },
        title: pr.title.clone(),
        author: pr.author.clone(),
        head_ref: pr.head.ref_name.clone(),
        base_ref: pr.base.ref_name.clone(),
        head_sha: pr.head.sha.clone(),
        base_sha: pr.base.sha.clone(),
        state: web_state.clone(),
        draft: pr.draft,
        mergeable: Mergeability {
            level: if mergeable { "mergeable" } else { "blocked" }.to_string(),
            can_merge: mergeable,
            reason,
            exact_head_sha: pr.head.sha.clone(),
            required_gate: if mergeable {
                None
            } else {
                Some("merge_passport".to_string())
            },
        },
        review,
        checks: CheckPosture {
            total: checks.total,
            passing: checks.passing,
            failing: checks.failing,
            pending: checks.pending,
            skipped: checks.skipped,
        },
        agents: AgentPosture {
            active_sessions: 0,
            proposed_patches: 0,
            evidence_packets: 0,
            blockers: 0,
        },
        labels: Vec::new(),
        updated_at: pr.updated_at.to_rfc3339(),
        passport_hash: Some(passport_hash),
        available_actions: available_actions(pr, &web_state),
    }
}

/// The route that marks a draft ready for review. The passport's
/// `passport_blocked_draft` blocker and the `pull.ready_for_review` action
/// both name it, so a client that sees the rule also sees how to clear it.
pub(super) fn ready_route(pr: &PullRequest) -> String {
    format!(
        "/api/v1/repos/{}/{}/pulls/{}/ready",
        pr.owner, pr.repo, pr.number
    )
}

/// The route that puts an open pull request back into draft.
fn draft_route(pr: &PullRequest) -> String {
    format!(
        "/api/v1/repos/{}/{}/pulls/{}/draft",
        pr.owner, pr.repo, pr.number
    )
}

/// What the viewer may do with this pull request, each action carrying the
/// route that performs it. The draft pair is exclusive: a draft offers "Ready
/// for review", an open pull request offers "Convert to draft".
fn available_actions(pr: &PullRequest, state: &WebPullRequestState) -> Vec<AvailableAction> {
    let mut actions = vec![
        AvailableAction::new("pull.approve", "Approve", None),
        AvailableAction::new("pull.merge", "Merge", Some("medium")),
    ];
    if *state == WebPullRequestState::Open {
        actions.push(if pr.draft {
            AvailableAction::new("pull.ready_for_review", "Ready for review", None)
                .route("POST", ready_route(pr))
        } else {
            AvailableAction::new("pull.convert_to_draft", "Convert to draft", Some("low"))
                .route("POST", draft_route(pr))
        });
    }
    actions
}

/// Where an open pull request's merge gate stands, for the attention inbox.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PullPosture {
    pub can_merge: bool,
    pub changes_requested: u32,
    pub approvals: u32,
    pub required_approvals: u32,
    /// Required contexts that failed: these block the merge.
    pub failing: Vec<String>,
    /// Failing checks the base branch does not require: red, but the pull
    /// request can still merge.
    pub failing_optional: Vec<String>,
    /// Something was checked, nothing failed and nothing is still running.
    pub checks_green: bool,
}

/// The posture of an open, non-draft pull request; `None` for any other.
pub(super) fn attention_posture(state: &WebState, pr: &PullRequest) -> Option<PullPosture> {
    if pr.draft || pr.merged || !matches!(web_pr_state(pr), WebPullRequestState::Open) {
        return None;
    }
    find_repo(state, &format!("{}/{}", pr.owner, pr.repo))?;
    let required = required_contexts(state, pr);
    let summary = summary_with_required_contexts(state, pr, &required);
    let mut failing: Vec<String> = required
        .iter()
        .filter(|context| context.state == RequiredContextState::Failing)
        .map(|context| context.name.clone())
        .collect();
    failing.sort();
    let mut failing_optional: Vec<String> = checks_for_pr(state, pr)
        .checks
        .into_iter()
        .filter(|check| check.status == "failure")
        .map(|check| check.name)
        .filter(|name| !required.iter().any(|context| &context.name == name))
        .collect();
    failing_optional.sort();
    failing_optional.dedup();
    if failing.is_empty() && failing_optional.is_empty() && summary.checks.failing > 0 {
        failing_optional.push(format!("{} check(s)", summary.checks.failing));
    }
    let required_passing = required
        .iter()
        .all(|context| context.state == RequiredContextState::Passing);
    Some(PullPosture {
        can_merge: summary.mergeable.can_merge,
        changes_requested: summary.review.changes_requested,
        approvals: summary.review.approvals,
        required_approvals: summary.review.required_approvals,
        checks_green: failing.is_empty()
            && required_passing
            && summary.checks.pending == 0
            && (!required.is_empty() || summary.checks.total > 0),
        failing,
        failing_optional,
    })
}

pub(in crate::web) mod diff;
mod posture;

#[cfg(test)]
use posture::required_contexts_with_enforcement;
#[cfg(test)]
pub(super) use posture::{audit_gate_repo_listed, audit_merge_enforced_value};
use posture::{
    checks_for_pr, comment_input, commit_tree_sha, passport, passport_blockers, passport_hash,
    required_contexts, review_posture, review_state, reviews_for_pr, threads_for_pr, web_pr_state,
};

fn not_found(purpose: &'static str, message: &str) -> AxumResponse {
    repair_error(
        StatusCode::NOT_FOUND,
        "not_found",
        purpose,
        message,
        &[
            "verify the repository id and pull request number",
            "refresh the local forge import before retrying",
        ],
        PROOF_LANE,
        None,
    )
}

fn stale_sha(expected: &str, current: &str) -> AxumResponse {
    repair_error(
        StatusCode::CONFLICT,
        "merge_sha_stale",
        "guard pull request mutation by exact head sha",
        "expected_head_sha does not match the current PR head",
        &[
            "refresh the PR detail and re-review the current head",
            "retry the mutation with the current expected_head_sha",
        ],
        PROOF_LANE,
        Some(json!({
            "expected_head_sha": expected,
            "current_head_sha": current,
        })),
    )
}

fn core_error(error: ForgeError, purpose: &'static str) -> AxumResponse {
    match error {
        ForgeError::NotFound(reason) => not_found(purpose, &reason),
        ForgeError::Validation(reason) => repair_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            purpose,
            &reason,
            &[
                "check request fields before retrying",
                "add or rerun a boundary test for the rejected shape",
            ],
            PROOF_LANE,
            None,
        ),
        ForgeError::BranchProtection(reason) => repair_error(
            StatusCode::CONFLICT,
            "merge_blocked",
            purpose,
            &reason,
            &[
                "inspect branch protection and merge passport blockers",
                "supply required checks, approvals, or proof evidence",
            ],
            PROOF_LANE,
            None,
        ),
        ForgeError::Conflict(reason) => repair_error(
            StatusCode::CONFLICT,
            "conflict",
            purpose,
            &reason,
            &[
                "refresh the pull request before retrying",
                "recompute merge evidence for the current head",
            ],
            PROOF_LANE,
            None,
        ),
        ForgeError::Forbidden(reason) => repair_error(
            StatusCode::FORBIDDEN,
            "forbidden",
            purpose,
            &reason,
            &[
                "ask a different reviewer: authors cannot approve their own changes",
                "act as an account with the right on this repository",
            ],
            PROOF_LANE,
            None,
        ),
        ForgeError::RepositoryArchived(reason) => repair_error(
            StatusCode::CONFLICT,
            "repository_archived",
            purpose,
            &reason,
            &[
                "unarchive the repository before changing its pull requests",
                "retry the flow against an active repository",
            ],
            PROOF_LANE,
            None,
        ),
        ForgeError::Storage(reason) => repair_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            purpose,
            &reason,
            &[
                "check the local SQLite store and filesystem permissions",
                "restart the local API after verifying storage health",
            ],
            PROOF_LANE,
            None,
        ),
    }
}

fn github_merge_error(response: crate::Response, pr: &PullRequest) -> AxumResponse {
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let repair_status = if status == StatusCode::METHOD_NOT_ALLOWED {
        StatusCode::CONFLICT
    } else {
        status
    };
    let message = serde_json::from_str::<Value>(&response.body)
        .ok()
        .and_then(|body| {
            body.get("message")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .filter(|message| !message.trim().is_empty())
        .unwrap_or(response.body);
    let code = match status {
        StatusCode::METHOD_NOT_ALLOWED | StatusCode::CONFLICT => "merge_blocked",
        StatusCode::UNPROCESSABLE_ENTITY => "merge_unprocessable",
        StatusCode::NOT_FOUND => "not_found",
        _ => "merge_failed",
    };
    repair_error(
        repair_status,
        code,
        "merge pull request",
        &message,
        &[
            "inspect the merge passport blockers before retrying",
            "rerun required checks and collect approvals for the current head",
        ],
        PROOF_LANE,
        Some(json!({ "head_sha": pr.head.sha })),
    )
}

fn repair_error(
    status: StatusCode,
    code: &'static str,
    purpose: &'static str,
    reason: &str,
    common_fixes: &'static [&'static str],
    repair_hint: &'static str,
    details: Option<Value>,
) -> AxumResponse {
    let error = json!({
        "code": code,
        "message": reason,
        "details": match details {
            Some(details) => details,
            None => json!({}),
        },
        "request_id": format!("pulls-{}", server_time()),
    });
    (
        status,
        Json(json!({
            "error": error,
            "code": code,
            "message": reason,
            "purpose": purpose,
            "reason": reason,
            "common_fixes": common_fixes,
            "docs_url": DOCS_URL,
            "repair_hint": repair_hint,
        })),
    )
        .into_response()
}
