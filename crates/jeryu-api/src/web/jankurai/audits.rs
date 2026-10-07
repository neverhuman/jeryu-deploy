//! The runner side of the audit queue.
//!
//! A gate runner claims tickets here, runs the governed auditor on its own
//! machine, and submits the report to
//! `POST /api/v1/repos/:id/jankurai-scores`. Nothing in this module runs an
//! audit: the forge only hands out work and judges what comes back.
//!
//! What makes a submitted report authoritative is checked in
//! [`authorize_runner_submission`]: a runner identity allowed to score, an open
//! ticket for exactly this branch, head and base, and the governed auditor's
//! version and sha256. Anything else is refused — never recorded as a pass.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use jeryu_core::{AccountSummary, ForgeCore};
use serde::{Deserialize, Serialize};

use super::{WebState, api_error};
use crate::ci_bridge::audit_queue::{
    self, AUDIT_MODE_FULL, AuditTicket, MAX_CLAIM_BATCH, NO_COMMIT_BASE_OID,
};
use crate::ci_bridge::{is_object_id, verify_reported_auditor};

/// What a runner hands in for one audited head. The verdict is not part of it:
/// the forge reads the report itself, so a submission is evidence only.
#[derive(Debug, Deserialize)]
pub(crate) struct RunnerAuditSubmission {
    pub(crate) branch: String,
    pub(crate) commit_sha: String,
    /// The base the audit diffed against; it must be the base the forge asked
    /// for, so a report cannot be a cheap diff of the runner's choosing.
    pub(crate) base_sha: String,
    pub(crate) runner_id: String,
    pub(crate) jankurai_version: String,
    pub(crate) jankurai_sha256: String,
    /// `diff` (the queued job's own audit against its base) or `full` (a
    /// whole-tree audit of the head: what the `<repo>/required` gate produces,
    /// and the only audit a head with no commit base can get).
    #[serde(default = "default_audit_mode")]
    pub(crate) audit_mode: String,
    /// Content address of the runner's own installation receipt, kept with the
    /// score as the audit trail of which install produced the report.
    #[serde(default)]
    pub(crate) jankurai_receipt_sha256: Option<String>,
    #[serde(default)]
    pub(crate) report: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) tool_exit: Option<i64>,
}

/// Why a submission was refused, and the status it answers with.
pub(crate) struct SubmissionRejected {
    pub(crate) status: StatusCode,
    pub(crate) reason: String,
}

impl SubmissionRejected {
    fn new(status: StatusCode, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
        }
    }
}

/// Why the submitting account may not produce this head's score: it authored
/// the change the score is required evidence for. `jankurai/proof` is the one
/// required check the forge completes from a submission, so an author who also
/// holds a runner identity could otherwise hand in the evidence that clears
/// their own pull request. Principals are compared normalized, so a
/// differently-cased or aliased login is the same author.
///
/// Checked before the ticket is claimed, so a refusal leaves the open ticket
/// for the runner that may actually run it.
pub(crate) fn author_produced_submission(
    core: &ForgeCore,
    account: &AccountSummary,
    owner: &str,
    repo: &str,
    head_sha: &str,
) -> Option<SubmissionRejected> {
    let pulls = core.list_pull_requests(owner, repo, None).ok()?;
    let authored = pulls.into_iter().find(|pull| {
        pull.head.sha == head_sha
            && crate::web::principals::same_principal(&pull.author, &account.login)
    })?;
    Some(SubmissionRejected::new(
        StatusCode::FORBIDDEN,
        format!(
            "a jankurai report for {head_sha} cannot come from {}, the author of pull request #{}",
            account.login, authored.number
        ),
    ))
}

