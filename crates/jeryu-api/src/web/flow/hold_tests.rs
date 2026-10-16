//! Holds: a lot that stopped stays at its station, held, with the reason and
//! who holds it; the arc back to Work is drawn only when a new head arrives.

use serde_json::Value;

use super::report::{Request, build, mark_superseded};
use super::{Station, Visit};
use crate::web::pipeline::attention::Superseded;
use crate::web::pipeline::{EventStore, NewEvent};

const MINUTE: i64 = 60 * 1000;
const START: i64 = 1_760_000_000_000;
const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn store() -> (tempfile::TempDir, EventStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = EventStore::open(&dir.path().join("shift.sqlite")).unwrap();
    (dir, store)
}

/// One event on `acme/widget#pr`, with the head it speaks about.
fn event(
    store: &EventStore,
    kind: &str,
    outcome: Option<&str>,
    pr: i64,
    sha: Option<&str>,
    detail: Option<Value>,
    minute: i64,
) {
    store
        .insert(
            "review-bot",
            &NewEvent {
                source: "gate".to_string(),
                kind: kind.to_string(),
                summary: format!("{kind} at {minute}"),
                outcome: outcome.map(str::to_string),
                family: Some("acme".to_string()),
                repo: Some("acme/widget".to_string()),
                pr: Some(pr),
                sha: sha.map(str::to_string),
                detail,
                ..NewEvent::default()
            },
            START + minute * MINUTE,
        )
        .unwrap();
}

fn todo_event(store: &EventStore, kind: &str, outcome: Option<&str>, todo: &str, minute: i64) {
    store
        .insert(
            "operator",
            &NewEvent {
                source: "todoq".to_string(),
                kind: kind.to_string(),
                summary: kind.to_string(),
                outcome: outcome.map(str::to_string),
                family: Some("acme".to_string()),
                repo: Some("acme/widget".to_string()),
                todo_id: Some(todo.to_string()),
                ..NewEvent::default()
            },
            START + minute * MINUTE,
        )
        .unwrap();
}

fn report(store: &EventStore, minute: i64) -> (Vec<Visit>, Value) {
    let visits = store.flow_visits_scoped(Some("acme"), 0, None).unwrap();
    let reached = store.flow_reached(Some("acme")).unwrap();
    let request = Request {
        family: Some("acme".to_string()),
        days: 28,
        now_ms: START + minute * MINUTE,
    };
    let body = serde_json::to_value(build(&request, &visits, &reached)).unwrap();
    (visits, body)
}

fn lot<'a>(body: &'a Value, station: &str, lot_id: &str) -> &'a Value {
    body["lots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|lot| lot["station"] == station && lot["lot_id"] == lot_id)
        .unwrap_or_else(|| panic!("no lot {lot_id} at {station}: {body}"))
}

fn station<'a>(body: &'a Value, id: &str) -> &'a Value {
    body["stations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .unwrap()
}

#[test]
fn a_review_hold_stays_at_review_with_who_holds_it_and_nobody_acting() {
    let (_dir, store) = store();
    event(&store, "pr.opened", None, 7, Some(HEAD_A), None, 0);
    event(&store, "review.started", None, 7, Some(HEAD_A), None, 1);
    let detail = serde_json::json!({"reviewer": "acme-reviewer"});
    event(
        &store,
        "review.finished",
        Some("request_changes"),
        7,
        Some(HEAD_A),
        Some(detail.clone()),
        5,
    );
    // The same verdict posted again (pr.review echo) keeps the first time.
    event(
        &store,
        "pr.review",
        Some("request_changes"),
        7,
        Some(HEAD_A),
        Some(detail),
        6,
    );

    let (visits, body) = report(&store, 120);
    let review: Vec<&Visit> = visits
        .iter()
        .filter(|v| v.station == Station::Review)
        .collect();
    assert_eq!(review.len(), 1, "one visit, no rework arc");
    assert_eq!(review[0].direction, "hold");
    assert!(review[0].left_ms.is_none());
    assert!(
        visits.iter().all(|v| v.station != Station::Work),
        "the lot was never sent back to Work"
    );

    let held = lot(&body, "review", "acme/widget#7");
    assert_eq!(held["state"], "held");
    assert_eq!(held["hold"]["reason"], "review_hold");
    assert_eq!(held["hold"]["by"], "acme-reviewer");
    assert_eq!(held["hold"]["nobody_acting"], true);
    assert_eq!(held["hold"]["age_s"], (120 - 5) * 60);
    assert_eq!(held["pr_href"], "/repos/jeryu/acme/widget/pulls/7");
    assert_eq!(
        held["hold"]["review_href"],
        "/repos/jeryu/acme/widget/pulls/7#changes-requested"
    );
    assert_eq!(held["href"], "/repos/jeryu/acme/widget/pulls/7");

    let at_review = station(&body, "review");
    assert_eq!(at_review["wip"]["held"], 1);
    assert_eq!(at_review["oldest_wait_s"], 120 * 60);
    assert_eq!(at_review["rework"], serde_json::json!([]));
}

