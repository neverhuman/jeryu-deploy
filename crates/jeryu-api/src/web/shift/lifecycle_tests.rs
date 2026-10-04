//! The todo lifecycle: which status moves an admin action may make, and that
//! the server never writes `merged` (todoq sets it only on blocker todos).

use chrono::Utc;
use serde_json::json;

use super::apply_action;
use super::todo_file::TodoFile;
use super::types::{BlockKind, TodoActionRequest, TodoStatus, WorkerState};

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

/// The repos the fixture family lists, which `edit` validates against.
const FAMILY_REPOS: [&str; 2] = ["jeryu-deploy", "jeryu-web"];

fn family_repos() -> Vec<String> {
    FAMILY_REPOS.iter().map(|r| r.to_string()).collect()
}

fn act(todo: &mut TodoFile, request: &TodoActionRequest) -> Result<(), String> {
    apply_action(todo, request, &family_repos())
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
        act(&mut todo, &action("release")).expect("release");
        assert_eq!(todo.status, TodoStatus::Open, "from {from:?}");
        assert!(todo.lease_until.is_empty());
        assert_eq!(todo.attempts, 0);
        assert_eq!(
            todo.note, "",
            "the reason the todo stopped is no longer true"
        );
    }
}

/// The note on a released todo says why it was taken back, not why it stopped.
#[test]
fn a_release_note_replaces_the_one_the_todo_stopped_with() {
    let mut todo = todo_in(TodoStatus::Blocked);
    let request: TodoActionRequest =
        serde_json::from_value(json!({"action": "release", "note": "the slot is gone"})).unwrap();
    act(&mut todo, &request).expect("release");
    assert_eq!(todo.status, TodoStatus::Open);
    assert_eq!(todo.note, "the slot is gone");
}

/// `force` only decides whether the route performs the release (see
/// `tests.rs`); the release it performs is the same one.
#[test]
fn force_changes_nothing_about_the_release_itself() {
    let mut forced = todo_in(TodoStatus::Claimed);
    let request: TodoActionRequest =
        serde_json::from_value(json!({"action": "release", "force": true})).unwrap();
    act(&mut forced, &request).expect("forced release");
    let mut plain = todo_in(TodoStatus::Claimed);
    act(&mut plain, &action("release")).expect("release");
    assert_eq!(forced.dump(), plain.dump());
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
        act(&mut todo, &request).expect("block");
        assert_eq!(todo.status, TodoStatus::Blocked, "from {from:?}");
        assert!(todo.lease_until.is_empty());
        assert_eq!(todo.attempts, 2);
        assert_eq!(todo.note, "needs design");
    }
}

#[test]
fn a_done_todo_refuses_every_action_that_would_move_it_and_is_left_untouched() {
    for name in ["release", "block", "close", "park"] {
        let mut todo = todo_in(TodoStatus::Done);
        todo.commits = vec![("jeryu-deploy".to_string(), "abc123".to_string())];
        let before = todo.dump();
        let refused = act(&mut todo, &action(name)).expect_err(name);
        assert_eq!(refused, format!("cannot {name} a done todo"));
        assert_eq!(todo.dump(), before, "{name} changed a done todo");
    }
    // Fields that do not move the lifecycle stay editable on done work.
    let mut todo = todo_in(TodoStatus::Done);
    let request: TodoActionRequest =
        serde_json::from_value(json!({"action": "priority", "value": 1})).unwrap();
    act(&mut todo, &request).expect("priority on a done todo");
    assert_eq!(todo.priority, 1);
    assert_eq!(todo.status, TodoStatus::Done);
}

#[test]
fn unknown_actions_are_refused_without_a_status_change() {
    let mut todo = todo_in(TodoStatus::Claimed);
    assert!(act(&mut todo, &action("finish")).is_err());
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
            for name in [
                "release", "block", "done", "close", "park", "priority", "mode",
            ] {
                let request: TodoActionRequest = serde_json::from_value(
                    json!({"action": name, "value": if name == "mode" { json!("now") } else { json!(2) }}),
                )
                .unwrap();
                let mut acted = parsed.clone();
                let _ = act(&mut acted, &request);
                assert_eq!(acted.merged, merged, "{name} on {status:?} wrote merged");
            }
        }
    }
}

