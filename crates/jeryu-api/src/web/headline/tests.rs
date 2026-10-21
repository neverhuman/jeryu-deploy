//! The three headline numbers on invented todos and events: no forge, no
//! store, so every rule is read off the arithmetic.

use std::collections::BTreeMap;

use chrono::{DateTime, TimeZone, Utc};

use super::{Headline, report};
use crate::web::pipeline::Event;
use crate::web::shift::todo_file::TodoFile;
use crate::web::shift::{Attempt, ShiftTodo, TodoPr, TodoStatus};

/// Day 10 of the window, at `hour`.
fn at(hour: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 10, 0, 0, 0).unwrap() + chrono::Duration::hours(hour)
}

fn stamp(hour: i64) -> String {
    at(hour).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn window() -> (DateTime<Utc>, DateTime<Utc>) {
    (at(-240), at(240))
}

fn attempt(outcome: &str, started: i64) -> Attempt {
    Attempt {
        by: "operator@node-a/w1".to_string(),
        host: "node-a".to_string(),
        slot: "w1".to_string(),
        model: "model-a".to_string(),
        session: String::new(),
        started: stamp(started),
        ended: String::new(),
        outcome: outcome.to_string(),
        cost_usd: Some(1.0),
        note: String::new(),
        shift: String::new(),
        agent: None,
        factory: None,
        run: None,
        experiment: None,
        arm_p: None,
        cost_estimated: None,
        usage: None,
    }
}

fn todo(id: &str, status: TodoStatus, filed: i64, attempts: Vec<Attempt>) -> ShiftTodo {
    let mut file = TodoFile::new(id.to_string(), "acme".to_string(), format!("Title {id}"));
    file.status = status;
    file.repos = vec!["widget".to_string()];
    file.filed_at = stamp(filed);
    let mut todo = file.to_api(at(0));
    todo.worked_by = attempts;
    todo
}

/// A done todo on one pull request, merged and (optionally) released.
fn shipped(id: &str, pr: u64, filed: i64, claimed: i64, released: Option<bool>) -> ShiftTodo {
    let mut todo = todo(id, TodoStatus::Done, filed, vec![attempt("done", claimed)]);
    todo.merged = true;
    todo.released = released;
    todo.prs = vec![TodoPr {
        repo: "acme/widget".to_string(),
        number: pr,
        state: "merged".to_string(),
        url: String::new(),
    }];
    todo
}

fn event(seq: i64, hour: i64, kind: &str, repo: &str, sha: &str) -> Event {
    Event {
        seq,
        ts: stamp(hour),
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
        outcome: None,
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

fn merged(seq: i64, hour: i64, pr: i64, sha: &str) -> Event {
    let mut event = event(seq, hour, "pr.merged", "acme/widget", sha);
    event.pr = Some(pr);
    event.outcome = Some("success".to_string());
    event
}

fn status(seq: i64, hour: i64, sha: &str, outcome: &str) -> Event {
    let mut event = event(seq, hour, "main.status", "acme/widget", sha);
    event.outcome = Some(outcome.to_string());
    event
}

fn run(todos: &[ShiftTodo], events: &[Event], deployed: &[(&str, u64, i64)]) -> Headline {
    let times: BTreeMap<(String, u64), String> = deployed
        .iter()
        .map(|(repo, pr, hour)| ((repo.to_string(), *pr), stamp(*hour)))
        .collect();
    report(todos, events, window(), at(240), &|repo: &str, pr: u64| {
        times.get(&(repo.to_string(), pr)).cloned()
    })
}

#[test]
fn lead_time_is_measured_from_the_deployment_of_the_todos_own_pull_request() {
    let todos: Vec<ShiftTodo> = (0..5)
        .map(|i| shipped(&format!("t{i}"), 70 + i, 0, 2, Some(true)))
        .collect();
    let events: Vec<Event> = (0..5)
        .map(|i| merged(i + 1, 6, 70 + i, &format!("sha{i}")))
        .collect();
    let deployed: Vec<(&str, u64, i64)> = (0..5).map(|i| ("acme/widget", 70 + i, 9)).collect();
    let headline = run(&todos, &events, &deployed);
    // filed at hour 0, production at hour 9; claimed at hour 2, main at hour 6.
    assert_eq!(headline.lead_time.filed_to_prod_s.p50, Some(9 * 3600));
    assert_eq!(headline.lead_time.claim_to_main_s.p50, Some(4 * 3600));
    assert_eq!(headline.lead_time.filed_to_prod_s.n, 5);
}

#[test]
fn a_todo_whose_pull_request_never_deployed_is_left_out_of_the_prod_lead_time() {
    let todos = vec![shipped("t0", 70, 0, 2, Some(true))];
    let events = vec![merged(1, 6, 70, "sha0")];
    let headline = run(&todos, &events, &[]);
    assert_eq!(headline.lead_time.filed_to_prod_s.n, 0);
    assert_eq!(headline.lead_time.claim_to_main_s.n, 1);
    // Below the sample floor both numbers stay null and say why.
    assert_eq!(headline.lead_time.claim_to_main_s.p50, None);
    assert_eq!(
        headline.lead_time.claim_to_main_s.unknown,
        Some(super::TOO_FEW)
    );
}

#[test]
fn a_deployment_before_the_todo_was_filed_is_not_a_negative_lead_time() {
    let mut todos: Vec<ShiftTodo> = (0..5)
        .map(|i| shipped(&format!("t{i}"), 70 + i, 0, 2, Some(true)))
        .collect();
    todos[0].filed_at = stamp(100); // filed after the deployment it is credited with
    let events: Vec<Event> = (0..5)
        .map(|i| merged(i + 1, 6, 70 + i, &format!("sha{i}")))
        .collect();
    let deployed: Vec<(&str, u64, i64)> = (0..5).map(|i| ("acme/widget", 70 + i, 9)).collect();
    let headline = run(&todos, &events, &deployed);
    assert_eq!(headline.lead_time.filed_to_prod_s.n, 4);
}

#[test]
fn a_multi_repo_todo_ships_when_its_last_repository_does() {
    let mut todo = shipped("t0", 70, 0, 2, Some(true));
    todo.prs.push(TodoPr {
        repo: "acme/gadget".to_string(),
        number: 9,
        state: "merged".to_string(),
        url: String::new(),
    });
    let events = vec![merged(1, 6, 70, "sha0")];
    let headline = run(
        &[todo.clone()],
        &events,
        &[("acme/widget", 70, 9), ("acme/gadget", 9, 30)],
    );
    assert_eq!(headline.lead_time.filed_to_prod_s.n, 1);
    // The other repository has no deployment: no date is invented from the one that has.
    let partial = run(&[todo], &events, &[("acme/widget", 70, 9)]);
    assert_eq!(partial.lead_time.filed_to_prod_s.n, 0);
}

#[test]
fn an_exception_is_a_todo_a_person_had_to_look_at_not_one_that_was_retried() {
    let todos = vec![
        shipped("clean", 70, 0, 2, Some(true)),
        todo(
            "retried",
            TodoStatus::Done,
            0,
            vec![attempt("retry", 1), attempt("done", 2)],
        ),
        todo("stuck", TodoStatus::Blocked, 0, vec![attempt("blocked", 1)]),
        todo(
            "handed_over",
            TodoStatus::Handoff,
            0,
            vec![attempt("done", 1)],
        ),
        todo(
            "unmet",
            TodoStatus::Done,
            0,
            vec![attempt("incomplete", 1), attempt("done", 2)],
        ),
    ];
    let headline = run(&todos, &[], &[]);
    assert_eq!(headline.exception_rate.todos, 5);
    assert_eq!(headline.exception_rate.needed_human, 3);
    assert_eq!(headline.exception_rate.rate, Some(0.6));
    assert_eq!(headline.exception_rate.reasons.get("blocked"), Some(&1));
    assert_eq!(headline.exception_rate.reasons.get("handoff"), Some(&1));
    assert_eq!(
        headline.exception_rate.reasons.get("attempt_left_it_unmet"),
        Some(&1)
    );
    assert_eq!(
        headline.exception_rate.reasons.get("attempt_gave_up"),
        Some(&1)
    );
}

#[test]
fn a_todo_a_person_took_on_is_an_exception_even_when_it_landed() {
    let mut todo = shipped("taken", 70, 0, 2, Some(true));
    todo.assignee = Some("ben".to_string());
    let headline = run(&[todo], &[], &[]);
    assert_eq!(headline.exception_rate.needed_human, 1);
    assert_eq!(
        headline.exception_rate.reasons.get("taken_on_by_a_person"),
        Some(&1)
    );
}

#[test]
fn a_change_that_turned_main_red_counts_once_and_a_flake_does_not_count() {
    let events = vec![
        merged(1, 1, 70, "bad111"),
        merged(2, 1, 71, "flaky1"),
        merged(3, 1, 72, "good11"),
        status(4, 0, "base000", "green"),
        status(5, 2, "bad111", "red"),
        status(6, 3, "bad111", "red"), // the same stretch, not a second bad change
        status(7, 4, "good11", "green"),
        status(8, 5, "flaky1", "red"),
        status(9, 6, "flaky1", "green"), // a rerun of the same commit went green
    ];
    let headline = run(&[], &events, &[]);
    assert_eq!(headline.bad_change_rate.changes, 3);
    assert_eq!(headline.bad_change_rate.bad, 1);
    assert_eq!(headline.bad_change_rate.rate, Some(0.3333));
    assert_eq!(headline.bad_change_rate.flaky_reds, 1);
    assert_eq!(
        headline.bad_change_rate.by_repo.get("acme/widget"),
        Some(&1)
    );
}

#[test]
fn a_production_deployment_that_failed_is_a_bad_change_too() {
    let mut failed = event(2, 3, "deploy.status", "acme/widget", "sha0");
    failed.pr = Some(70);
    failed.outcome = Some("failure".to_string());
    let headline = run(&[], &[merged(1, 1, 70, "sha0"), failed], &[]);
    assert_eq!(
        (
            headline.bad_change_rate.changes,
            headline.bad_change_rate.bad
        ),
        (1, 1)
    );
    assert_eq!(headline.bad_change_rate.rate, Some(1.0));
}

#[test]
fn a_failure_on_a_change_that_merged_outside_the_window_is_not_counted_against_it() {
    let mut failed = event(2, 3, "deploy.status", "acme/widget", "sha0");
    failed.pr = Some(70);
    failed.outcome = Some("failure".to_string());
    let headline = run(&[], &[merged(1, -1000, 70, "sha0"), failed], &[]);
    assert_eq!(
        (
            headline.bad_change_rate.changes,
            headline.bad_change_rate.bad
        ),
        (0, 0)
    );
    assert_eq!(headline.bad_change_rate.rate, None);
    assert_eq!(
        headline.bad_change_rate.unknown,
        Some(super::NO_MERGED_CHANGES)
    );
}

#[test]
fn an_empty_window_says_nothing_rather_than_zero_rates() {
    let headline = run(&[], &[], &[]);
    assert_eq!(headline.exception_rate.rate, None);
    assert_eq!(headline.bad_change_rate.rate, None);
    assert_eq!(headline.lead_time.filed_to_prod_s.p50, None);
}