#[test]
fn a_new_head_draws_the_rework_arc_and_the_lot_is_judged_afresh() {
    let (_dir, store) = store();
    event(&store, "pr.opened", None, 8, Some(HEAD_A), None, 0);
    event(&store, "gate.started", None, 8, Some(HEAD_A), None, 1);
    event(
        &store,
        "gate.finished",
        Some("failure"),
        8,
        Some(HEAD_A),
        None,
        4,
    );
    let (visits, body) = report(&store, 30);
    let gate: Vec<&Visit> = visits
        .iter()
        .filter(|v| v.station == Station::Gate)
        .collect();
    assert_eq!(gate.len(), 1);
    assert_eq!(gate[0].direction, "hold");
    let held = lot(&body, "gate", "acme/widget#8");
    assert_eq!(held["hold"]["reason"], "gate_red");
    assert_eq!(held["hold"]["by"], "pr-gate-runner");

    // The same head again changes nothing; a different head closes the hold.
    event(&store, "gate.started", None, 8, Some(HEAD_B), None, 20);
    let visits = store.flow_visits_scoped(Some("acme"), 0, None).unwrap();
    let gate: Vec<&Visit> = visits
        .iter()
        .filter(|v| v.station == Station::Gate)
        .collect();
    assert_eq!(gate.len(), 2);
    assert_eq!(gate[0].direction, "rework");
    assert_eq!(gate[0].rework_to, Some(Station::Work));
    assert_eq!(gate[0].left_ms, Some(START + 20 * MINUTE));
    assert!(gate[1].left_ms.is_none());
    assert_ne!(gate[1].direction, "hold");
}

#[test]
fn a_claimed_fix_means_somebody_is_acting() {
    let (_dir, store) = store();
    let todo = "20261012-000001-aaaaaa";
    todo_event(&store, "todo.filed", None, todo, 0);
    todo_event(&store, "todo.claimed", None, todo, 1);
    todo_event(&store, "todo.attempt_finished", Some("done"), todo, 9);
    event(&store, "pr.opened", None, 9, Some(HEAD_A), None, 10);
    store
        .insert(
            "operator",
            &NewEvent {
                source: "todoq".to_string(),
                kind: "pr.opened".to_string(),
                summary: "bind".to_string(),
                family: Some("acme".to_string()),
                repo: Some("acme/widget".to_string()),
                pr: Some(9),
                sha: Some(HEAD_A.to_string()),
                todo_id: Some(todo.to_string()),
                ..NewEvent::default()
            },
            START + 10 * MINUTE,
        )
        .unwrap();
    event(
        &store,
        "review.finished",
        Some("hold"),
        9,
        Some(HEAD_A),
        None,
        15,
    );
    let (_, body) = report(&store, 60);
    assert_eq!(lot(&body, "review", todo)["hold"]["nobody_acting"], true);

    // Somebody claims a fix todo for the lot after the hold.
    todo_event(&store, "todo.claimed", None, todo, 30);
    let (_, body) = report(&store, 60);
    assert_eq!(lot(&body, "review", todo)["hold"]["nobody_acting"], false);
    assert_eq!(lot(&body, "review", todo)["hold"]["reason"], "review_hold");
}

