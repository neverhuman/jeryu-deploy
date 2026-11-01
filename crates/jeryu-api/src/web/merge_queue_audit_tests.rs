//! Queue replays must reach the same governed audit queue as pushed heads.
use super::merge_queue_tests::Fixture;
use super::*;
use crate::ci_bridge::audit_queue;

#[tokio::test]
async fn queue_replay_schedules_exact_sha_and_tick_restores_lost_ticket() {
    let mut fx = Fixture::with_audit_store("audit-queue.txt", "queue audit recovery\n");
    let base = fx.advance_main("audit-base.txt", "base\n");
    isolate_base(&mut fx, "audit-recovery");
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let sha = body["queue_sha"].as_str().unwrap();
    assert_ne!(sha, fx.head);
    assert_eq!(
        merge_queue::authored_audit(&fx.state, "alice", "jeryu", sha, &base, "alice").unwrap(),
        Some(fx.number)
    );
    assert_eq!(
        merge_queue::authored_audit(
            &fx.state,
            "alice",
            "jeryu",
            sha,
            &base,
            "independent-runner"
        )
        .unwrap(),
        None
    );
    let ticket = audit_queue::queue()
        .lock()
        .unwrap()
        .take("alice", "jeryu", sha)
        .expect("replay scheduled an audit");
    assert_eq!(ticket.head_sha, sha);
    assert_eq!(ticket.branch, "refs/queue/audit-recovery/2");
    assert_eq!(ticket.base_sha, base);
    assert_eq!(ticket.audit_mode, "diff");
    let runs = fx
        .state
        .core
        .list_check_runs("alice", "jeryu", Some(sha))
        .unwrap();
    assert!(
        runs.check_runs
            .iter()
            .any(|run| run.name == "jankurai/proof"
                && run.status == jeryu_core::CheckRunStatus::InProgress
                && run.conclusion.is_none())
    );
    assert!(
        fx.state
            .core
            .list_jankurai_scores("alice", "jeryu", None, Some(sha))
            .unwrap()
            .is_empty()
    );
    // Model a score write followed by a failed check publication. A record the
    // auditor put no verdict in (`decision` is not `scored`) must not suppress
    // recovery of the missing proof: there is no verdict to republish.
    fx.state
        .core
        .record_jankurai_score(
            "alice",
            "jeryu",
            jeryu_core::RecordJankuraiScoreRequest {
                branch: ticket.branch.clone(),
                commit_sha: sha.to_string(),
                score: Some(98),
                decision: "tool-failed".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
    // Losing the in-memory ticket models a restart; the persisted entry is
    // sufficient to schedule it again without rebuilding or passing a proof.
    assert_eq!(merge_queue::tick(&fx.state), 0);
    let restored = audit_queue::queue()
        .lock()
        .unwrap()
        .take("alice", "jeryu", sha)
        .expect("tick restored audit");
    assert_eq!(restored.base_sha, ticket.base_sha);
    assert_eq!(restored.head_sha, ticket.head_sha);
    assert_eq!(fx.main(), base);
}

#[tokio::test]
async fn protected_queue_waits_for_provenance_checked_exact_sha_report() {
    use super::merge_queue_tests::account;
    use crate::ci_bridge::{self, AuditedHead};
    let mut fx = Fixture::with_audit_store("protected-audit.txt", "protected queue audit\n");
    let report = serde_json::json!({"score": 98, "caps_applied": [],
        "decision": {"hard_findings": 0, "minimum_score": 85}});
    ci_bridge::record_audited_head(
        &fx.state.core,
        &AuditedHead {
            owner: "alice",
            repo: "jeryu",
            branch: "feature",
            head_sha: &fx.head,
            origin_base_url: "http://forge.test",
        },
        Some(report.clone()),
        0,
    )
    .unwrap();
    fx.state
        .core
        .set_branch_protection(
            "alice",
            "jeryu",
            "main",
            jeryu_core::SetBranchProtectionRequest {
                required_status_checks: vec!["ci/fast".into(), "jankurai/proof".into()],
                require_jankurai_proof: true,
                ..Default::default()
            },
        )
        .unwrap();
    let base = fx.advance_main("protected-base.txt", "base\n");
    isolate_base(&mut fx, "audit-protected");
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let sha = body["queue_sha"].as_str().unwrap();
    fx.status(sha, jeryu_core::CommitStatusState::Success);
    assert_eq!(merge_queue::tick(&fx.state), 0);
    assert_eq!(fx.main(), base);
    let ticket = audit_queue::queue()
        .lock()
        .unwrap()
        .tickets()
        .iter()
        .find(|t| t.owner == "alice" && t.repo == "jeryu" && t.head_sha == sha)
        .unwrap()
        .clone();
    let (version, _) = ci_bridge::jankurai::governed_auditor_identity().unwrap();
    let mut submission = serde_json::json!({
        "branch": ticket.branch, "commit_sha": sha, "base_sha": ticket.base_sha,
        "runner_id": "independent-test-runner", "jankurai_version": version,
        "jankurai_sha256": "0".repeat(64), "tool_exit": 0, "report": report
    });
    async fn ingest(fx: &Fixture, login: &str, body: &Value) -> StatusCode {
        let response = repo_jankurai_scores_ingest(
            State(fx.state.clone()),
            account(login, UserRole::Admin),
            AxumPath("alice/jeryu".to_string()),
            axum::body::Bytes::from(body.to_string()),
        )
        .await;
        let status = response.status();
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            panic!(
                "{}",
                String::from_utf8_lossy(
                    &axum::body::to_bytes(response.into_body(), usize::MAX)
                        .await
                        .unwrap()
                )
            );
        }
        status
    }
    assert_eq!(
        ingest(&fx, "independent-runner", &submission).await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(merge_queue::tick(&fx.state), 0);
    submission["jankurai_sha256"] = ci_bridge::GOVERNED_JANKURAI_SHA256.into();
    assert_eq!(
        ingest(&fx, "alice", &submission).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        ingest(&fx, "independent-runner", &submission).await,
        StatusCode::CREATED
    );
    // The receipt is persisted independently of both the report and the index.
    Arc::get_mut(&mut fx.state).unwrap().merge_queue = Arc::new(merge_queue::MergeQueue::default());
    assert_eq!(merge_queue::tick(&fx.state), 1);
    assert_eq!(
        super::merge_queue_tests::git(&fx.bare, &["rev-parse", "refs/heads/audit-protected"]),
        sha
    );
}

// Other queue fixtures intentionally share alice/jeryu. Give these audits
// separate base refs so concurrent tickets cannot supersede each other.
fn isolate_base(fx: &mut Fixture, base: &str) {
    use super::merge_queue_tests::git;
    git(
        &fx.bare,
        &["update-ref", &format!("refs/heads/{base}"), &fx.main()],
    );
    let pr = fx
        .state
        .core
        .create_pull_request(
            "alice",
            "jeryu",
            "alice",
            jeryu_core::CreatePullRequestRequest {
                title: "audited queue".into(),
                head: "feature".into(),
                base: base.into(),
                head_sha: Some(fx.head.clone()),
                base_sha: Some(fx.main()),
                changed_files: vec!["feature.txt".into()],
                ..Default::default()
            },
        )
        .unwrap();
    fx.number = pr.number;
    fx.state
        .core
        .create_review(
            "alice",
            "jeryu",
            fx.number,
            "review-bot",
            jeryu_core::CreateReviewRequest {
                body: None,
                event: jeryu_core::ReviewState::Approved,
                comments: Vec::new(),
                expected_head_sha: Some(fx.head.clone()),
            },
        )
        .unwrap();
    {
        crate::ci_bridge::record_audited_head(
            &fx.state.core,
            &crate::ci_bridge::AuditedHead {
                owner: "alice",
                repo: "jeryu",
                branch: "feature",
                head_sha: &fx.head,
                origin_base_url: "http://forge.test",
            },
            Some(serde_json::json!({"score": 98, "caps_applied": [],
            "decision": {"hard_findings": 0, "minimum_score": 85}})),
            0,
        )
        .unwrap();
        fx.state
            .core
            .set_branch_protection(
                "alice",
                "jeryu",
                base,
                jeryu_core::SetBranchProtectionRequest {
                    required_status_checks: vec!["ci/fast".into(), "jankurai/proof".into()],
                    require_jankurai_proof: true,
                    ..Default::default()
                },
            )
            .unwrap();
    }
}

#[tokio::test]
async fn a_stack_tickets_every_queue_sha_and_holds_each_author_off_the_top() {
    let mut fx = Fixture::with_audit_store("stack-audit.txt", "stack audit\n");
    let base = fx.advance_main("stack-base.txt", "base\n");
    isolate_base(&mut fx, "audit-stack");
    let second = add_audited_pr(&fx, "audit-stack", "feature-two", "stack-two.txt", "bob");
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let bottom = body["queue_sha"].as_str().unwrap().to_string();
    let (status, body) = enqueue_number(&fx, second).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let top = body["queue_sha"].as_str().unwrap().to_string();
    assert_ne!(bottom, top, "the stacked entry is its own commit");

    // One ticket per entry, each naming its own queue ref: a stack must not
    // supersede the tickets of the entries it sits on.
    let mut queue = audit_queue::queue().lock().unwrap();
    for (number, sha) in [(fx.number, &bottom), (second, &top)] {
        let ticket = queue
            .take("alice", "jeryu", sha)
            .unwrap_or_else(|| panic!("#{number} was ticketed at {sha}"));
        assert_eq!(ticket.branch, format!("refs/queue/audit-stack/{number}"));
        assert_eq!(ticket.base_sha, base, "diffed against the protected base");
        assert_eq!(ticket.audit_mode, "diff");
    }
    drop(queue);

    // The top commit carries both changes, so neither author may score it,
    // while an independent runner still may.
    assert_eq!(
        merge_queue::authored_audit(&fx.state, "alice", "jeryu", &top, &base, "alice").unwrap(),
        Some(fx.number)
    );
    assert_eq!(
        merge_queue::authored_audit(&fx.state, "alice", "jeryu", &top, &base, "bob").unwrap(),
        Some(second)
    );
    assert_eq!(
        merge_queue::authored_audit(&fx.state, "alice", "jeryu", &top, &base, "gate-runner")
            .unwrap(),
        None
    );
    // The bottom commit does not carry the entry above it.
    assert_eq!(
        merge_queue::authored_audit(&fx.state, "alice", "jeryu", &bottom, &base, "bob").unwrap(),
        None
    );
}

/// Independence is about the change being audited, not the queue's history:
/// every queue commit descends from the ones that landed before it.
#[tokio::test]
async fn a_landed_entrys_author_may_audit_a_later_queue_sha() {
    let fx = Fixture::with_audit_store("landed-audit.txt", "landed audit\n");
    let bob = add_audited_pr(&fx, "main", "feature-bob", "bob.txt", "bob");
    fx.advance_main("landed-base.txt", "base\n");
    let (status, body) = enqueue_number(&fx, bob).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let landed = body["queue_sha"].as_str().unwrap().to_string();
    fx.status(&landed, jeryu_core::CommitStatusState::Success);
    assert_eq!(merge_queue::tick(&fx.state), 1);
    assert_eq!(fx.main(), landed, "bob's entry landed");

    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let sha = body["queue_sha"].as_str().unwrap().to_string();
    assert_eq!(
        merge_queue::authored_audit(&fx.state, "alice", "jeryu", &sha, &landed, "bob").unwrap(),
        None,
        "bob's landed change is not the change this sha is being audited for"
    );
    assert_eq!(
        merge_queue::authored_audit(&fx.state, "alice", "jeryu", &sha, &landed, "alice").unwrap(),
        Some(fx.number)
    );
    // Whole-tree tickets use the empty tree, which cannot be a rev-list
    // commit boundary. Historical authors remain eligible, while the live
    // queue author is still excluded.
    for no_range in [
        crate::ci_bridge::audit_queue::NO_COMMIT_BASE_OID,
        &"f".repeat(40),
    ] {
        assert_eq!(
            merge_queue::authored_audit(&fx.state, "alice", "jeryu", &sha, no_range, "bob")
                .unwrap(),
            None,
        );
        assert_eq!(
            merge_queue::authored_audit(&fx.state, "alice", "jeryu", &sha, no_range, "alice")
                .unwrap(),
            Some(fx.number),
        );
    }
}

/// A Git failure while establishing independence is an error, not "no author":
/// a Building entry whose commit cannot be tested for ancestry must not let the
/// submission through.
#[tokio::test]
async fn an_unreadable_queue_stack_fails_closed() {
    let fx = Fixture::with_audit_store("fail-closed.txt", "fail closed\n");
    fx.advance_main("fail-closed-base.txt", "base\n");
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let sha = body["queue_sha"].as_str().unwrap().to_string();
    let stored = super::merge_queue_tests::git(
        &fx.bare,
        &["show", &format!("refs/queue-meta/main/{}", fx.number)],
    );
    let mut ghost: merge_queue::QueueEntry = serde_json::from_str(&stored).unwrap();
    ghost.number = 99;
    ghost.queue_sha = "1".repeat(40);
    let blob = {
        use std::io::Write;
        let mut child = std::process::Command::new("git")
            .arg("-C")
            .arg(&fx.bare)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(serde_json::to_string(&ghost).unwrap().as_bytes())
            .unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string()
    };
    super::merge_queue_tests::git(&fx.bare, &["update-ref", "refs/queue-meta/main/99", &blob]);
    assert!(
        merge_queue::authored_audit(
            &fx.state,
            "alice",
            "jeryu",
            &sha,
            &fx.main(),
            "independent-runner"
        )
        .is_err()
    );
}

/// Exactly one audit per head means exactly one recorded verdict. When the
/// score was written but publishing the proof failed, the recovery republishes
/// the recorded verdict and tickets nothing, so no second report can be
/// authorized to replace it.
#[tokio::test]
async fn a_recorded_verdict_is_republished_rather_than_audited_again() {
    let mut fx = Fixture::with_audit_store("republish.txt", "republish verdict\n");
    fx.advance_main("republish-base.txt", "base\n");
    isolate_base(&mut fx, "audit-republish");
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let sha = body["queue_sha"].as_str().unwrap().to_string();
    let ticket = audit_queue::queue()
        .lock()
        .unwrap()
        .take("alice", "jeryu", &sha)
        .expect("replay scheduled an audit");
    // The ingest's own verdict for this head, stored before its check-run
    // publication failed.
    let recorded = fx.state
        .core
        .record_jankurai_score(
            "alice",
            "jeryu",
            jeryu_core::RecordJankuraiScoreRequest {
                branch: ticket.branch.clone(),
                commit_sha: sha.clone(),
                score: Some(98),
                hard_findings: Some(0),
                decision: "scored".to_string(),
                report: Some(serde_json::json!({"score": 98, "caps_applied": [],
                "decision": {"hard_findings": 0, "minimum_score": 85},
                "jeryu_audit_provenance": {
                    "runner_id": "independent-runner", "base_sha": ticket.base_sha,
                    "audit_mode": ticket.audit_mode,
                    "jankurai_version": crate::ci_bridge::jankurai::governed_auditor_identity().unwrap().0,
                    "jankurai_sha256": crate::ci_bridge::jankurai::governed_auditor_identity().unwrap().1
                }})),
                ..Default::default()
            },
        )
        .unwrap();
    merge_queue::record_queue_score(&fx.state, &recorded).unwrap();
    assert_eq!(merge_queue::tick(&fx.state), 0);
    assert!(
        audit_queue::queue()
            .lock()
            .unwrap()
            .take("alice", "jeryu", &sha)
            .is_none(),
        "a head with a recorded verdict must not be ticketed for a second report"
    );
    let proofs: Vec<_> = fx
        .state
        .core
        .list_check_runs("alice", "jeryu", Some(&sha))
        .unwrap()
        .check_runs
        .into_iter()
        .filter(|run| {
            run.name == "jankurai/proof" && run.status == jeryu_core::CheckRunStatus::Completed
        })
        .collect();
    assert_eq!(proofs.len(), 1, "the recorded verdict published once");
    assert_eq!(
        proofs[0].conclusion,
        Some(jeryu_core::CheckConclusion::Success)
    );
    // The recorded score is the one the republished proof reports: recovery
    // writes no second score row.
    let scores = fx
        .state
        .core
        .list_jankurai_scores("alice", "jeryu", None, Some(&sha))
        .unwrap();
    assert_eq!(scores.len(), 1);
    assert_eq!(scores[0].score, Some(98));
}

async fn enqueue_number(fx: &Fixture, number: u64) -> (StatusCode, Value) {
    use super::merge_queue_tests::account;
    let response = merge_queue::enqueue(
        State(fx.state.clone()),
        account("rel-bot", UserRole::Admin),
        None,
        AxumPath((fx.repo_id.clone(), number)),
    )
    .await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Another author's approved, proof-carrying pull request on the same base,
/// so a stack holds changes from more than one person.
fn add_audited_pr(fx: &Fixture, base: &str, branch: &str, file: &str, author: &str) -> u64 {
    use super::merge_queue_tests::git;
    let base_sha = fx.main();
    git(&fx.work, &["checkout", "--quiet", "-b", branch, &base_sha]);
    std::fs::write(fx.work.join(file), "stacked\n").unwrap();
    git(&fx.work, &["add", "."]);
    git(&fx.work, &["commit", "--quiet", "-m", branch]);
    let head = git(&fx.work, &["rev-parse", "HEAD"]);
    git(
        &fx.work,
        &["push", "--quiet", fx.bare.to_str().unwrap(), branch],
    );
    git(&fx.work, &["checkout", "--quiet", "main"]);
    let core = &fx.state.core;
    let pr = core
        .create_pull_request(
            "alice",
            "jeryu",
            author,
            jeryu_core::CreatePullRequestRequest {
                title: branch.into(),
                head: branch.into(),
                base: base.into(),
                head_sha: Some(head.clone()),
                base_sha: Some(base_sha),
                changed_files: vec![file.into()],
                ..Default::default()
            },
        )
        .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        jeryu_core::CreateCheckRunRequest {
            name: "ci/fast".into(),
            head_sha: head.clone(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(jeryu_core::CheckConclusion::Success),
            ..Default::default()
        },
    )
    .unwrap();
    core.create_review(
        "alice",
        "jeryu",
        pr.number,
        "review-bot",
        jeryu_core::CreateReviewRequest {
            body: None,
            event: jeryu_core::ReviewState::Approved,
            comments: Vec::new(),
            expected_head_sha: Some(head.clone()),
        },
    )
    .unwrap();
    crate::ci_bridge::record_audited_head(
        core,
        &crate::ci_bridge::AuditedHead {
            owner: "alice",
            repo: "jeryu",
            branch,
            head_sha: &head,
            origin_base_url: "http://forge.test",
        },
        Some(serde_json::json!({"score": 98, "caps_applied": [],
        "decision": {"hard_findings": 0, "minimum_score": 85}})),
        0,
    )
    .unwrap();
    pr.number
}

#[path = "merge_queue_audit_refresh_tests.rs"]
mod refresh_tests;

#[path = "merge_queue_audit_recovery_tests.rs"]
mod recovery_tests;

#[path = "merge_queue_audit_admission_tests.rs"]
mod admission_tests;
