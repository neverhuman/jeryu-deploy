//! Work follows the todo's recorded state, end to end: a real queue branch,
//! ledger rows in the shapes older binaries wrote (copied from a production
//! ledger, ids and repositories renamed), the closure snapshot, and the
//! report the route answers with.

use std::collections::BTreeSet;

use rusqlite::params;

use super::closure::snapshot;
use super::report::{LotsPage, Request, build_paged};
use super::{Station, Visit};
use crate::web::WebState;
use crate::web::shift::todo_file::TodoFile;
use crate::web::shift::{TodoStatus, tests::run_git};

const H: i64 = 3_600_000;
const T0: i64 = 1_793_000_000_000;

/// One `flow_ledger` row as an older binary left it.
struct Row<'a> {
    lot: &'a str,
    todo: bool,
    station: &'a str,
    entered: i64,
    started: bool,
    left: Option<i64>,
    direction: &'a str,
    rework_to: Option<&'a str>,
    pr: Option<i64>,
}

fn seed(state: &WebState, rows: &[Row<'_>]) {
    state.events.with_conn_for_test(|conn| {
        for (n, row) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO flow_ledger (lot_id, family, repo, pr, todo_id, shift, station,
                     entered_ms, started_ms, left_ms, direction, rework_to,
                     source_seq_first, source_seq_last)
                 VALUES (?1, 'acme', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
                params![
                    row.lot,
                    row.pr.map(|_| "acme/widget"),
                    row.pr,
                    row.todo.then_some(row.lot),
                    row.todo.then(|| format!("todo/{}", row.lot)),
                    row.station,
                    row.entered,
                    row.started.then_some(row.entered),
                    row.left,
                    row.direction,
                    row.rework_to,
                    i64::try_from(n).unwrap() + 1,
                ],
            )
            .unwrap();
        }
    });
}

fn todo_row<'a>(lot: &'a str, station: &'a str, entered: i64, left: Option<i64>) -> Row<'a> {
    Row {
        lot,
        todo: true,
        station,
        entered,
        started: station == "work",
        left,
        direction: if left.is_some() { "forward" } else { "open" },
        rework_to: None,
        pr: None,
    }
}

/// The queue's record for each todo, on a real `queue` branch.
fn queue(root: &std::path::Path, todos: &[(&str, TodoStatus, bool, bool)]) {
    let repo = root.join("acme/acme-todo.git");
    std::fs::create_dir_all(repo.join("todos")).unwrap();
    run_git(&repo, &["init", "-q"]);
    run_git(&repo, &["checkout", "-q", "-b", "queue"]);
    std::fs::write(repo.join("family.toml"), "[family]\nname = 'acme'\n").unwrap();
    for (id, status, merged, lease) in todos {
        let mut file = TodoFile::new((*id).into(), "acme".into(), format!("work {id}"));
        file.status = *status;
        file.merged = *merged;
        if *lease {
            file.claim_by = "operator/w1".into();
            file.lease_until = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        }
        std::fs::write(repo.join(format!("todos/{id}.md")), file.dump()).unwrap();
    }
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "queue fixture"]);
}