#[test]
fn done_and_close_end_the_work_and_keep_the_note() {
    for (name, status) in [("done", TodoStatus::Done), ("close", TodoStatus::Closed)] {
        for from in [TodoStatus::Open, TodoStatus::Claimed, TodoStatus::Blocked] {
            let mut todo = todo_in(from);
            let request: TodoActionRequest =
                serde_json::from_value(json!({"action": name, "note": "did it by hand"})).unwrap();
            act(&mut todo, &request).unwrap_or_else(|err| panic!("{name} from {from:?}: {err}"));
            assert_eq!(todo.status, status, "{name} from {from:?}");
            assert!(todo.lease_until.is_empty());
            assert_eq!(todo.note, "did it by hand");
            assert_eq!(todo.attempts, 2, "{name} rewrote the attempt count");
        }
    }
}

#[test]
fn park_stores_the_date_it_comes_back_and_refuses_anything_else() {
    let mut todo = todo_in(TodoStatus::Claimed);
    let request: TodoActionRequest = serde_json::from_value(
        json!({"action": "park", "until": "2026-10-20T09:00:00+02:00", "note": "waits on a tag"}),
    )
    .unwrap();
    act(&mut todo, &request).expect("park");
    assert_eq!(todo.status, TodoStatus::Parked);
    assert!(todo.lease_until.is_empty());
    // Stored in UTC, the spelling every other time in the file uses.
    assert_eq!(todo.park_until, "2026-10-20T07:00:00Z");
    assert_eq!(todo.note, "waits on a tag");

    // A park with no date waits for a person instead of for a clock.
    let mut forever = todo_in(TodoStatus::Open);
    act(&mut forever, &action("park")).expect("park with no date");
    assert_eq!(forever.status, TodoStatus::Parked);
    assert!(forever.park_until.is_empty());

    // Releasing a parked todo forgets the date; so does finishing it.
    let mut released = todo.clone();
    act(&mut released, &action("release")).expect("release");
    assert_eq!(released.status, TodoStatus::Open);
    assert!(released.park_until.is_empty());
    let mut finished = todo.clone();
    act(&mut finished, &action("done")).expect("done");
    assert!(finished.park_until.is_empty());

    let mut bad = todo_in(TodoStatus::Open);
    let request: TodoActionRequest =
        serde_json::from_value(json!({"action": "park", "until": "next tuesday"})).unwrap();
    let refused = act(&mut bad, &request).expect_err("a park needs a date it can read");
    assert!(
        refused.starts_with("until must be an RFC 3339 time"),
        "{refused}"
    );
    assert_eq!(bad.status, TodoStatus::Open);
}

#[test]
fn park_until_round_trips_and_is_written_only_when_it_is_set() {
    let mut todo = todo_in(TodoStatus::Parked);
    todo.park_until = "2026-10-20T07:00:00Z".to_string();
    let dumped = todo.dump();
    assert!(
        dumped.contains("park_until = \"2026-10-20T07:00:00Z\"\n"),
        "{dumped}"
    );
    let parsed = TodoFile::parse(&dumped).expect("parse a parked todo");
    assert_eq!(parsed.park_until, todo.park_until);
    assert_eq!(parsed.status, TodoStatus::Parked);
    assert_eq!(parsed.to_api(Utc::now()).park_until, todo.park_until);
    // A todo nobody parked carries no such key, so a file todoq wrote without
    // it round-trips byte-for-byte.
    let plain = todo_in(TodoStatus::Open).dump();
    assert!(!plain.contains("park_until"), "{plain}");
    assert_eq!(TodoFile::parse(&plain).unwrap().dump(), plain);
}

