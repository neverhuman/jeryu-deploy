//! Current-head verdicts must agree with red-main attention rows.
use chrono::Utc;
use jeryu_core::ForgeCore;

use super::main_gate_failure;
use crate::web::WebState;
use crate::web::pipeline::NewEvent;

#[test]
fn red_main_survives_a_second_red_head_but_not_an_unjudged_or_green_head() {
    let state = WebState::new(ForgeCore::new());
    let first = "a".repeat(40);
    let next = "b".repeat(40);
    let started = Utc::now().timestamp_millis() - 10_000;
    let status = |sha: &str, outcome: &str, offset: i64| {
        let event = NewEvent {
            repo: Some("acme/widget".to_string()),
            sha: Some(sha.to_string()),
            outcome: Some(outcome.to_string()),
            ..NewEvent::forge("main.status", "test main verdict")
        };
        state
            .events
            .insert("forge", &event, started + offset)
            .unwrap();
    };
    status(&first, "red", 0);
    assert!(main_gate_failure(&state, "acme", "widget", &first).is_some());
    assert!(main_gate_failure(&state, "acme", "widget", &next).is_none());
    status(&next, "started", 1);
    assert!(main_gate_failure(&state, "acme", "widget", &next).is_none());
    status(&next, "red", 2);
    assert!(main_gate_failure(&state, "acme", "widget", &next).is_some());
    status(&next, "green", 3);
    assert!(main_gate_failure(&state, "acme", "widget", &next).is_none());
}

#[test]
fn red_repos_reads_every_repository_newest_verdict_in_one_query() {
    use crate::web::pipeline::main_health::{red_now, red_repos};
    let state = WebState::new(ForgeCore::new());
    let started = Utc::now().timestamp_millis() - 100_000;
    let status = |repo: &str, outcome: &str, offset: i64| {
        let event = NewEvent {
            repo: Some(repo.to_string()),
            sha: Some("c".repeat(40)),
            outcome: Some(outcome.to_string()),
            ..NewEvent::forge("main.status", "test main verdict")
        };
        state
            .events
            .insert("forge", &event, started + offset)
            .unwrap();
    };
    status("acme/widget", "red", 0);
    status("acme/gadget", "red", 1);
    status("acme/gadget", "green", 2);
    // More `started` events than any fixed page cannot hide the verdict.
    for offset in 0..60 {
        status("acme/widget", "started", 10 + offset);
    }
    let red = red_repos(&state).unwrap();
    assert!(red.contains("acme/widget"), "{red:?}");
    assert!(!red.contains("acme/gadget"), "{red:?}");
    assert!(red_now(&state, "acme/widget").unwrap());
    assert!(!red_now(&state, "acme/gadget").unwrap());
    assert!(!red_now(&state, "acme/never-gated").unwrap());
}

#[test]
fn unreadable_verdicts_are_unknown_in_repository_summaries() {
    use crate::web::pipeline::main_health::{red_now, red_repos};
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "acme",
            jeryu_core::CreateRepositoryRequest {
                name: "widget".into(),
                private: false,
                description: None,
                default_branch: None,
            },
        )
        .unwrap();
    let state = WebState::new(core);
    state
        .events
        .with_conn(|conn| {
            conn.execute_batch("DROP TABLE pipeline_events")
                .map_err(|err| err.to_string())
        })
        .unwrap();
    assert!(red_now(&state, "acme/widget").is_err());
    assert!(red_repos(&state).is_err());
    assert_eq!(
        crate::web::repositories::repo_summary(&state, &repo).health,
        "unknown"
    );
}