#[test]
fn work_shows_only_what_the_queue_and_the_ledger_say_is_in_progress() {
    let root = tempfile::tempdir().unwrap();
    queue(
        root.path(),
        &[
            ("t-merged", TodoStatus::Done, true, false),
            ("t-landed", TodoStatus::Done, true, false),
            ("t-shift", TodoStatus::Done, false, false),
            ("t-blocked", TodoStatus::Blocked, false, false),
            ("t-held", TodoStatus::Blocked, false, false),
            ("t-open", TodoStatus::Open, false, false),
            ("t-live", TodoStatus::Claimed, false, true),
        ],
    );
    let state =
        WebState::new_with_git_storage(jeryu_core::ForgeCore::new(), root.path().to_path_buf());
    let finished = T0 + H;
    let idle = finished + 3_000;
    seed(
        &state,
        &[
            // Done and merged, landed by today's fold: Work closed, then the
            // idle beat's ghost, then the landing closed it (no ghost left).
            todo_row("t-merged", "work", T0, Some(finished)),
            todo_row("t-merged", "shift", finished, None),
            todo_row("t-merged", "work", idle, None),
            // Done and merged, landed by an older binary: the landing only
            // entered Stage, so Work and Shift stayed open beside it.
            todo_row("t-landed", "work", T0, Some(finished)),
            todo_row("t-landed", "shift", finished, None),
            todo_row("t-landed", "work", idle, None),
            todo_row("t-landed", "stage", finished + 10 * H, None),
            // Done, waiting on its shift pull request: the ghost beside Shift.
            todo_row("t-shift", "work", T0, Some(finished)),
            todo_row("t-shift", "shift", finished, None),
            todo_row("t-shift", "work", idle, None),
            // Blocked after a retry an older binary sent to Queue, then the
            // idle beat's ghost: shown active.
            todo_row("t-blocked", "work", T0, None),
            // Blocked and already held: stays held, not counted.
            Row {
                direction: "hold",
                ..todo_row("t-held", "work", T0, None)
            },
            // Open again with no claim, started visit left behind.
            todo_row("t-open", "work", T0, None),
            // A live claim: work in progress.
            todo_row("t-live", "work", T0 + 2 * H, None),
            // A pull request no todo carries, sent to Work by a red gate.
            Row {
                lot: "acme/widget#7",
                todo: false,
                station: "gate",
                entered: T0,
                started: true,
                left: Some(T0 + H),
                direction: "rework",
                rework_to: Some("work"),
                pr: Some(7),
            },
            Row {
                lot: "acme/widget#7",
                todo: false,
                station: "work",
                entered: T0 + H,
                started: false,
                left: None,
                direction: "open",
                rework_to: None,
                pr: Some(7),
            },
            // One sent to Work by a held review, with a new head since at Gate.
            Row {
                lot: "acme/widget#8",
                todo: false,
                station: "review",
                entered: T0,
                started: true,
                left: Some(T0 + H),
                direction: "rework",
                rework_to: Some("work"),
                pr: Some(8),
            },
            Row {
                lot: "acme/widget#8",
                todo: false,
                station: "work",
                entered: T0 + H,
                started: false,
                left: None,
                direction: "open",
                rework_to: None,
                pr: Some(8),
            },
            Row {
                lot: "acme/widget#8",
                todo: false,
                station: "gate",
                entered: T0 + 2 * H,
                started: false,
                left: None,
                direction: "open",
                rework_to: None,
                pr: Some(8),
            },
        ],
    );
    // t-merged's ghost was closed by today's landing.
    state.events.with_conn_for_test(|conn| {
        conn.execute(
            "UPDATE flow_ledger SET left_ms = ?1, direction = 'forward'
              WHERE lot_id = 't-merged' AND left_ms IS NULL",
            params![finished + 10 * H],
        )
        .unwrap();
    });

    let open = state.events.flow_open_lifecycle(Some("acme")).unwrap();
    let lifecycle = snapshot(&state, &open, None);
    assert_eq!(
        serde_json::to_value(lifecycle.work).unwrap(),
        serde_json::json!({
            "landed_left": 2,
            "done_left": 1,
            "blocked_held": 1,
            "open_waiting": 1,
            "pull_requests_held_where_stopped": 1,
            "pull_requests_left": 1,
        }),
        "t-landed leaves Work and Shift; t-shift leaves Work only"
    );

    let mut visits = state.events.flow_visits(Some("acme"), 0).unwrap();
    lifecycle.settle(&mut visits);
    let request = Request {
        family: Some("acme".into()),
        days: 28,
        now_ms: finished + 20 * H,
    };
    let is_current = |v: &Visit| lifecycle.is_current(v);
    let report = serde_json::to_value(build_paged(
        &request,
        &visits,
        &BTreeSet::from(Station::ALL),
        &is_current,
        LotsPage {
            limit: 100,
            ..LotsPage::default()
        },
    ))
    .unwrap();
    let station = |id: &str| {
        report["stations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        station("work")["wip"],
        serde_json::json!({"waiting": 1, "active": 1, "held": 2}),
        "t-open waits, t-live works, t-blocked and t-held are held"
    );
    let lots = |id: &str| -> Vec<(String, String)> {
        report["lots"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|lot| lot["station"] == id)
            .map(|lot| {
                (
                    lot["lot_id"].as_str().unwrap().to_string(),
                    lot["state"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let mut work = lots("work");
    work.sort();
    assert_eq!(
        work,
        [
            ("t-blocked".into(), "held".into()),
            ("t-held".into(), "held".into()),
            ("t-live".into(), "active".into()),
            ("t-open".into(), "waiting".into()),
        ]
    );
    assert_eq!(
        lots("shift"),
        [("t-shift".to_string(), "waiting".to_string())]
    );
    let mut gate = lots("gate");
    gate.sort();
    assert_eq!(
        gate,
        [
            ("acme/widget#7".into(), "held".into()),
            ("acme/widget#8".into(), "waiting".into()),
        ],
        "a todo-less pull request is held at the gate that stopped it"
    );
    // The ledger itself is untouched.
    let after = state.events.flow_open_lifecycle(Some("acme")).unwrap();
    assert_eq!(after.len(), open.len());
}
