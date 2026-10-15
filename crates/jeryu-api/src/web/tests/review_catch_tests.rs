//! The defect links the forge draws itself: a rework, and an incident cause.
//! Both end up in the catch rate's denominator. The family, repository and
//! todo ids below are invented.

use super::*;
use crate::web::pipeline::{NewEvent, record};
use crate::web::review_catch::{self, DEFECT_KIND};

const REPO: &str = "acme/widgets";
const ORIGINAL: &str = "20261001-120000-aaaaaa";
const REDO: &str = "20261002-120000-bbbbbb";

fn fixture() -> Arc<WebState> {
    let core = ForgeCore::new();
    core.create_repository(
        "acme",
        CreateRepositoryRequest {
            name: "widgets".into(),
            ..Default::default()
        },
    )
    .unwrap();
    Arc::new(WebState::new(core))
}

/// The original todo's work landed as `acme/widgets#7`, as the flow ledger
/// recorded it when the pull request moved.
fn landed(state: &WebState, todo: &str, pr: i64) {
    state
        .events
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO flow_ledger (lot_id, todo_id, family, repo, pr, station,
                   entered_ms, left_ms, direction, source_seq_first, source_seq_last)
                 VALUES (?1, ?1, 'acme', ?2, ?3, 'review', 1, 2, 'forward', 1, 1)",
                rusqlite::params![todo, REPO, pr],
            )
            .map_err(|err| err.to_string())
        })
        .unwrap();
}

