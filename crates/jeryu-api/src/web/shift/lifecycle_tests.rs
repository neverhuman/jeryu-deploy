//! The todo lifecycle: which status moves an admin action may make, and that
//! the server never writes `merged` (todoq sets it only on blocker todos).

use chrono::Utc;
use serde_json::json;

use super::apply_action;
use super::todo_file::TodoFile;
use super::types::{TodoActionRequest, TodoStatus, WorkerState};

fn todo_in(status: TodoStatus) -> TodoFile {
    let mut todo = TodoFile::new(
        "20260921-000000-aaaaaa".to_string(),
        "jeryu".to_string(),
        "Lifecycle".to_string(),
    );
    todo.status = status;
    todo.attempts = 2;
    todo.lease_until = "2026-09-21T01:00:00Z".to_string();
    todo.note = "before".to_string();
    todo
}

fn action(name: &str) -> TodoActionRequest {
    serde_json::from_value(json!({"action": name})).expect("action request")
}

#[test]
fn statuses_round_trip_through_file_and_wire_spellings() {
    for status in TodoStatus::ALL {
        assert_eq!(TodoStatus::parse(status.as_str()), Some(status));
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            json!(status.as_str())
        );
        let todo = todo_in(status);
        let parsed = TodoFile::parse(&todo.dump()).expect("parse dumped todo");
        assert_eq!(parsed.status, status);
        assert_eq!(todo.to_api(Utc::now()).status, status);
    }
    assert_eq!(TodoStatus::parse("Done"), None);
    assert_eq!(TodoStatus::parse("merged"), None);
    assert_eq!(TodoStatus::default(), TodoStatus::Open);
    for state in WorkerState::ALL {
        assert_eq!(WorkerState::parse(state.as_str()), Some(state));
    }
    assert_eq!(WorkerState::parse("sleeping"), None);
}

#[test]
fn done_is_final_and_every_other_status_may_move() {
    for from in TodoStatus::ALL {
        for to in TodoStatus::ALL {
            let expected = from == to || from != TodoStatus::Done;
            assert_eq!(from.allows(to), expected, "{from:?} -> {to:?}");
        }
    }
}

#[test]
fn release_returns_unfinished_work_to_open() {
    for from in [
        TodoStatus::Open,
        TodoStatus::Claimed,
        TodoStatus::Blocked,
        TodoStatus::Handoff,
    ] {
        let mut todo = todo_in(from);
        apply_action(&mut todo, &action("release")).expect("release");
        assert_eq!(todo.status, TodoStatus::Open, "from {from:?}");
        assert!(todo.lease_until.is_empty());
        assert_eq!(todo.attempts, 0);
        assert_eq!(todo.note, "before", "no note leaves the note alone");
    }
}

#[test]
fn block_parks_unfinished_work_and_keeps_attempts() {
    for from in [
        TodoStatus::Open,
        TodoStatus::Claimed,
        TodoStatus::Blocked,
        TodoStatus::Handoff,
    ] {
        let mut todo = todo_in(from);
        let request: TodoActionRequest =
            serde_json::from_value(json!({"action": "block", "note": "needs design"})).unwrap();
        apply_action(&mut todo, &request).expect("block");
        assert_eq!(todo.status, TodoStatus::Blocked, "from {from:?}");
        assert!(todo.lease_until.is_empty());
        assert_eq!(todo.attempts, 2);
        assert_eq!(todo.note, "needs design");
    }
}

#[test]
fn a_done_todo_refuses_release_and_block_and_is_left_untouched() {
    for name in ["release", "block"] {
        let mut todo = todo_in(TodoStatus::Done);
        todo.commits = vec![("jeryu-deploy".to_string(), "abc123".to_string())];
        let before = todo.dump();
        let refused = apply_action(&mut todo, &action(name)).expect_err(name);
        assert_eq!(refused, format!("cannot {name} a done todo"));
        assert_eq!(todo.dump(), before, "{name} changed a done todo");
    }
    // Fields that do not move the lifecycle stay editable on done work.
    let mut todo = todo_in(TodoStatus::Done);
    let request: TodoActionRequest =
        serde_json::from_value(json!({"action": "priority", "value": 1})).unwrap();
    apply_action(&mut todo, &request).expect("priority on a done todo");
    assert_eq!(todo.priority, 1);
    assert_eq!(todo.status, TodoStatus::Done);
}

#[test]
fn unknown_actions_are_refused_without_a_status_change() {
    let mut todo = todo_in(TodoStatus::Claimed);
    assert!(apply_action(&mut todo, &action("finish")).is_err());
    assert_eq!(todo.status, TodoStatus::Claimed);
}

/// todoq writes `merged = true` only for a todo another todo waits on; the
/// server reads the flag and passes it through but never writes it itself.
#[test]
fn the_server_never_writes_merged_on_a_todo_file() {
    for merged in [false, true] {
        for status in TodoStatus::ALL {
            let mut todo = todo_in(status);
            todo.merged = merged;
            let parsed = TodoFile::parse(&todo.dump()).unwrap();
            assert_eq!(parsed.merged, merged);
            assert_eq!(parsed.to_api(Utc::now()).merged, merged);
            for name in ["release", "block", "priority", "mode"] {
                let request: TodoActionRequest = serde_json::from_value(
                    json!({"action": name, "value": if name == "mode" { json!("now") } else { json!(2) }}),
                )
                .unwrap();
                let mut acted = parsed.clone();
                let _ = apply_action(&mut acted, &request);
                assert_eq!(acted.merged, merged, "{name} on {status:?} wrote merged");
            }
        }
    }
}
