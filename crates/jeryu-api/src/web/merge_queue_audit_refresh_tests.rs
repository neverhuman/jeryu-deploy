//! A running audit must not retain a diff that includes newly landed entries.
use super::*;
use crate::ci_bridge;

pub(super) async fn ingest(fx: &Fixture, login: &str, submission: &Value) -> StatusCode {
    repo_jankurai_scores_ingest(
        State(fx.state.clone()),
        super::super::merge_queue_tests::account(login, UserRole::Admin),
        AxumPath("alice/jeryu".into()),
        axum::body::Bytes::from(submission.to_string()),
    )
    .await
    .status()
}

pub(super) fn submission(ticket: &audit_queue::AuditTicket) -> Value {
    serde_json::json!({
        "branch": ticket.branch, "commit_sha": ticket.head_sha, "base_sha": ticket.base_sha,
        "runner_id": "refresh-runner", "audit_mode": ticket.audit_mode,
        "jankurai_version": ci_bridge::jankurai::governed_auditor_identity().unwrap().0,
        "jankurai_sha256": ci_bridge::GOVERNED_JANKURAI_SHA256,
        "report": {"score": 98, "caps_applied": [],
            "decision": {"hard_findings": 0, "minimum_score": 85}},
        "tool_exit": 0
    })
}

pub(super) fn ticket(sha: &str) -> audit_queue::AuditTicket {
    audit_queue::queue()
        .lock()
        .unwrap()
        .tickets()
        .iter()
        .find(|ticket| ticket.head_sha == sha)
        .unwrap()
        .clone()
}

#[tokio::test]
async fn landing_refreshes_claimed_stack_diff_and_rejects_previous_report() {
    let mut fx = Fixture::with_audit_store("refresh-audit.txt", "refresh audit\n");
    let base = fx.advance_main("refresh-base.txt", "base\n");
    isolate_base(&mut fx, "audit-refresh");
    let second = add_audited_pr(
        &fx,
        "audit-refresh",
        "refresh-two",
        "refresh-two.txt",
        "bob",
    );
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let bottom = body["queue_sha"].as_str().unwrap().to_string();
    let (status, body) = enqueue_number(&fx, second).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let top = body["queue_sha"].as_str().unwrap().to_string();
    let previous = ticket(&top);
    assert_eq!(previous.base_sha, base);
    // Model the lease of a runner already auditing this exact ticket, without
    // claiming unrelated fixtures' jobs from the process-wide queue.
    {
        let mut queue = audit_queue::queue().lock().unwrap();
        let held = queue
            .tickets_for_test()
            .iter_mut()
            .find(|t| t.head_sha == top)
            .unwrap();
        held.claimed_by = Some("refresh-runner".into());
        held.claimed_at = Some(chrono::Utc::now());
    }
    assert_eq!(
        ingest(&fx, "independent-runner", &submission(&ticket(&bottom))).await,
        StatusCode::CREATED
    );
    fx.status(&bottom, jeryu_core::CommitStatusState::Success);
    fx.status(&top, jeryu_core::CommitStatusState::Success);
    assert_eq!(merge_queue::tick(&fx.state), 1);
    let refreshed = ticket(&top);
    assert_eq!(refreshed.head_sha, previous.head_sha);
    assert_eq!(refreshed.base_sha, bottom);
    assert!(refreshed.claimed_by.is_none());
    assert!(refreshed.claimed_at.is_none());
    // A landed author still cannot submit the previous, wider diff, even in
    // the interval between landing and ticket replacement.
    assert_eq!(
        ingest(&fx, "alice", &submission(&previous)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        ingest(&fx, "independent-runner", &submission(&previous)).await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        ticket(&top),
        refreshed,
        "rejection preserves the replacement"
    );
    assert_eq!(
        merge_queue::tick(&fx.state),
        0,
        "replacement needs real proof"
    );
    let mut report = submission(&refreshed);
    report["jankurai_sha256"] = "0".repeat(64).into();
    assert_eq!(
        ingest(&fx, "alice", &report).await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        ingest(&fx, "bob", &submission(&refreshed)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        ingest(&fx, "alice", &submission(&refreshed)).await,
        StatusCode::CREATED
    );
    let scores = fx
        .state
        .core
        .list_jankurai_scores("alice", "jeryu", None, Some(&top))
        .unwrap();
    assert_eq!(scores.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(scores[0].report_json.as_deref().unwrap()).unwrap()["jeryu_audit_provenance"]
            ["base_sha"],
        bottom
    );
    assert_eq!(merge_queue::tick(&fx.state), 1);
}

#[test]
fn refreshing_tickets_revokes_changed_claims_and_preserves_identical_claims() {
    let mut queue = audit_queue::AuditQueue::default();
    let original = audit_queue::AuditTicket::new(
        "alice",
        "repo",
        "refs/queue/main/2",
        &"a".repeat(40),
        &"b".repeat(40),
    );
    assert_eq!(
        queue.enqueue(original.clone()),
        audit_queue::EnqueueOutcome::Queued
    );
    let mut refreshed = original;
    refreshed.base_sha = "c".repeat(40);
    assert_eq!(
        queue.enqueue(refreshed.clone()),
        audit_queue::EnqueueOutcome::Superseded(1)
    );
    assert_eq!(queue.claim("runner", 1)[0].base_sha, refreshed.base_sha);
    assert_eq!(
        queue.enqueue(refreshed.clone()),
        audit_queue::EnqueueOutcome::AlreadyQueued
    );
    assert_eq!(queue.tickets().len(), 1);
    assert_eq!(queue.tickets()[0].claimed_by.as_deref(), Some("runner"));
    refreshed.base_sha = "d".repeat(40);
    assert_eq!(
        queue.enqueue(refreshed.clone()),
        audit_queue::EnqueueOutcome::Superseded(1)
    );
    let claimed = queue.claim("next-runner", 1);
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].base_sha, refreshed.base_sha);
    assert_eq!(claimed[0].claimed_by.as_deref(), Some("next-runner"));
    assert_eq!(queue.tickets().len(), 1);
}