#[test]
fn queue_refusals_and_failures_hold_at_merge() {
    let (_dir, store) = store();
    event(&store, "pr.opened", None, 11, Some(HEAD_A), None, 0);
    event(
        &store,
        "review.finished",
        Some("approve"),
        11,
        Some(HEAD_A),
        None,
        2,
    );
    event(
        &store,
        "gate.finished",
        Some("success"),
        11,
        Some(HEAD_A),
        None,
        3,
    );
    event(&store, "queue.enqueued", None, 11, Some(HEAD_A), None, 4);
    event(&store, "queue.refused", None, 11, Some(HEAD_A), None, 6);
    event(&store, "pr.opened", None, 12, Some(HEAD_B), None, 0);
    event(
        &store,
        "review.finished",
        Some("approve"),
        12,
        Some(HEAD_B),
        None,
        2,
    );
    event(
        &store,
        "gate.finished",
        Some("success"),
        12,
        Some(HEAD_B),
        None,
        3,
    );
    event(&store, "queue.enqueued", None, 12, Some(HEAD_B), None, 4);
    event(&store, "queue.failed", None, 12, Some(HEAD_B), None, 8);
    let (_, body) = report(&store, 60);
    let refused = lot(&body, "merge", "acme/widget#11");
    assert_eq!(refused["hold"]["reason"], "queue_conflict");
    assert_eq!(refused["hold"]["by"], "merge-queue");
    let failed = lot(&body, "merge", "acme/widget#12");
    assert_eq!(failed["hold"]["reason"], "queue_failed");
    assert_eq!(station(&body, "merge")["wip"]["held"], 2);
    // Queued again: somebody acted, the hold lifts.
    event(&store, "queue.enqueued", None, 12, Some(HEAD_B), None, 20);
    let (_, body) = report(&store, 60);
    assert_eq!(lot(&body, "merge", "acme/widget#12")["hold"], Value::Null);
}

#[test]
fn a_superseded_pull_request_is_held_where_it_stopped() {
    let (_dir, store) = store();
    event(&store, "pr.opened", None, 3, Some(HEAD_A), None, 0);
    event(&store, "review.started", None, 3, Some(HEAD_A), None, 1);
    let mut visits = store.flow_visits_scoped(Some("acme"), 0, None).unwrap();
    let replaced_at = chrono::DateTime::from_timestamp_millis(START + 30 * MINUTE).unwrap();
    mark_superseded(
        &mut visits,
        &[
            Superseded {
                repo: "acme/widget".to_string(),
                number: 3,
                replacement_repo: "acme/widget".to_string(),
                replacement_number: 4,
                replacement_state: "open",
                replaced_at,
            },
            Superseded {
                repo: "globex/other".to_string(),
                number: 3,
                replacement_repo: "globex/other".to_string(),
                replacement_number: 5,
                replacement_state: "open",
                replaced_at,
            },
        ],
    );
    let request = Request {
        family: Some("acme".to_string()),
        days: 28,
        now_ms: START + 90 * MINUTE,
    };
    let reached = store.flow_reached(Some("acme")).unwrap();
    let body = serde_json::to_value(build(&request, &visits, &reached)).unwrap();
    let held = lot(&body, "review", "acme/widget#3");
    assert_eq!(held["state"], "held");
    assert_eq!(held["hold"]["reason"], "superseded");
    assert_eq!(held["hold"]["by"], "acme/widget#4");
    assert_eq!(held["hold"]["age_s"], 60 * 60);
    assert_eq!(station(&body, "review")["wip"]["held"], 1);
}

#[test]
fn production_actor_and_recipe_identify_the_holder_and_a_rerun_keeps_the_hold() {
    let (_dir, store) = store();
    event(&store, "pr.opened", None, 24, Some(HEAD_A), None, 0);
    store
        .insert(
            "forge",
            &NewEvent {
                source: "forge".into(),
                kind: "pr.review".into(),
                summary: "Review requires a fix".into(),
                actor: Some("acme-reviewer".into()),
                outcome: Some("request_changes".into()),
                family: Some("acme".into()),
                repo: Some("acme/widget".into()),
                pr: Some(24),
                sha: Some(HEAD_A.into()),
                ..NewEvent::default()
            },
            START + MINUTE,
        )
        .unwrap();
    event(&store, "review.started", None, 24, Some(HEAD_A), None, 2);
    event(
        &store,
        "gate.finished",
        Some("failure"),
        24,
        Some(HEAD_A),
        Some(serde_json::json!({"recipe": "acme-required"})),
        3,
    );
    event(&store, "gate.started", None, 24, Some(HEAD_A), None, 4);
    let (_, body) = report(&store, 60);
    assert_eq!(
        lot(&body, "review", "acme/widget#24")["hold"]["by"],
        "acme-reviewer"
    );
    assert_eq!(lot(&body, "review", "acme/widget#24")["state"], "held");
    assert_eq!(lot(&body, "gate", "acme/widget#24")["state"], "held");
    assert_eq!(
        lot(&body, "gate", "acme/widget#24")["hold"]["by"],
        "acme-required"
    );
    assert_eq!(station(&body, "gate")["rework"], serde_json::json!([]));
}
