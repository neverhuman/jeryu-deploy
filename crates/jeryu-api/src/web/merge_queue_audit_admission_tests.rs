//! Admission must reject untrusted submissions before Git independence reads.
use super::*;

#[tokio::test]
async fn invalid_submitters_and_tickets_skip_expensive_independence_reads() {
    use super::super::merge_queue_tests::account;
    use super::refresh_tests::{submission, ticket};
    use crate::web::jankurai::audits::{authorize_runner_submission, consume_runner_submission};
    let mut fx = Fixture::with_audit_store("early-validation.txt", "audit\n");
    fx.advance_main("early-validation-base.txt", "base\n");
    isolate_base(&mut fx, "early-validation");
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let head = body["queue_sha"].as_str().unwrap();
    let original = ticket(head);
    for case in ["identity", "base", "branch", "head", "digest", "mode"] {
        let mut report = submission(&original);
        match case {
            "base" => report["base_sha"] = fx.head.clone().into(),
            "branch" => report["branch"] = "wrong".into(),
            "head" => report["commit_sha"] = "f".repeat(40).into(),
            "digest" => report["jankurai_sha256"] = "0".repeat(64).into(),
            "mode" => report["audit_mode"] = "invalid".into(),
            _ => {}
        }
        let role = if case == "identity" {
            UserRole::User
        } else {
            UserRole::Admin
        };
        merge_queue::AUDIT_GIT_READS.with(|count| count.set(0));
        let response = repo_jankurai_scores_ingest(
            State(fx.state.clone()),
            account("untrusted-submitter", role),
            AxumPath("alice/jeryu".into()),
            axum::body::Bytes::from(report.to_string()),
        )
        .await;
        assert!(response.status().is_client_error(), "{case}");
        merge_queue::AUDIT_GIT_READS.with(|count| assert_eq!(count.get(), 0, "{case}"));
        assert_eq!(ticket(head), original, "{case}: ticket untouched");
    }
    merge_queue::AUDIT_GIT_READS.with(|count| count.set(0));
    assert_eq!(
        super::refresh_tests::ingest(&fx, "alice", &submission(&original)).await,
        StatusCode::FORBIDDEN
    );
    merge_queue::AUDIT_GIT_READS.with(|count| {
        assert!(
            count.get() >= 2,
            "valid ticket reaches metadata and range reads"
        )
    });
    assert_eq!(ticket(head), original, "author refusal retains ticket");
    let input = fx.work.join("invalid-admission-metadata.json");
    std::fs::write(&input, "invalid JSON").unwrap();
    let blob = super::super::merge_queue_tests::git(
        &fx.bare,
        &["hash-object", "-w", input.to_str().unwrap()],
    );
    super::super::merge_queue_tests::git(
        &fx.bare,
        &["update-ref", "refs/queue-meta/invalid-admission/999", &blob],
    );
    assert_eq!(
        super::refresh_tests::ingest(&fx, "independent-runner", &submission(&original)).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(ticket(head), original, "failed independence retains ticket");
    let runner = account("independent-runner", UserRole::Admin).0;
    let report = serde_json::from_value(submission(&original)).unwrap();
    let validated = authorize_runner_submission(&runner, "alice", "jeryu", &report)
        .unwrap_or_else(|e| panic!("{}", e.reason));
    assert_eq!(ticket(head), original, "validation does not consume");
    let mut replacement = original.clone();
    replacement.base_sha = fx.head.clone();
    audit_queue::queue()
        .lock()
        .unwrap()
        .enqueue(replacement.clone());
    assert!(consume_runner_submission(&validated).is_err());
    assert_eq!(
        ticket(head),
        replacement,
        "freshness refusal preserves replacement"
    );
    let report = serde_json::from_value(submission(&replacement)).unwrap();
    let validated = authorize_runner_submission(&runner, "alice", "jeryu", &report)
        .unwrap_or_else(|e| panic!("{}", e.reason));
    assert!(consume_runner_submission(&validated).is_ok());
    assert!(
        consume_runner_submission(&validated).is_err(),
        "only consumed once"
    );
}