#[tokio::test]
async fn landing_re_audits_a_queue_head_whose_proof_covered_the_landed_entry() {
    use super::merge_queue_tests::git;
    let mut fx = Fixture::with_audit_store("reaudit-audit.txt", "reaudit audit\n");
    let base = fx.advance_main("reaudit-base.txt", "base\n");
    isolate_base(&mut fx, "audit-reaudit");
    let second = add_audited_pr(
        &fx,
        "audit-reaudit",
        "reaudit-two",
        "reaudit-two.txt",
        "bob",
    );
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let bottom = body["queue_sha"].as_str().unwrap().to_string();
    let (status, body) = enqueue_number(&fx, second).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let top = body["queue_sha"].as_str().unwrap().to_string();
    let wide = ticket(&top);
    assert_eq!(wide.base_sha, base);
    // An independent runner proves both entries against the base below the
    // stack, and only the bottom entry's own gate has reported.
    assert_eq!(
        ingest(&fx, "independent-runner", &submission(&ticket(&bottom))).await,
        StatusCode::CREATED
    );
    assert_eq!(
        ingest(&fx, "independent-runner", &submission(&wide)).await,
        StatusCode::CREATED
    );
    fx.status(&bottom, jeryu_core::CommitStatusState::Success);
    assert_eq!(merge_queue::tick(&fx.state), 1);
    assert_eq!(
        git(&fx.bare, &["rev-parse", "refs/heads/audit-reaudit"]),
        bottom
    );
    // The top keeps its sha, so the proof it carries is the audit of a diff
    // that includes the landed entry. It is ticketed again against the new
    // base, and its proof reads pending until that audit comes back.
    let narrow = ticket(&top);
    assert_eq!(narrow.head_sha, top);
    assert_eq!(narrow.base_sha, bottom);
    assert!(narrow.claimed_by.is_none());
    let proof = vec![crate::ci_bridge::JANKURAI_PROOF_CHECK.to_string()];
    assert_eq!(
        merge_queue::gate_verdict(&fx.state.core, "alice", "jeryu", &top, &proof),
        None,
        "the replaced audit is not this diff's proof"
    );
    fx.status(&top, jeryu_core::CommitStatusState::Success);
    assert_eq!(
        merge_queue::tick(&fx.state),
        0,
        "a green gate plus a replaced proof does not land the entry"
    );
    // The report for the replaced base no longer matches the ticket, and the
    // landed entry's author may audit what is left of the diff.
    assert_eq!(
        ingest(&fx, "independent-runner", &submission(&wide)).await,
        StatusCode::CONFLICT
    );
    assert_eq!(ticket(&top), narrow, "a refusal keeps the open ticket");
    assert_eq!(
        ingest(&fx, "bob", &submission(&narrow)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        ingest(&fx, "alice", &submission(&narrow)).await,
        StatusCode::CREATED
    );
    let scores = fx
        .state
        .core
        .list_jankurai_scores("alice", "jeryu", None, Some(&top))
        .unwrap();
    let bases: Vec<Value> = scores
        .iter()
        .map(|score| {
            serde_json::from_str::<Value>(score.report_json.as_deref().unwrap()).unwrap()
                ["jeryu_audit_provenance"]["base_sha"]
                .clone()
        })
        .collect();
    assert!(
        bases.contains(&Value::from(bottom.as_str())),
        "the narrowed audit is recorded with its own base: {bases:?}"
    );
    assert_eq!(merge_queue::tick(&fx.state), 1);
    assert_eq!(
        git(&fx.bare, &["rev-parse", "refs/heads/audit-reaudit"]),
        top
    );
}
