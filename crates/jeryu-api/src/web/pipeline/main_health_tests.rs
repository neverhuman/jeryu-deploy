//! The main-health projection: red intervals, the change that turned main
//! red, and the flake rate. Invented repositories only.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::main_health::project;
use super::types::Event;

fn at(minute: i64) -> String {
    DateTime::<Utc>::from_timestamp(1_800_000_000 + minute * 60, 0)
        .unwrap()
        .to_rfc3339()
}

fn event(seq: i64, minute: i64, kind: &str, repo: &str, sha: &str, outcome: Option<&str>) -> Event {
    Event {
        seq,
        ts: at(minute),
        event_id: None,
        source: "forge".to_string(),
        kind: kind.to_string(),
        reporter: "forge".to_string(),
        actor: None,
        family: None,
        family_label: None,
        repo: Some(repo.to_string()),
        pr: None,
        sha: Some(sha.to_string()),
        todo_id: None,
        shift: None,
        outcome: outcome.map(str::to_string),
        needs_human: false,
        summary: "s".to_string(),
        reason: None,
        cost_usd: None,
        seconds: None,
        log_tail: None,
        log_url: None,
        detail: None,
    }
}

fn status(seq: i64, minute: i64, repo: &str, sha: &str, outcome: &str) -> Event {
    event(seq, minute, "main.status", repo, sha, Some(outcome))
}

fn with_detail(mut event: Event, detail: Value) -> Event {
    event.detail = Some(detail);
    event
}

#[test]
fn red_runs_from_the_first_red_head_to_the_next_green_head() {
    let mut merged = event(1, 0, "pr.merged", "acme/widget", "bbb2222", Some("success"));
    merged.pr = Some(7);
    let events = vec![
        status(2, 1, "acme/widget", "aaa1111", "green"),
        merged,
        status(3, 10, "acme/widget", "bbb2222", "red"),
        status(4, 20, "acme/widget", "ccc3333", "red"),
        status(5, 40, "acme/widget", "ddd4444", "green"),
    ];
    let now = DateTime::<Utc>::from_timestamp(1_800_000_000 + 60 * 60, 0).unwrap();
    let health = project(&events, now);
    assert_eq!(health.len(), 1);
    let repo = &health[0];
    assert!(!repo.red_now);
    assert_eq!(repo.intervals.len(), 1);
    let interval = &repo.intervals[0];
    assert_eq!(interval.sha, "bbb2222", "the first red head opens it");
    assert_eq!(interval.seconds, 30 * 60);
    assert_eq!(interval.fixed_by.as_deref(), Some("ddd4444"));
    assert_eq!(interval.culprit_pr, Some(7));
    assert!(!interval.flake);
    assert_eq!(repo.escaped_defects, 1);
    assert_eq!(repo.red_seconds, 30 * 60);
    assert_eq!(repo.observed_seconds, 59 * 60);
}

#[test]
fn an_unfixed_red_is_still_open_at_now() {
    let events = vec![
        status(1, 0, "globex/api", "aaa1111", "green"),
        status(2, 5, "globex/api", "bbb2222", "red"),
        status(3, 6, "globex/api", "bbb2222", "started"),
    ];
    let now = DateTime::<Utc>::from_timestamp(1_800_000_000 + 15 * 60, 0).unwrap();
    let repo = &project(&events, now)[0];
    assert!(repo.red_now);
    assert_eq!(repo.intervals[0].until, None);
    assert_eq!(repo.intervals[0].seconds, 10 * 60);
    assert_eq!(repo.gate_runs, 2, "a started event is not a gate run");
}

#[test]
fn a_rerun_that_passes_on_the_same_commit_is_a_flake() {
    let failing = with_detail(
        status(2, 5, "acme/widget", "bbb2222", "red"),
        json!({"tests": ["suite::slow_case"]}),
    );
    let events = vec![
        status(1, 0, "acme/widget", "aaa1111", "green"),
        failing,
        status(3, 8, "acme/widget", "bbb2222", "green"),
        status(4, 20, "acme/widget", "ccc3333", "green"),
    ];
    let now = DateTime::<Utc>::from_timestamp(1_800_000_000 + 30 * 60, 0).unwrap();
    let repo = &project(&events, now)[0];
    assert_eq!(repo.gate_runs, 4);
    assert_eq!(repo.flaky_runs, 1);
    assert!((repo.flake_rate - 0.25).abs() < 1e-9);
    assert_eq!(repo.flaky_tests, vec!["suite::slow_case".to_string()]);
    assert!(repo.intervals[0].flake);
    assert_eq!(repo.escaped_defects, 0, "a flake is no escaped defect");
}

#[test]
fn test_names_come_from_the_runners_gate_log_when_the_status_has_none() {
    let mut log = event(9, 6, "gate.log", "acme/widget", "bbb2222", Some("failure"));
    log.detail = Some(json!({"tests": ["suite::a", "suite::b"]}));
    let events = vec![
        status(1, 5, "acme/widget", "bbb2222", "red"),
        log,
        status(2, 9, "acme/widget", "bbb2222", "green"),
    ];
    let repo = &project(&events, Utc::now())[0];
    assert_eq!(repo.flaky_tests, vec!["suite::a", "suite::b"]);
}

#[test]
fn repositories_are_projected_separately() {
    let events = vec![
        status(1, 0, "acme/widget", "aaa1111", "red"),
        status(2, 1, "globex/api", "bbb2222", "green"),
    ];
    let health = project(&events, Utc::now());
    assert_eq!(health.len(), 2);
    assert!(health[0].red_now && !health[1].red_now);
}