fn defects(state: &WebState) -> Vec<(String, String)> {
    let query = crate::web::pipeline::EventsQuery {
        kind: Some(DEFECT_KIND.to_string()),
        ..Default::default()
    };
    state
        .events
        .query(&query)
        .unwrap()
        .iter()
        .map(|event| {
            (
                format!("{}#{}", event.repo.clone().unwrap(), event.pr.unwrap()),
                event
                    .detail
                    .as_ref()
                    .and_then(|detail| detail.get("cause"))
                    .and_then(|cause| cause.as_str())
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

#[test]
fn a_rework_links_a_defect_to_the_changes_the_redone_todo_landed() {
    let state = fixture();
    landed(&state, ORIGINAL, 7);
    review_catch::link_defect(&state, "rework", REDO, &[ORIGINAL.to_string()]);
    assert_eq!(
        defects(&state),
        vec![(format!("{REPO}#7"), "rework".into())]
    );
    // Editing the same rework link again links the change once.
    review_catch::link_defect(&state, "rework", REDO, &[ORIGINAL.to_string()]);
    assert_eq!(defects(&state).len(), 1);
    // A todo the flow ledger knows nothing about names no change.
    review_catch::link_defect(&state, "rework", REDO, &["20261003-120000-cccccc".into()]);
    assert_eq!(defects(&state).len(), 1);
}

#[tokio::test]
async fn an_incident_cause_escapes_past_the_review_that_passed_the_change() {
    let state = fixture();
    // The change passed the agent reviewer, then merged.
    record(
        &state,
        "pr-redteam",
        NewEvent {
            repo: Some(REPO.into()),
            pr: Some(7),
            outcome: Some("approve".into()),
            ..NewEvent::forge("pr.reviewed", "pr-redteam reviewed acme/widgets#7: approve")
        },
    )
    .unwrap();
    // An alert blames it for an incident: one named by number, one by the
    // todo whose work it was.
    landed(&state, ORIGINAL, 8);
    record(
        &state,
        "alert-bot",
        NewEvent {
            source: "alert-webhook".into(),
            family: Some("acme".into()),
            detail: Some(json!({
                "incident_id": "inc-1",
                "changes": [format!("{REPO}#7"), ORIGINAL],
            })),
            ..NewEvent::forge("incident.caused_by", "checkout is down")
        },
    )
    .unwrap();
    assert_eq!(
        defects(&state),
        vec![
            (format!("{REPO}#8"), "incident".into()),
            (format!("{REPO}#7"), "incident".into()),
        ],
        "both the change named by number and the one the todo landed are linked"
    );

    // Re-posting the same cause (an alert that fires twice) links once more.
    record(
        &state,
        "alert-bot",
        NewEvent {
            source: "alert-webhook".into(),
            family: Some("acme".into()),
            detail: Some(json!({"incident_id": "inc-1", "change": format!("{REPO}#7")})),
            ..NewEvent::forge("incident.caused_by", "checkout is down")
        },
    )
    .unwrap();
    assert_eq!(defects(&state).len(), 2);

    // The reviewer that passed #7 carries the escape; #8 passed no review.
    let events = review_catch::read_events(&state, None).unwrap();
    let report = review_catch::report(&events, None);
    assert_eq!(report.agent.escaped, 1);
    assert_eq!(report.human.escaped, 0);
    assert_eq!(report.agent.catch_rate, Some(0.0));
}

#[test]
fn automation_review_routes_do_not_emit_human_findings() {
    let state = fixture();
    state
        .site_settings
        .set("account_kind:review-service", Some("service"), "admin", 1)
        .unwrap();
    let pr = state
        .core
        .create_pull_request(
            "acme",
            "widgets",
            "author",
            jeryu_core::CreatePullRequestRequest {
                title: "review fixture".into(),
                head_sha: Some("a".repeat(40)),
                base_sha: Some("b".repeat(40)),
                head: "feature".into(),
                base: "main".into(),
                ..Default::default()
            },
        )
        .unwrap();
    crate::web::pipeline::emit::pull_reviewed(&state, &pr, "review-service", "approve", None);
    review_catch::human_findings(&state, &pr, "review-service", "request_changes", vec![]);
    let events = review_catch::read_events(&state, None).unwrap();
    assert!(!events.iter().any(|e| e.kind == review_catch::FINDING_KIND));
    let route = events.iter().find(|e| e.kind == "pr.review").unwrap();
    assert_eq!(
        route.detail.as_ref().unwrap()["reviewer_kind"],
        review_catch::AGENT
    );
}

#[test]
fn review_catch_reverts_link_only_merged_declared_targets_once() {
    let state = fixture();
    let target = state
        .core
        .create_pull_request(
            "acme",
            "widgets",
            "author",
            CreatePullRequestRequest {
                title: "Original change".into(),
                head: "original".into(),
                base: "main".into(),
                head_sha: Some("a".repeat(40)),
                base_sha: Some("b".repeat(40)),
                ..Default::default()
            },
        )
        .unwrap();
    state
        .core
        .merge_pull_request(
            "acme",
            "widgets",
            target.number,
            jeryu_core::MergePullRequestRequest::default(),
        )
        .unwrap();
    let unmerged = state
        .core
        .create_pull_request(
            "acme",
            "widgets",
            "author",
            CreatePullRequestRequest {
                title: "Unrelated open change".into(),
                head: "open".into(),
                base: "main".into(),
                head_sha: Some("c".repeat(40)),
                base_sha: Some("b".repeat(40)),
                ..Default::default()
            },
        )
        .unwrap();
    let revert = state
        .core
        .create_pull_request(
            "acme",
            "widgets",
            "author",
            CreatePullRequestRequest {
                title: "Revert the original change".into(),
                head: "revert".into(),
                base: "main".into(),
                head_sha: Some("d".repeat(40)),
                base_sha: Some("b".repeat(40)),
                body: Some(format!(
                    "Reverts #{}\nReverts #{}\nReverts #99999\nSee also #77",
                    target.number, unmerged.number
                )),
                ..Default::default()
            },
        )
        .unwrap();
    review_catch::link_revert(&state, &revert);
    review_catch::link_revert(&state, &revert);
    assert_eq!(
        defects(&state),
        vec![(format!("{REPO}#{}", target.number), "revert".into())]
    );
}