#[test]
fn edit_sets_the_title_body_and_repos_and_refuses_a_repo_the_family_lacks() {
    let mut todo = todo_in(TodoStatus::Open);
    todo.triaged = false;
    let request: TodoActionRequest = serde_json::from_value(json!({
        "action": "edit", "title": "  Pin jeryu-web  ", "body": "do the thing",
        "repos": ["jeryu-web"],
    }))
    .unwrap();
    act(&mut todo, &request).expect("edit");
    assert_eq!(todo.title, "Pin jeryu-web");
    assert_eq!(todo.body, "do the thing");
    assert_eq!(todo.repos, vec!["jeryu-web".to_string()]);
    // A title and repos are exactly what triage is, so the edit triages it.
    assert!(todo.triaged);
    assert_eq!(todo.status, TodoStatus::Open, "edit moved the lifecycle");

    let before = todo.clone();
    for body in [
        json!({"action": "edit", "repos": ["acme-ops"]}),
        json!({"action": "edit", "repos": [""]}),
        json!({"action": "edit", "title": "   "}),
        json!({"action": "edit"}),
    ] {
        let request: TodoActionRequest = serde_json::from_value(body.clone()).unwrap();
        let mut todo = before.clone();
        assert!(act(&mut todo, &request).is_err(), "{body}");
        assert_eq!(todo.dump(), before.dump(), "a refused edit changed {body}");
    }
}

/// What a worker could not get past decides what the inbox asks of a person,
/// so it is read off the words the todo itself carries.
#[test]
fn block_kind_is_read_off_the_note_the_title_and_the_outcome() {
    let cases = [
        ("OWNER: pick a name", "", "", BlockKind::OwnerTask),
        (
            "Pin the tag",
            "needs the owner first",
            "",
            BlockKind::OwnerTask,
        ),
        ("Pin the tag", "", "over_budget", BlockKind::OverBudget),
        (
            "Pin the tag",
            "spent $9 of a $4 budget cap",
            "",
            BlockKind::OverBudget,
        ),
        (
            "Pin the tag",
            "repo \"acme-ops\" is not in family",
            "",
            BlockKind::UnknownRepo,
        ),
        (
            "Pin the tag",
            "unknown repo in the ask",
            "",
            BlockKind::UnknownRepo,
        ),
        (
            "Pin the tag",
            "finish by hand: the forge refused",
            "",
            BlockKind::Handoff,
        ),
        (
            "Pin the tag",
            "the gate fails on main too",
            "",
            BlockKind::AgentBlocked,
        ),
        ("Pin the tag", "", "", BlockKind::AgentBlocked),
    ];
    for (title, note, outcome, want) in cases {
        assert_eq!(
            BlockKind::derive(TodoStatus::Blocked, title, note, outcome),
            Some(want),
            "{title:?} {note:?} {outcome:?}"
        );
    }
    // A handed-off todo is a handoff whatever its note says.
    assert_eq!(
        BlockKind::derive(TodoStatus::Handoff, "Pin the tag", "the gate fails", ""),
        Some(BlockKind::Handoff)
    );
    // Nothing else carries a kind: only stopped work needs a person.
    for status in [
        TodoStatus::Open,
        TodoStatus::Claimed,
        TodoStatus::Done,
        TodoStatus::Parked,
        TodoStatus::Closed,
    ] {
        assert_eq!(
            BlockKind::derive(status, "OWNER: do it", "over budget", ""),
            None
        );
    }
    for kind in BlockKind::ALL {
        assert_eq!(
            serde_json::to_value(kind).unwrap(),
            json!(kind.as_str()),
            "{kind:?}"
        );
    }
}

/// The kind a blocked todo's own words name reaches the wire, so the web UI
/// and the inbox read the same answer.
#[test]
fn a_blocked_todos_kind_reaches_the_api() {
    let mut todo = todo_in(TodoStatus::Blocked);
    todo.note = "spent the budget cap".to_string();
    assert_eq!(
        todo.to_api(Utc::now()).block_kind,
        Some(BlockKind::OverBudget)
    );
    assert_eq!(
        todo_in(TodoStatus::Open).to_api(Utc::now()).block_kind,
        None
    );
}
