//! Required evidence has to be real and produced by someone other than the
//! author. These are the refusals: a status the author posted for their own
//! head, a status nobody is named for, a conclusion recorded while CI is
//! simulated, a required context with nothing recorded at all, an approval the
//! author gives themselves under another spelling of their login, and an audit
//! report handed in by the author of the change it would clear.

use super::*;
use crate::ci_bridge::audit_queue::{self, AuditTicket};
use serde_json::{Value, json};

const REQUIRED_CONTEXT: &str = "jeryu/required";

/// A repository whose `main` requires one status context, with an open pull
/// request authored by `author` at `head`.
fn repo_with_required_context(repo: &str, author: &str, head: &str) -> (WebState, u64) {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: repo.to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.set_branch_protection(
        "alice",
        repo,
        "main",
        SetBranchProtectionRequest {
            required_status_checks: vec![REQUIRED_CONTEXT.to_string()],
            ..SetBranchProtectionRequest::default()
        },
    )
    .unwrap();
    let pr = core
        .create_pull_request(
            "alice",
            repo,
            author,
            CreatePullRequestRequest {
                title: "a change that needs evidence".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some(head.to_string()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    (WebState::new(core), pr.number)
}

/// Record a green `jeryu/required` for `head` as `creator`.
fn green_status(state: &WebState, repo: &str, head: &str, creator: &str) {
    state
        .github
        .core()
        .create_commit_status(
            "alice",
            repo,
            head,
            creator,
            CreateCommitStatusRequest {
                state: CommitStatusState::Success,
                context: REQUIRED_CONTEXT.to_string(),
                description: Some("gate green".to_string()),
                target_url: None,
            },
        )
        .unwrap();
}

fn pull(state: &WebState, repo: &str, number: u64) -> jeryu_core::PullRequest {
    state
        .github
        .core()
        .get_pull_request("alice", repo, number)
        .unwrap()
}

fn blockers(detail: &Value) -> Vec<(String, String)> {
    detail["merge_passport"]["blockers"]
        .as_array()
        .expect("a passport lists its blockers")
        .iter()
        .map(|blocker| {
            (
                blocker["code"].as_str().unwrap_or_default().to_string(),
                blocker["details"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// The author posting their own green status is the author vouching for
/// themselves. Spelling it differently does not make them someone else.
#[test]
fn a_required_status_produced_by_the_author_is_not_evidence() {
    let head = "evidence-author-head";
    let (state, number) = repo_with_required_context("evidence-author", "mina", head);
    green_status(&state, "evidence-author", head, "  @Mina ");

    let detail = serde_json::to_value(crate::web::pulls::detail_for_pr_with_audit_enforcement(
        &state,
        &pull(&state, "evidence-author", number),
        false,
    ))
    .unwrap();

    assert_eq!(detail["merge_passport"]["status"], "blocked", "{detail}");
    let (code, details) = blockers(&detail)
        .into_iter()
        .find(|(code, _)| code == "passport_blocked_checks")
        .unwrap_or_else(|| panic!("the refusal is a check blocker: {detail}"));
    assert_eq!(code, "passport_blocked_checks");
    assert!(
        details.contains("produced by `mina`") && details.contains("author of the change"),
        "the blocker says whose evidence it refused: {details}"
    );
}

/// An independent producer's green status is the evidence the gate wanted, so
/// the refusal above is about who produced it and nothing else.
#[test]
fn a_required_status_from_an_independent_producer_is_evidence() {
    let head = "evidence-independent-head";
    let (state, number) = repo_with_required_context("evidence-independent", "mina", head);
    green_status(&state, "evidence-independent", head, "gatebot");

    let detail = serde_json::to_value(crate::web::pulls::detail_for_pr_with_audit_enforcement(
        &state,
        &pull(&state, "evidence-independent", number),
        false,
    ))
    .unwrap();

    assert!(
        !blockers(&detail)
            .iter()
            .any(|(code, _)| code.starts_with("passport_blocked_checks")),
        "nothing is refused about the evidence: {detail}"
    );
    assert_eq!(detail["merge_passport"]["status"], "pass", "{detail}");
}

/// A conclusion recorded while CI is simulated is not the outcome of a run, so
/// no required context counts as satisfied by it.
#[test]
fn a_simulated_conclusion_never_satisfies_a_required_context() {
    let head = "evidence-simulated-head";
    let (state, number) = repo_with_required_context("evidence-simulated", "mina", head);
    green_status(&state, "evidence-simulated", head, "gatebot");

    let detail = serde_json::to_value(crate::web::pulls::detail_for_pr_with_simulated_ci(
        &state,
        &pull(&state, "evidence-simulated", number),
    ))
    .unwrap();

    assert_eq!(detail["merge_passport"]["status"], "blocked", "{detail}");
    assert!(
        blockers(&detail)
            .iter()
            .any(|(_, details)| details.contains("CI simulated")),
        "the blocker says the evidence is simulated: {detail}"
    );
}

/// No evidence at all is not a pass: a required context with nothing recorded
/// for the head blocks the passport.
#[test]
fn a_required_context_with_no_evidence_at_all_blocks_the_passport() {
    let head = "evidence-absent-head";
    let (state, number) = repo_with_required_context("evidence-absent", "mina", head);

    let detail = serde_json::to_value(crate::web::pulls::detail_for_pr_with_audit_enforcement(
        &state,
        &pull(&state, "evidence-absent", number),
        false,
    ))
    .unwrap();

    assert_eq!(detail["merge_passport"]["status"], "blocked", "{detail}");
    assert!(
        blockers(&detail)
            .iter()
            .any(|(code, _)| code == "passport_blocked_checks_missing"),
        "{detail}"
    );
}

/// An approval is required evidence too: the author cannot give it to
/// themselves by signing in under another spelling of their own login.
#[tokio::test]
async fn an_author_cannot_approve_their_own_change_under_another_spelling() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "evidence-self-approval".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    let pr = core
        .create_pull_request(
            "alice",
            "evidence-self-approval",
            "mina",
            CreatePullRequestRequest {
                title: "self approval".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some("self-approval-head".to_string()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    let state = Arc::new(WebState::new(core));

    for reviewer in ["Mina", " mina ", "@mina"] {
        let refused = crate::web::pulls::approve(
            State(state.clone()),
            authenticated_account(reviewer),
            AxumPath((repo.id.to_string(), pr.number)),
            axum::body::Bytes::from(json!({"expected_head_sha": "self-approval-head"}).to_string()),
        )
        .await;
        assert_eq!(
            refused.status(),
            StatusCode::FORBIDDEN,
            "{reviewer} is the author"
        );
        assert_eq!(
            response_json(refused).await["code"],
            "pull_self_approval_forbidden"
        );
    }
    assert_eq!(
        state
            .github
            .core()
            .list_reviews("alice", "evidence-self-approval", pr.number)
            .unwrap()
            .len(),
        0,
        "a refused approval is never recorded"
    );
}

/// The one required check the forge completes from a submission is
/// `jankurai/proof`. A report for a head whose pull request the submitter
/// authored is refused, nothing is recorded, and the open ticket is left for a
/// runner that may actually produce it.
#[tokio::test]
async fn an_audit_report_from_the_change_author_is_refused() {
    let head = "9".repeat(40);
    let base = "a".repeat(40);
    let repo = "evidence-author-report";
    let core = ForgeCore::new();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: repo.to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    // The author holds the runner identity allowed to score: exactly the case
    // a producer check has to catch.
    core.create_pull_request(
        "jeryu",
        repo,
        "GateBot",
        CreatePullRequestRequest {
            title: "scored by its own author".to_string(),
            head: "codex/feature".to_string(),
            base: "main".to_string(),
            head_sha: Some(head.clone()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    let ticket = AuditTicket::new("jeryu", repo, "codex/feature", &head, &base);
    audit_queue::queue().lock().unwrap().enqueue(ticket.clone());
    let state = Arc::new(WebState::new(core));

    let body = json!({
        "branch": ticket.branch,
        "commit_sha": ticket.head_sha,
        "base_sha": ticket.base_sha,
        "runner_id": "runner/slot0",
        "jankurai_version": "jankurai 1.6.11",
        "jankurai_sha256": crate::ci_bridge::GOVERNED_JANKURAI_SHA256,
        "tool_exit": 0,
        "report": {
            "score": 100,
            "caps_applied": [],
            "decision": {"hard_findings": 0, "minimum_score": 85}
        }
    });
    let refused = crate::web::repositories::repo_jankurai_scores_ingest(
        State(state.clone()),
        authenticated_account("gatebot"),
        AxumPath(format!("jeryu/{repo}")),
        axum::body::Bytes::from(body.to_string()),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let refusal = response_json(refused).await;
    assert_eq!(refusal["code"], "permission_denied", "{refusal}");

    let core = state.github.core();
    assert!(
        core.list_jankurai_scores("jeryu", repo, None, Some(&head))
            .unwrap()
            .is_empty(),
        "a refused report is never recorded"
    );
    assert_eq!(
        core.list_check_runs("jeryu", repo, Some(&head))
            .unwrap()
            .total_count,
        0,
        "a refused report publishes no proof"
    );
    assert!(
        audit_queue::queue()
            .lock()
            .unwrap()
            .take("jeryu", repo, &head)
            .is_some(),
        "the refusal left the open ticket claimable"
    );
}
