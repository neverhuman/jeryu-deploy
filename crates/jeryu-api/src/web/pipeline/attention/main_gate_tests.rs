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
