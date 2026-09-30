//! The authoritative path for a jankurai score now that the audit runs on a
//! gate runner: claim a ticket, submit a report bound to it, and have the forge
//! judge the report. Everything else is refused.

use super::*;
use crate::ci_bridge::audit_queue::{self, AuditTicket};
use crate::web::jankurai::audits::{claim, list};
use jeryu_core::CheckRunStatus;
use serde_json::json;

const GOVERNED_VERSION: &str = "jankurai 1.6.11";
const GOVERNED_SHA256: &str = "9e6b8857a26f6004d4c74e510e13b06d880f2e2ae0c89502698889ed690c5d6c";

/// A repository plus one queued audit ticket for a unique head, so tests that
/// share the process-wide queue never see each other's work.
fn repo_with_ticket(name: &str, head: &str, base: &str) -> (Arc<WebState>, AuditTicket) {
    let core = ForgeCore::new();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: name.to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let ticket = AuditTicket::new("jeryu", name, "codex/feature", head, base);
    audit_queue::queue().lock().unwrap().enqueue(ticket.clone());
    (Arc::new(WebState::new(core)), ticket)
}

fn submission(ticket: &AuditTicket, score: u64) -> serde_json::Value {
    json!({
        "branch": ticket.branch,
        "commit_sha": ticket.head_sha,
        "base_sha": ticket.base_sha,
        "runner_id": "xbabe2/slot0",
        "jankurai_version": GOVERNED_VERSION,
        "jankurai_sha256": GOVERNED_SHA256,
        "jankurai_receipt_sha256": "a".repeat(64),
        "tool_exit": 0,
        "report": {
            "score": score,
            "caps_applied": [],
            "decision": {"hard_findings": 0, "minimum_score": 85}
        }
    })
}

async fn ingest(
    state: &Arc<WebState>,
    account: Extension<AccountSummary>,
    repo: &str,
    body: &serde_json::Value,
) -> AxumResponse {
    repo_jankurai_scores_ingest(
        State(state.clone()),
        account,
        AxumPath(format!("jeryu/{repo}")),
        axum::body::Bytes::from(body.to_string()),
    )
    .await
}

