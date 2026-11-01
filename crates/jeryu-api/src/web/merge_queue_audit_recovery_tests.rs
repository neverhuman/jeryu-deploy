//! Lost proof publication must reuse only a governed verdict for this base.
use super::*;

#[tokio::test]
async fn recovery_rejects_unqualified_scores_and_preserves_governed_failures() {
    use jeryu_core::{
        CheckConclusion, CheckRunStatus, CreateCheckRunRequest, RecordJankuraiScoreRequest,
    };
    for case in [
        "admin",
        "version",
        "digest",
        "base",
        "inconsistent",
        "failure",
        "latest-failure",
        "new-pending",
    ] {
        let mut fx =
            Fixture::with_audit_store(&format!("recovery-{case}.txt"), &format!("{case}\n"));
        fx.advance_main(&format!("recovery-base-{case}.txt"), "base\n");
        isolate_base(&mut fx, &format!("recovery-{case}"));
        let (status, body) = fx.enqueue().await;
        assert_eq!(status, StatusCode::CREATED, "{case}: {body}");
        let head = body["queue_sha"].as_str().unwrap();
        let ticket = audit_queue::queue()
            .lock()
            .unwrap()
            .take("alice", "jeryu", head)
            .unwrap();
        let (version, digest) = crate::ci_bridge::jankurai::governed_auditor_identity().unwrap();
        let failing = matches!(case, "failure" | "latest-failure");
        let score = if failing { 40 } else { 98 };
        let mut report = serde_json::json!({"score": score, "caps_applied": [],
            "decision": {"hard_findings": 0, "minimum_score": 85},
            "jeryu_audit_provenance": {"runner_id": "independent-runner",
                "base_sha": ticket.base_sha, "audit_mode": ticket.audit_mode,
                "jankurai_version": version, "jankurai_sha256": digest}});
        match case {
            "admin" => {
                report
                    .as_object_mut()
                    .unwrap()
                    .remove("jeryu_audit_provenance");
            }
            "version" => {
                report["jeryu_audit_provenance"]["jankurai_version"] = "jankurai 0.0.0".into()
            }
            "digest" => report["jeryu_audit_provenance"]["jankurai_sha256"] = "0".repeat(64).into(),
            "base" => report["jeryu_audit_provenance"]["base_sha"] = "f".repeat(40).into(),
            _ => {}
        }
        let request = RecordJankuraiScoreRequest {
            branch: ticket.branch.clone(),
            commit_sha: head.to_string(),
            score: Some(if case == "inconsistent" { 100 } else { score }),
            hard_findings: Some(0),
            decision: "scored".into(),
            report: Some(report.clone()),
            ..Default::default()
        };
        if case == "latest-failure" {
            let mut earlier = request.clone();
            earlier.branch.push_str("-earlier");
            earlier.score = Some(98);
            earlier.report.as_mut().unwrap()["score"] = 98.into();
            fx.state
                .core
                .record_jankurai_score("alice", "jeryu", earlier)
                .unwrap();
        }
        if case == "new-pending" {
            for status in [CheckRunStatus::Completed, CheckRunStatus::InProgress] {
                fx.state
                    .core
                    .create_check_run(
                        "alice",
                        "jeryu",
                        CreateCheckRunRequest {
                            name: "jankurai/proof".into(),
                            head_sha: head.to_string(),
                            status: Some(status.clone()),
                            conclusion: (status == CheckRunStatus::Completed)
                                .then_some(CheckConclusion::Success),
                            ..Default::default()
                        },
                    )
                    .unwrap();
            }
        }
        fx.state
            .core
            .record_jankurai_score("alice", "jeryu", request)
            .unwrap();
        assert_eq!(merge_queue::tick(&fx.state), 0, "{case}");
        let should_recover = failing || case == "new-pending";
        let pending = audit_queue::queue()
            .lock()
            .unwrap()
            .take("alice", "jeryu", head);
        assert_eq!(pending.is_none(), should_recover, "{case}: ticket recovery");
        let runs = fx
            .state
            .core
            .list_check_runs("alice", "jeryu", Some(head))
            .unwrap();
        let newest = runs
            .check_runs
            .iter()
            .filter(|r| r.name == "jankurai/proof")
            .max_by_key(|r| r.completed_at.unwrap_or(r.started_at))
            .unwrap();
        if should_recover {
            assert_eq!(newest.status, CheckRunStatus::Completed, "{case}");
            assert_eq!(
                newest.conclusion,
                Some(if failing {
                    CheckConclusion::Failure
                } else {
                    CheckConclusion::Success
                }),
                "{case}"
            );
        } else {
            assert_ne!(
                newest.status,
                CheckRunStatus::Completed,
                "{case}: unqualified score published proof"
            );
        }
        assert_eq!(
            fx.state
                .core
                .list_jankurai_scores("alice", "jeryu", None, Some(head))
                .unwrap()
                .len(),
            if case == "latest-failure" { 2 } else { 1 },
            "{case}: no history overwritten or added"
        );
    }
}

#[tokio::test]
async fn corrupt_metadata_and_failed_pull_reads_refuse_without_consuming_tickets() {
    use super::super::merge_queue_tests::{account, git};
    let mut fx = Fixture::with_audit_store("corrupt-meta-recovery.txt", "corrupt metadata\n");
    fx.advance_main("corrupt-meta-base.txt", "base\n");
    isolate_base(&mut fx, "corrupt-meta-proof");
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let head = body["queue_sha"].as_str().unwrap();
    let path = fx.work.join("malformed-queue-metadata");
    std::fs::write(&path, "not JSON").unwrap();
    let blob = git(&fx.bare, &["hash-object", "-w", path.to_str().unwrap()]);
    git(&fx.bare, &["update-ref", "refs/queue-meta/main/99", &blob]);
    let error =
        merge_queue::authored_audit(&fx.state, "alice", "jeryu", head, &fx.main(), "gatebot")
            .unwrap_err();
    assert!(error.contains("invalid queue metadata"), "{error}");
    assert!(
        audit_queue::queue()
            .lock()
            .unwrap()
            .take("alice", "jeryu", head)
            .is_some()
    );
    let missing = "unreadable-pr-list";
    audit_queue::queue()
        .lock()
        .unwrap()
        .enqueue(audit_queue::AuditTicket::new(
            missing,
            "absent",
            "feature",
            &fx.head,
            &fx.main(),
        ));
    let denied = crate::web::jankurai::audits::author_produced_submission(
        &fx.state,
        &account("gatebot", UserRole::Admin).0,
        missing,
        "absent",
        &fx.head,
        &fx.main(),
    )
    .expect("a pull-list error must not authorize a report");
    assert_eq!(denied.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        denied
            .reason
            .starts_with("pull-request author independence could not be checked"),
        "{}",
        denied.reason
    );
    assert!(
        audit_queue::queue()
            .lock()
            .unwrap()
            .take(missing, "absent", &fx.head)
            .is_some()
    );
}