/// Decide whether this submission may become the head's authoritative score,
/// and consume the ticket that authorized it so the head is scored once.
pub(crate) fn authorize_runner_submission(
    account: &AccountSummary,
    owner: &str,
    repo: &str,
    submission: &RunnerAuditSubmission,
) -> Result<AuditTicket, SubmissionRejected> {
    if !super::super::auth::can_submit_runner_audit(account) {
        return Err(SubmissionRejected::new(
            StatusCode::FORBIDDEN,
            "jankurai audit submission requires a runner identity allowed to score",
        ));
    }
    if submission.runner_id.trim().is_empty() {
        return Err(SubmissionRejected::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "runner_id is required",
        ));
    }
    for (field, value) in [
        ("commit_sha", &submission.commit_sha),
        ("base_sha", &submission.base_sha),
    ] {
        if !is_object_id(value) {
            return Err(SubmissionRejected::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("{field} must be a full commit sha"),
            ));
        }
    }
    if let Some(receipt) = submission.jankurai_receipt_sha256.as_deref()
        && (receipt.len() != 64 || !receipt.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(SubmissionRejected::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "jankurai_receipt_sha256 must be a sha256 content address",
        ));
    }
    if !matches!(submission.audit_mode.as_str(), "diff" | "full") {
        return Err(SubmissionRejected::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "audit_mode must be diff or full",
        ));
    }
    verify_reported_auditor(&submission.jankurai_version, &submission.jankurai_sha256)
        .map_err(|error| SubmissionRejected::new(StatusCode::UNPROCESSABLE_ENTITY, error))?;

    let Ok(mut queue) = audit_queue::queue().lock() else {
        return Err(SubmissionRejected::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the audit queue is unavailable",
        ));
    };
    let Some(ticket) = queue.take(owner, repo, &submission.commit_sha) else {
        return Err(SubmissionRejected::new(
            StatusCode::CONFLICT,
            "no open audit job for this head",
        ));
    };
    // A head with no commit base is ticketed for a whole-tree audit; a diff
    // report for it can only be a diff against nothing, which scores nothing.
    if ticket.audit_mode == AUDIT_MODE_FULL && submission.audit_mode != AUDIT_MODE_FULL {
        let reason = format!(
            "audit job for {} has no commit base and expects a full audit, not a diff against {NO_COMMIT_BASE_OID}",
            ticket.head_sha
        );
        queue.enqueue(ticket);
        return Err(SubmissionRejected::new(StatusCode::CONFLICT, reason));
    }
    // The ticket is what the forge asked for; a report bound to anything else
    // is a different audit. Put the ticket back so the real one can still run.
    if ticket.branch != submission.branch || ticket.base_sha != submission.base_sha {
        let reason = format!(
            "audit job for {} expects branch {} against base {}",
            ticket.head_sha, ticket.branch, ticket.base_sha
        );
        queue.enqueue(ticket);
        return Err(SubmissionRejected::new(StatusCode::CONFLICT, reason));
    }
    Ok(ticket)
}

fn default_audit_mode() -> String {
    "diff".to_string()
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ClaimRequest {
    pub(crate) runner_id: String,
    #[serde(default)]
    pub(crate) max: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimResponse {
    lease_seconds: i64,
    tickets: Vec<AuditTicket>,
}

/// POST /api/v1/jankurai-audits/claim — a gate runner takes audit work.
pub(crate) async fn claim(
    State(_state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    body: Bytes,
) -> AxumResponse {
    if !super::super::auth::can_submit_runner_audit(&account) {
        return api_error(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "claiming jankurai audit work requires a runner identity allowed to score",
        );
    }
    let request: ClaimRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                &format!("claim body failed to parse: {error}"),
            );
        }
    };
    if request.runner_id.trim().is_empty() {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "runner_id is required",
        );
    }
    let Ok(mut queue) = audit_queue::queue().lock() else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            "the audit queue is unavailable",
        );
    };
    let tickets = queue.claim(
        &request.runner_id,
        request.max.unwrap_or(1).min(MAX_CLAIM_BATCH),
    );
    Json(ClaimResponse {
        lease_seconds: audit_queue::CLAIM_LEASE_SECONDS,
        tickets,
    })
    .into_response()
}

/// GET /api/v1/jankurai-audits — what still waits for a runner.
pub(crate) async fn list(
    State(_state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> AxumResponse {
    if !super::super::auth::can_submit_runner_audit(&account) {
        return api_error(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "the jankurai audit queue is visible to runners and admins",
        );
    }
    let Ok(queue) = audit_queue::queue().lock() else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            "the audit queue is unavailable",
        );
    };
    Json(ClaimResponse {
        lease_seconds: audit_queue::CLAIM_LEASE_SECONDS,
        tickets: queue.tickets().to_vec(),
    })
    .into_response()
}