/// The happy path: a runner claims the head the push queued, submits the
/// governed auditor's report, and the forge records the score and completes
/// `jankurai/proof` from the report it was handed.
#[tokio::test]
async fn a_runner_report_for_an_open_ticket_becomes_the_head_proof() {
    let head = "1".repeat(40);
    let base = "2".repeat(40);
    let (state, ticket) = repo_with_ticket("audit-happy", &head, &base);

    let claimed = response_json(
        claim(
            State(state.clone()),
            authenticated_account("gatebot"),
            axum::body::Bytes::from(json!({"runner_id": "xbabe2/slot0", "max": 8}).to_string()),
        )
        .await,
    )
    .await;
    assert!(
        claimed["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|claimed| claimed["headSha"] == head),
        "the runner is handed the queued head: {claimed}"
    );

    let accepted = ingest(
        &state,
        authenticated_account("gatebot"),
        "audit-happy",
        &submission(&ticket, 92),
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::CREATED);

    let core = state.github.core();
    let scores = core
        .list_jankurai_scores("jeryu", "audit-happy", None, Some(&head))
        .unwrap();
    assert_eq!(scores.len(), 1);
    assert_eq!(scores[0].score, Some(92));
    let report = scores[0].report_json.as_deref().expect("report is kept");
    assert!(
        report.contains("xbabe2/slot0") && report.contains(&base),
        "the score keeps the provenance of the run: {report}"
    );
    let checks = core
        .list_check_runs("jeryu", "audit-happy", Some(&head))
        .unwrap();
    let proof = checks
        .check_runs
        .iter()
        .find(|check| check.name == "jankurai/proof")
        .expect("the proof is completed");
    assert_eq!(proof.status, CheckRunStatus::Completed);
    assert_eq!(proof.conclusion, Some(CheckConclusion::Success));

    // The ticket is spent: the same head cannot be scored twice, so the forge
    // and `<repo>/required` between them audit a head once.
    let again = ingest(
        &state,
        authenticated_account("gatebot"),
        "audit-happy",
        &submission(&ticket, 99),
    )
    .await;
    assert_eq!(again.status(), StatusCode::CONFLICT);
    assert_eq!(
        core.list_jankurai_scores("jeryu", "audit-happy", None, Some(&head))
            .unwrap()[0]
            .score,
        Some(92)
    );
}

/// A forged or mismatched report is refused, and nothing is recorded: no
/// score, and no `jankurai/proof` that could be read as a pass.
#[tokio::test]
async fn forged_and_mismatched_reports_are_rejected() {
    let head = "3".repeat(40);
    let base = "4".repeat(40);
    let (state, ticket) = repo_with_ticket("audit-forged", &head, &base);

    let mut wrong_binary = submission(&ticket, 100);
    wrong_binary["jankurai_sha256"] = json!("0".repeat(64));
    let mut wrong_version = submission(&ticket, 100);
    wrong_version["jankurai_version"] = json!("jankurai 1.6.10");
    let mut wrong_head = submission(&ticket, 100);
    wrong_head["commit_sha"] = json!("5".repeat(40));
    let mut wrong_base = submission(&ticket, 100);
    wrong_base["base_sha"] = json!("6".repeat(40));
    let mut short_sha = submission(&ticket, 100);
    short_sha["commit_sha"] = json!("abc");
    let mut unknown_mode = submission(&ticket, 100);
    unknown_mode["audit_mode"] = json!("whatever");

    for (case, body, expected) in [
        (
            "another binary",
            wrong_binary,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "another version",
            wrong_version,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("another head", wrong_head, StatusCode::CONFLICT),
        ("another base", wrong_base, StatusCode::CONFLICT),
        ("a partial sha", short_sha, StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let response = ingest(
            &state,
            authenticated_account("gatebot"),
            "audit-forged",
            &body,
        )
        .await;
        assert_eq!(response.status(), expected, "{case} must be refused");
    }

    // An account that is not a runner cannot hand in a score at all.
    let outsider = ingest(
        &state,
        authenticated_account("agent"),
        "audit-forged",
        &submission(&ticket, 100),
    )
    .await;
    assert_eq!(outsider.status(), StatusCode::FORBIDDEN);

    let core = state.github.core();
    assert!(
        core.list_jankurai_scores("jeryu", "audit-forged", None, Some(&head))
            .unwrap()
            .is_empty(),
        "a refused report is never recorded"
    );
    assert_eq!(
        core.list_check_runs("jeryu", "audit-forged", Some(&head))
            .unwrap()
            .total_count,
        0,
        "a refused report publishes no proof"
    );
    // The rejections left the real job claimable.
    let good = ingest(
        &state,
        authenticated_account("gatebot"),
        "audit-forged",
        &submission(&ticket, 90),
    )
    .await;
    assert_eq!(good.status(), StatusCode::CREATED);
}

/// The queue is runner-facing: an ordinary account can neither see it nor
/// take work from it.
#[tokio::test]
async fn the_audit_queue_is_runner_only() {
    let head = "7".repeat(40);
    let (state, _) = repo_with_ticket("audit-queue-acl", &head, &"8".repeat(40));

    for response in [
        list(State(state.clone()), authenticated_account("agent")).await,
        claim(
            State(state.clone()),
            authenticated_account("agent"),
            axum::body::Bytes::from(json!({"runner_id": "agent"}).to_string()),
        )
        .await,
    ] {
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let visible =
        response_json(list(State(state), authenticated_admin_account("alton2")).await).await;
    assert!(
        visible["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|ticket| ticket["headSha"] == head)
    );
    audit_queue::queue()
        .lock()
        .unwrap()
        .take("jeryu", "audit-queue-acl", &head);
}
