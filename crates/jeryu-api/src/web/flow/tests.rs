//! Flow: real event sequences through the projection, then the report and the
//! route they answer with.
//!
//! Every test here drives `EventStore::insert` with the events a real lot
//! produces and asserts the numbers that come out, not the shape: a lot that
//! runs Queue to Deploy, a red gate that loops back to Work, a held review, a
//! refused merge-queue replay, a killed attempt and a blocked todo.

use std::collections::BTreeSet;
use std::path::Path;

use axum::http::{Request as HttpRequest, StatusCode, header};
use jeryu_core::{ForgeCore, UserRole};
use rusqlite::{Connection, params};
use serde_json::Value;
use tower::ServiceExt;

use super::report::{FLOW_SCHEMA, Request, build, lot_href, window_start};
use super::{Station, Visit};
use crate::web::pipeline::{EventStore, NewEvent};
use crate::web::shift::migrate_shift_store_through;
use crate::web::{WebState, app};

const MINUTE: i64 = 60 * 1000;
const START: i64 = 1_760_000_000_000;

/// A store in a fresh temporary directory, with the directory kept alive.
fn store() -> (tempfile::TempDir, EventStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = EventStore::open(&dir.path().join("shift.sqlite")).unwrap();
    (dir, store)
}

/// One posted event, at the millisecond the server stored it.
struct Post<'a> {
    kind: &'a str,
    outcome: Option<&'a str>,
    todo: Option<&'a str>,
    pr: Option<i64>,
    shift: Option<&'a str>,
    at_ms: i64,
}

/// Post an `acme/widget` event of family `acme`. Todo events carry their
/// repository as todoq posts them, which is what lets a shift pull request of
/// that repository find them.
fn post(store: &EventStore, event: &Post<'_>) {
    post_as(store, "acme", Some("acme/widget"), event);
}

fn post_as(store: &EventStore, family: &str, repo: Option<&str>, event: &Post<'_>) {
    store
        .insert(
            "operator",
            &NewEvent {
                source: "todoq".to_string(),
                kind: event.kind.to_string(),
                summary: format!("{} at {}", event.kind, event.at_ms),
                outcome: event.outcome.map(str::to_string),
                family: Some(family.to_string()),
                repo: repo.map(str::to_string),
                pr: event.pr,
                todo_id: event.todo.map(str::to_string),
                shift: event.shift.map(str::to_string),
                ..NewEvent::default()
            },
            event.at_ms,
        )
        .unwrap();
}

/// A whole lot, Queue to Deploy, with no loop anywhere: filed at `base_ms`,
/// every station taking the same handful of minutes. With
/// `pin` the work is carried into its consumer by a pin bump; without it the
/// repository releases itself and the lot never visits Pin.
///
/// The events are posted oldest first, as the log holds them.
fn forward_lot(store: &EventStore, todo: &str, pr: i64, base_ms: i64, pin: bool) {
    let shift = format!("dayshift/{todo}");
    let mut steps: Vec<(&str, Option<&str>, i64, bool, bool)> = vec![
        ("todo.filed", None, 0, false, false),
        ("todo.claimed", None, 2, false, false),
        ("worker.stage", None, 3, false, false),
        ("todo.attempt_finished", Some("done"), 13, false, true),
        ("pr.opened", None, 15, true, true),
        ("review.started", None, 16, true, false),
        ("gate.started", None, 17, true, false),
        ("review.finished", Some("approve"), 20, true, false),
        ("gate.finished", Some("success"), 23, true, false),
        ("queue.enqueued", None, 24, true, false),
        ("queue.landed", None, 30, true, false),
        ("release.staged", None, 40, true, false),
        ("deploy.created", None, 41, true, false),
        ("deploy.status", Some("success"), 45, true, false),
    ];
    if pin {
        steps.push(("pin.bump_opened", None, 32, false, false));
    }
    steps.sort_by_key(|(_, _, offset, _, _)| *offset);
    for (kind, outcome, offset, carry_pr, carry_shift) in steps {
        post(
            store,
            &Post {
                kind,
                outcome,
                todo: Some(todo),
                pr: carry_pr.then_some(pr),
                shift: carry_shift.then_some(shift.as_str()),
                at_ms: base_ms + offset * MINUTE,
            },
        );
    }
}

/// Every visit of family `acme`: a window starting at the epoch reads the
/// whole ledger.
fn visits_of(store: &EventStore) -> Vec<Visit> {
    store.flow_visits(Some("acme"), 0).unwrap()
}

fn at(visits: &[Visit], station: Station) -> Vec<&Visit> {
    visits
        .iter()
        .filter(|visit| visit.station == station)
        .collect()
}

/// The 28-day report, read with the same window bound the route uses.
fn report_of(store: &EventStore, now_minutes: i64) -> Value {
    let request = Request {
        family: Some("acme".to_string()),
        days: 28,
        now_ms: START + now_minutes * MINUTE,
    };
    let since = window_start(request.now_ms, request.days);
    let visits = store.flow_visits(Some("acme"), since).unwrap();
    let reached = store.flow_reached(Some("acme")).unwrap();
    serde_json::to_value(build(&request, &visits, &reached)).unwrap()
}

fn station(report: &Value, id: &str) -> Value {
    report["stations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|station| station["id"] == id)
        .unwrap()
        .clone()
}

/// The one lot that flows: every station is visited once, in order, and the
/// durations are the ones the events describe.
#[test]
fn a_lot_that_flows_leaves_one_forward_visit_per_station() {
    let (_dir, store) = store();
    forward_lot(&store, "20261012-000001-aaaaaa", 7, START, false);
    let visits = visits_of(&store);

    for station in Station::ALL {
        let own = at(&visits, station);
        if station == Station::Pin {
            // The repository releases itself: Pin is not on this lot's route
            // and is left out of its numbers.
            assert!(own.is_empty(), "nothing pinned this work");
            continue;
        }
        assert_eq!(own.len(), 1, "{} visited once", station.id());
        assert_eq!(own[0].direction, "forward", "{} left forward", station.id());
    }
    // Queue: filed at 0, claimed at 2.
    let queue = at(&visits, Station::Queue)[0];
    assert_eq!(queue.started_ms, Some(START + 2 * MINUTE));
    assert_eq!(queue.left_ms, Some(START + 2 * MINUTE));
    // Work: claimed at 2, the agent picked it up at 3, done at 13.
    let work = at(&visits, Station::Work)[0];
    assert_eq!(work.entered_ms, START + 2 * MINUTE);
    assert_eq!(work.started_ms, Some(START + 3 * MINUTE));
    assert_eq!(work.left_ms, Some(START + 13 * MINUTE));
    // Shift: landed at 13, the shift pull request opened at 15.
    // Shift's tool is the pull request opener, a timer with no start signal:
    // arrival and exit measure dwell, but execution time remains unknown.
    let shift = at(&visits, Station::Shift)[0];
    assert_eq!(shift.entered_ms, START + 13 * MINUTE);
    assert_eq!(shift.started_ms, None, "timer wait is not execution");
    assert_eq!(shift.left_ms, Some(START + 15 * MINUTE));
    // Merge was entered when the head was approved and green, not before.
    let merge = at(&visits, Station::Merge)[0];
    assert_eq!(merge.entered_ms, START + 23 * MINUTE, "green at 23");
    assert_eq!(
        merge.started_ms,
        Some(START + 24 * MINUTE),
        "enqueued at 24"
    );
    assert_eq!(merge.left_ms, Some(START + 30 * MINUTE), "landed at 30");
    let deploy = at(&visits, Station::Deploy)[0];
    assert_eq!(deploy.left_ms, Some(START + 45 * MINUTE));
    // Nothing is left on the line.
    assert!(visits.iter().all(|visit| visit.left_ms.is_some()));
}

/// A red gate sends the lot back to Work; it comes back and lands. The Gate
/// station then has two visits and the loop is countable.
#[test]
fn a_red_gate_loops_back_to_work_and_the_lot_returns() {
    let (_dir, store) = store();
    let todo = "20261012-000002-bbbbbb";
    let shift = "dayshift/red";
    let steps: &[(&str, Option<&str>, i64, bool)] = &[
        ("todo.filed", None, 0, false),
        ("todo.claimed", None, 1, false),
        ("worker.stage", None, 2, false),
        ("todo.attempt_finished", Some("done"), 10, false),
        ("pr.opened", None, 11, true),
        ("gate.started", None, 12, true),
        ("gate.finished", Some("failure"), 20, true),
        // Back at Work: a second attempt, a new head, a green gate.
        ("worker.stage", None, 21, false),
        ("todo.attempt_finished", Some("done"), 30, false),
        ("pr.opened", None, 31, true),
        ("gate.started", None, 32, true),
        ("gate.finished", Some("success"), 38, true),
        ("pr.review", Some("approve"), 39, true),
        ("pr.merged", None, 40, true),
    ];
    for (kind, outcome, minute, carry_pr) in steps {
        post(
            &store,
            &Post {
                kind,
                outcome: *outcome,
                todo: Some(todo),
                pr: carry_pr.then_some(9),
                shift: Some(shift),
                at_ms: START + (*minute) * MINUTE,
            },
        );
    }
    let visits = visits_of(&store);
    let gates = at(&visits, Station::Gate);
    assert_eq!(gates.len(), 2, "two gate visits, one per head");
    assert_eq!(gates[0].direction, "rework");
    assert_eq!(gates[0].rework_to, Some(Station::Work));
    assert_eq!(gates[0].left_ms, Some(START + 20 * MINUTE));
    assert_eq!(gates[1].direction, "forward");

    let work = at(&visits, Station::Work);
    assert_eq!(work.len(), 2, "the red gate opened a second Work visit");
    assert_eq!(work[1].entered_ms, START + 20 * MINUTE);
    assert_eq!(work[1].left_ms, Some(START + 30 * MINUTE));
    assert_eq!(
        at(&visits, Station::Merge)[0].left_ms,
        Some(START + 40 * MINUTE)
    );

    // The station report counts the loop and says the lot did not pass first
    // time, with the rate over the visits that left.
    let report = report_of(&store, 60);
    let gate = station(&report, "gate");
    assert_eq!(gate["rework"][0]["to"], "work");
    assert_eq!(gate["rework"][0]["count"], 1);
    assert_eq!(gate["rework"][0]["rate"], 0.5, "one of the two that left");
}

/// A reviewer that holds sends the work back; the todoq attempt that is
/// blocked is held at Work until a person releases it.
#[test]
fn a_held_review_and_a_blocked_todo_remain_at_their_stations() {
    let (_dir, store) = store();
    let held = "20261012-000003-cccccc";
    for (kind, outcome, minute, carry_pr) in [
        ("todo.filed", None, 0, false),
        ("todo.claimed", None, 1, false),
        ("todo.attempt_finished", Some("done"), 9, false),
        ("pr.opened", None, 10, true),
        ("review.started", None, 11, true),
        ("review.finished", Some("hold"), 18, true),
    ] {
        post(
            &store,
            &Post {
                kind,
                outcome,
                todo: Some(held),
                pr: carry_pr.then_some(11),
                shift: Some("dayshift/held"),
                at_ms: START + (minute) * MINUTE,
            },
        );
    }
    let blocked = "20261012-000004-dddddd";
    for (kind, outcome, minute) in [
        ("todo.filed", None, 0),
        ("todo.claimed", None, 2),
        ("worker.stage", None, 3),
        ("todo.attempt_finished", Some("blocked"), 20),
    ] {
        post(
            &store,
            &Post {
                kind,
                outcome,
                todo: Some(blocked),
                pr: None,
                shift: None,
                at_ms: START + (minute) * MINUTE,
            },
        );
    }

    let visits = visits_of(&store);
    let review = at(&visits, Station::Review);
    assert_eq!(review.len(), 1);
    assert_eq!(review[0].direction, "hold");
    assert_eq!(review[0].rework_to, None);

    let work: Vec<&Visit> = at(&visits, Station::Work)
        .into_iter()
        .filter(|visit| visit.lot_id == blocked)
        .collect();
    assert_eq!(work.len(), 1);
    assert_eq!(
        work[0].direction, "hold",
        "a blocked todo is held, not gone"
    );
    assert!(work[0].left_ms.is_none(), "a held lot has not left");

    // Work holds one lot and waits on the one the reviewer sent back.
    let report = report_of(&store, 30);
    let work = station(&report, "work");
    assert_eq!(work["wip"]["held"], 1);
    assert_eq!(work["wip"]["waiting"], 0, "review hold stays at review");
    assert_eq!(work["wip"]["active"], 0);
    assert_eq!(
        work["oldest_wait_s"],
        (30 - 2) * 60,
        "the held lot has been at Work since it was claimed at 2"
    );

    // A held lot has no ETA: its next step is a person.
    let held_lot = report["lots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|lot| lot["lot_id"] == blocked)
        .unwrap()
        .clone();
    assert_eq!(held_lot["state"], "held");
    assert!(held_lot["eta_s"].is_null());
    assert!(held_lot["slowest_ahead"].is_null());
    assert_eq!(
        held_lot["eta_partial"], false,
        "no ETA is not a partial one"
    );
    assert_eq!(held_lot["eta_unknown_stations"], serde_json::json!([]));

    // The person releases it and it is waiting for a slot again.
    post(
        &store,
        &Post {
            kind: "todo.action",
            outcome: Some("released"),
            todo: Some(blocked),
            pr: None,
            shift: None,
            at_ms: START + (25) * MINUTE,
        },
    );
    let report = report_of(&store, 30);
    let work = station(&report, "work");
    assert_eq!(work["wip"]["held"], 0);
    // The slot still had it when the hold went on, so it is working again,
    // while the review hold stays at Review.
    assert_eq!(work["wip"]["active"], 1);
    assert_eq!(work["wip"]["waiting"], 0);
}

/// A refused merge-queue replay and a killed attempt are both rework: one
/// sends the lot from Merge back to Work, the other from Work to Queue.
#[test]
fn a_refused_replay_and_a_killed_attempt_both_loop_back() {
    let (_dir, store) = store();
    let refused = "20261012-000005-eeeeee";
    for (kind, outcome, minute, carry_pr) in [
        ("todo.filed", None, 0, false),
        ("todo.claimed", None, 1, false),
        ("todo.attempt_finished", Some("done"), 8, false),
        ("pr.opened", None, 9, true),
        ("review.finished", Some("approve"), 12, true),
        ("gate.finished", Some("success"), 13, true),
        ("queue.enqueued", None, 14, true),
        ("queue.refused", None, 19, true),
    ] {
        post(
            &store,
            &Post {
                kind,
                outcome,
                todo: Some(refused),
                pr: carry_pr.then_some(13),
                shift: Some("dayshift/refused"),
                at_ms: START + (minute) * MINUTE,
            },
        );
    }
    let killed = "20261012-000006-ffffff";
    for (kind, outcome, minute) in [
        ("todo.filed", None, 0),
        ("todo.claimed", None, 1),
        ("worker.stage", None, 2),
        ("todo.attempt_finished", Some("killed"), 15),
    ] {
        post(
            &store,
            &Post {
                kind,
                outcome,
                todo: Some(killed),
                pr: None,
                shift: None,
                at_ms: START + (minute) * MINUTE,
            },
        );
    }

    let visits = visits_of(&store);
    let merge: Vec<&Visit> = at(&visits, Station::Merge)
        .into_iter()
        .filter(|visit| visit.lot_id == refused)
        .collect();
    assert_eq!(merge.len(), 1);
    assert_eq!(merge[0].direction, "rework");
    assert_eq!(merge[0].rework_to, Some(Station::Work));
    assert_eq!(merge[0].started_ms, Some(START + 14 * MINUTE));
    assert_eq!(merge[0].left_ms, Some(START + 19 * MINUTE));

    let queue: Vec<&Visit> = at(&visits, Station::Queue)
        .into_iter()
        .filter(|visit| visit.lot_id == killed)
        .collect();
    assert_eq!(queue.len(), 2, "the killed attempt requeued the todo");
    assert!(queue[1].left_ms.is_none(), "it waits for another slot");
    let work: Vec<&Visit> = at(&visits, Station::Work)
        .into_iter()
        .filter(|visit| visit.lot_id == killed)
        .collect();
    assert_eq!(work[0].direction, "rework");
    assert_eq!(work[0].rework_to, Some(Station::Queue));

    let report = report_of(&store, 30);
    let merge = station(&report, "merge");
    assert_eq!(merge["rework"][0]["to"], "work");
    assert_eq!(merge["rework"][0]["count"], 1);
    assert_eq!(merge["rework"][0]["rate"], 1.0, "nothing else left Merge");
    assert_eq!(merge["first_pass_rate"], Value::Null, "nothing passed yet");
    let work = station(&report, "work");
    assert_eq!(work["rework"][0]["to"], "queue");
}

/// A pull request no todo carries is its own lot, and the todo that lands it
/// adopts it, so the same work is never counted twice.
#[test]
fn a_pull_request_lot_is_adopted_by_the_todo_that_carries_it() {
    let (dir, store) = store();
    for (kind, outcome, minute) in [
        ("pr.opened", None, 0),
        ("review.finished", Some("approve"), 5),
        ("gate.finished", Some("success"), 6),
    ] {
        post(
            &store,
            &Post {
                kind,
                outcome,
                todo: None,
                pr: Some(21),
                shift: None,
                at_ms: START + (minute) * MINUTE,
            },
        );
    }
    let lots: Vec<String> = visits_of(&store).iter().map(|v| v.lot_id.clone()).collect();
    assert!(
        lots.iter().all(|lot| lot == "acme/widget#21"),
        "{lots:?} is the pull request's own lot"
    );

    let conn = Connection::open(dir.path().join("shift.sqlite")).unwrap();
    conn.execute("INSERT INTO flow_activity SELECT event_seq,'20261012-000007-777777',station,state,ts_ms,family,repo,pr,sha,summary,runner,log_url FROM flow_activity WHERE lot_id='acme/widget#21'",[]).unwrap();

    post(
        &store,
        &Post {
            kind: "pr.merged",
            outcome: None,
            todo: Some("20261012-000007-777777"),
            pr: Some(21),
            shift: None,
            at_ms: START + (9) * MINUTE,
        },
    );
    let lots: Vec<String> = visits_of(&store).iter().map(|v| v.lot_id.clone()).collect();
    assert!(
        lots.iter().all(|lot| lot == "20261012-000007-777777"),
        "{lots:?} is one lot, the todo's"
    );
    let after = visits_of(&store);
    let stage = at(&after, Station::Stage);
    assert_eq!(stage.len(), 1, "the merge put it on one Stage visit");
    let aliases: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM flow_activity WHERE lot_id='acme/widget#21'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        aliases, 0,
        "adoption merges duplicate evidence keys instead of retaining alias counts"
    );
}

/// A shift pull request names no todo, so the branch is the join: it moves
/// every lot whose work landed on that shift from Shift to Review and Gate.
#[test]
fn a_shift_pull_request_moves_every_lot_on_its_branch() {
    let (_dir, store) = store();
    let shift = "dayshift/2026-10-12";
    for todo in ["20261012-000041-aaaaaa", "20261012-000042-bbbbbb"] {
        for (kind, outcome, minute) in [
            ("todo.filed", None, 0),
            ("todo.claimed", None, 1),
            ("todo.attempt_finished", Some("done"), 8),
        ] {
            post(
                &store,
                &Post {
                    kind,
                    outcome,
                    todo: Some(todo),
                    pr: None,
                    shift: Some(shift),
                    at_ms: START + minute * MINUTE,
                },
            );
        }
    }
    assert_eq!(at(&visits_of(&store), Station::Shift).len(), 2);

    post(
        &store,
        &Post {
            kind: "shift.pr_opened",
            outcome: None,
            todo: None,
            pr: Some(31),
            shift: Some(shift),
            at_ms: START + 10 * MINUTE,
        },
    );
    let visits = visits_of(&store);
    for station in [Station::Review, Station::Gate] {
        let own = at(&visits, station);
        assert_eq!(own.len(), 2, "both lots reached {}", station.id());
    }
    for visit in at(&visits, Station::Shift) {
        assert_eq!(visit.direction, "forward");
        assert_eq!(visit.left_ms, Some(START + 10 * MINUTE));
    }
    // Both lots are now one pull request's worth of work, and each is still
    // its own lot.
    let lots: BTreeSet<&str> = visits.iter().map(|visit| visit.lot_id.as_str()).collect();
    assert_eq!(lots.len(), 2, "{lots:?}");
}

/// A todo that lands on a shift branch after its pull request opened is not
/// the open head's work: a verdict on that head moves the lots the pull request
/// carries, and the lot waits at Shift until a head that holds it is gated.
#[test]
fn a_later_todo_on_an_open_shift_pull_request_waits_for_the_next_head() {
    let (_dir, store) = store();
    let shift = "dayshift/2026-10-12";
    let finish = |todo: &str, done_at: i64| {
        for (kind, outcome, minute) in [
            ("todo.filed", None, 0),
            ("todo.claimed", None, 1),
            ("todo.attempt_finished", Some("done"), done_at),
        ] {
            post(
                &store,
                &Post {
                    kind,
                    outcome,
                    todo: Some(todo),
                    pr: None,
                    shift: Some(shift),
                    at_ms: START + minute * MINUTE,
                },
            );
        }
    };
    let on_pr = |kind: &str, outcome: Option<&str>, minute: i64| {
        post(
            &store,
            &Post {
                kind,
                outcome,
                todo: None,
                pr: Some(31),
                shift: Some(shift),
                at_ms: START + minute * MINUTE,
            },
        );
    };
    let (a, b, c) = (
        "20261012-000041-aaaaaa",
        "20261012-000042-bbbbbb",
        "20261012-000043-cccccc",
    );
    finish(a, 8);
    finish(b, 8);
    on_pr("shift.pr_opened", None, 10);
    finish(c, 12);
    on_pr("gate.finished", Some("success"), 14);

    let visits = visits_of(&store);
    let of = |lot: &str, station: Station| -> Vec<Visit> {
        at(&visits, station)
            .into_iter()
            .filter(|visit| visit.lot_id == lot)
            .cloned()
            .collect()
    };
    for lot in [a, b] {
        let gate = of(lot, Station::Gate);
        assert_eq!(gate.len(), 1, "{lot} has one gate visit");
        assert_eq!(
            gate[0].left_ms,
            Some(START + 14 * MINUTE),
            "{lot} left the gate"
        );
    }
    // C was not in the gated head: it is still at Shift and nowhere else.
    assert!(of(c, Station::Gate).is_empty());
    assert!(of(c, Station::Review).is_empty());
    assert_eq!(of(c, Station::Shift)[0].left_ms, None);

    // The next head holds C: gating it carries C onto the pull request.
    on_pr("gate.started", None, 16);
    let visits = visits_of(&store);
    let of = |lot: &str, station: Station| -> Vec<Visit> {
        at(&visits, station)
            .into_iter()
            .filter(|visit| visit.lot_id == lot)
            .cloned()
            .collect()
    };
    assert_eq!(of(c, Station::Shift)[0].left_ms, Some(START + 16 * MINUTE));
    assert_eq!(of(c, Station::Review).len(), 1);
    assert_eq!(of(c, Station::Gate).len(), 1);
    // A and B are still the pull request's lots, not dropped by the sweep.
    for lot in [a, b] {
        assert_eq!(
            of(lot, Station::Review).len(),
            1,
            "{lot} still under review"
        );
    }
}

#[test]
fn a_lot_links_to_its_todo_or_to_its_pull_request() {
    assert_eq!(
        lot_href("20261012-000041-aaaaaa"),
        "/work/20261012-000041-aaaaaa"
    );
    assert_eq!(lot_href("acme/widget#31"), "/acme/widget/pull/31");
    assert_eq!(lot_href("acme/team/widget#7"), "/acme%2Fteam/widget/pull/7");
    assert_eq!(lot_href("odd lot#x"), "/work/odd%20lot%23x");
}

/// Enough lots give every station percentiles, a bottleneck and an ETA; a
/// station with fewer than three samples is null and says why.
#[test]
fn percentiles_the_bottleneck_and_an_eta_come_from_the_samples() {
    let (_dir, store) = store();
    for (index, todo) in [
        "20261012-000011-aaaaaa",
        "20261012-000012-bbbbbb",
        "20261012-000013-cccccc",
        "20261012-000014-dddddd",
    ]
    .into_iter()
    .enumerate()
    {
        forward_lot(
            &store,
            todo,
            100 + index as i64,
            START + index as i64 * 60 * MINUTE,
            false,
        );
    }
    let report = report_of(&store, 24 * 60);

    // Every lot takes the same minutes, so both percentiles are that number.
    let work = station(&report, "work");
    assert_eq!(
        work["queue_s"]["p50"], 60,
        "claimed one minute after filing"
    );
    assert_eq!(work["process_s"]["p50"], 10 * 60);
    assert_eq!(work["process_s"]["p90"], 10 * 60);
    assert_eq!(work["first_pass_rate"], 1.0);
    assert!(work["rework"].as_array().unwrap().is_empty());
    assert_eq!(work["throughput_per_hour"], 0.01, "four lots in 28 days");

    // Stage's tool is the release timer: no queue, and the whole wait from
    // the merge at 30 to the staged release at 40 is process time.
    let stage = station(&report, "stage");
    assert!(stage["queue_s"].is_null());
    assert!(stage["process_s"].is_null());
    // Queue holds the most lot-hours of waiting: two minutes for a slot,
    // longer than any other station's queue.
    let queue = station(&report, "queue");
    assert_eq!(queue["queue_s"]["p50"], 2 * 60);
    assert_eq!(queue["bottleneck"], true);
    assert_eq!(stage["bottleneck"], false);
    assert_eq!(
        report["stations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|station| station["bottleneck"] == true)
            .count(),
        1,
        "one bottleneck"
    );

    // The line finished four lots, each 45 minutes from filing to deployed.
    assert_eq!(report["line"]["finished"], 4);
    assert_eq!(report["line"]["wip"], 0);
    assert_eq!(report["line"]["cycle_s"]["p50"], 45 * 60);
    assert!(
        report["line"]["x_factor"]["p50"].as_f64().unwrap() > 1.0,
        "the line waits as well as works"
    );

    // Pin is on the route but nothing pinned anything: it is unknown, not
    // zero, and it says why.
    let pin = station(&report, "pin");
    assert!(pin["queue_s"].is_null());
    assert_eq!(pin["wip"]["waiting"], 0);
    assert!(
        pin["unknown"]
            .as_array()
            .unwrap()
            .iter()
            .any(|unknown| unknown["metric"] == "all" && unknown["reason"] == "no_events"),
        "{pin}"
    );
    // Capacity waits on the heartbeat half of Flow, and says so.
    assert!(pin["capacity"].is_null());
    assert!(
        pin["unknown"]
            .as_array()
            .unwrap()
            .iter()
            .any(|unknown| unknown["reason"] == "no_heartbeat")
    );

    // A fifth lot now in flight gets an ETA from the four that went before.
    // Pin has no estimate: it is named, left out of the sum, and the ETA says
    // it is partial rather than being dropped or counting Pin as zero.
    post(
        &store,
        &Post {
            kind: "todo.filed",
            outcome: None,
            todo: Some("20261012-000015-eeeeee"),
            pr: None,
            shift: None,
            at_ms: START + (24 * 60) * MINUTE,
        },
    );
    let report = report_of(&store, 24 * 60 + 1);
    let waiting = report["lots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|lot| lot["lot_id"] == "20261012-000015-eeeeee")
        .unwrap()
        .clone();
    assert_eq!(waiting["state"], "waiting");
    assert_eq!(waiting["station"], "queue");
    assert_eq!(waiting["eta_partial"], true, "{waiting}");
    assert_eq!(
        waiting["eta_unknown_stations"],
        serde_json::json!(["pin"]),
        "Pin has no estimate, so the ETA names it instead of guessing"
    );
    // Queue's two minutes less the one already spent (60), plus every other
    // station with an estimate: Work 660, Shift 120, Review 300, Gate 480,
    // Merge 420, Stage 600, Deploy 300. Pin contributes nothing.
    let sum = 60 + 660 + 120 + 300 + 480 + 420 + 600 + 300;
    assert_eq!(waiting["eta_s"]["p50"], sum, "{waiting}");
    assert_eq!(
        waiting["eta_s"]["p90"], sum,
        "every lot took the same minutes"
    );
    assert_eq!(waiting["slowest_ahead"], "work");
}

/// With no estimate anywhere ahead there is no ETA, and every station the
/// lot still has to pass is named.
#[test]
fn an_eta_with_no_estimate_ahead_is_null_and_names_every_station() {
    let (_dir, store) = store();
    post(
        &store,
        &Post {
            kind: "todo.filed",
            outcome: None,
            todo: Some("20261012-000016-ffffff"),
            pr: None,
            shift: None,
            at_ms: START,
        },
    );
    let report = report_of(&store, 5);
    let lot = &report["lots"][0];
    assert_eq!(lot["station"], "queue");
    assert!(lot["eta_s"].is_null(), "{lot}");
    assert!(lot["slowest_ahead"].is_null(), "{lot}");
    assert_eq!(lot["eta_partial"], true);
    assert_eq!(
        lot["eta_unknown_stations"],
        serde_json::json!(Station::ALL.map(Station::id).to_vec())
    );
}

/// Three lots that all pass through Pin give every station on the route an
/// estimate, and then an in-flight lot has a real ETA and a slowest station.
#[test]
fn an_eta_sums_the_stations_still_ahead() {
    let (_dir, store) = store();
    for (index, todo) in [
        "20261012-000021-aaaaaa",
        "20261012-000022-bbbbbb",
        "20261012-000023-cccccc",
    ]
    .into_iter()
    .enumerate()
    {
        forward_lot(
            &store,
            todo,
            200 + index as i64,
            START + index as i64 * 60 * MINUTE,
            true,
        );
    }
    let report = report_of(&store, 24 * 60);
    assert_eq!(station(&report, "pin")["process_s"]["p50"], 8 * 60);

    // A lot that has just been claimed: Work takes a minute of queue and ten
    // of process, and every station after it adds its own p50.
    post(
        &store,
        &Post {
            kind: "todo.filed",
            outcome: None,
            todo: Some("20261012-000024-dddddd"),
            pr: None,
            shift: None,
            at_ms: START + (24 * 60) * MINUTE,
        },
    );
    post(
        &store,
        &Post {
            kind: "todo.claimed",
            outcome: None,
            todo: Some("20261012-000024-dddddd"),
            pr: None,
            shift: None,
            at_ms: START + (24 * 60 + 2) * MINUTE,
        },
    );
    let report = report_of(&store, 24 * 60 + 4);
    let lot = report["lots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|lot| lot["lot_id"] == "20261012-000024-dddddd")
        .unwrap()
        .clone();
    assert_eq!(lot["station"], "work");
    let eta = lot["eta_s"]["p50"]
        .as_i64()
        .expect("every station has a p50");
    assert!(eta > 0, "{lot}");
    // Work's own eleven minutes less the two already spent (540), plus the
    // p50 of every station still ahead: Shift 120, Review 300, Gate 480,
    // Merge 420, Pin 480, Stage 600, Deploy 300.
    assert_eq!(eta, 540 + 120 + 300 + 480 + 420 + 480 + 600 + 300);
    assert_eq!(
        lot["eta_partial"], false,
        "total dwell does not require a measured start"
    );
    assert_eq!(lot["eta_unknown_stations"], serde_json::json!([]));
    // Work itself, at eleven minutes, is the slowest station the lot still
    // has to get through; Stage, at ten, is the slowest one after it.
    assert_eq!(lot["slowest_ahead"], "work");
}

/// The projection is a pure function of the event sequence: a store an older
/// binary left (through 0010) is replayed into visits on the first open, and
/// opening it again replays nothing twice.
#[test]
fn the_migration_replays_every_retained_event_into_an_empty_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shift.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        migrate_shift_store_through(&conn, 10).unwrap();
        let missing: bool = conn
            .query_row(
                "SELECT COUNT(*) = 0 FROM sqlite_master WHERE name = 'flow_ledger'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(missing, "version 10 has no flow ledger");
        for (minute, kind, outcome) in [
            (0, "todo.filed", None),
            (1, "todo.claimed", None),
            (2, "worker.stage", None),
            (9, "todo.attempt_finished", Some("done")),
        ] {
            conn.execute(
                "INSERT INTO pipeline_events (ts_ms, source, kind, reporter, family, todo_id,
                   outcome, summary)
                 VALUES (?1, 'todoq', ?2, 'operator', 'acme', '20261012-000099-999999', ?3, 's')",
                params![START + minute * MINUTE, kind, outcome],
            )
            .unwrap();
        }
    }
    let store = EventStore::open(&path).unwrap();
    let visits = visits_of(&store);
    assert_eq!(visits.len(), 3, "queue, work and the shift it landed on");
    assert_eq!(
        at(&visits, Station::Work)[0].left_ms,
        Some(START + 9 * MINUTE)
    );
    let replays: i64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM flow_ledger_replay", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(replays, 1);
    drop(store);

    let store = EventStore::open(&path).unwrap();
    assert_eq!(
        visits_of(&store).len(),
        3,
        "a restart replays nothing twice"
    );
}

/// `(station, direction, repo, pr)` of every visit of `lot`, oldest first,
/// read straight from the ledger.
fn ledger_rows(
    dir: &tempfile::TempDir,
    lot: &str,
) -> Vec<(String, String, Option<String>, Option<i64>)> {
    let conn = Connection::open(dir.path().join("shift.sqlite")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT station, direction, repo, pr FROM flow_ledger WHERE lot_id = ?1 ORDER BY seq",
        )
        .unwrap();
    stmt.query_map(params![lot], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

/// Shift branch names are shared by every repository and family. A shift pull
/// request of one repository moves only that repository's lots on the branch:
/// the other repository of the same family, the other family's repository of
/// the same name, and a lot that recorded no repository all stay at Shift
/// with no pull request stamped on them.
#[test]
fn a_shift_pull_request_moves_only_its_own_repositorys_lots() {
    let (dir, store) = store();
    let shift = "dayshift/2026-10-12";
    let app_lots = ["20261012-000051-aaaaaa", "20261012-000052-bbbbbb"];
    let web_lot = "20261012-000053-cccccc";
    let globex_lot = "20261012-000054-dddddd";
    let no_repo_lot = "20261012-000055-eeeeee";
    let landed: [(&str, &str, Option<&str>); 5] = [
        (app_lots[0], "acme", Some("acme/app")),
        (app_lots[1], "acme", Some("acme/app")),
        (web_lot, "acme", Some("acme/web")),
        // Another family, a repository of the same short name.
        (globex_lot, "globex", Some("globex/app")),
        (no_repo_lot, "acme", None),
    ];
    for (todo, family, repo) in landed {
        for (kind, outcome, minute) in [
            ("todo.filed", None, 0),
            ("todo.claimed", None, 1),
            ("todo.attempt_finished", Some("done"), 8),
        ] {
            post_as(
                &store,
                family,
                repo,
                &Post {
                    kind,
                    outcome,
                    todo: Some(todo),
                    pr: None,
                    shift: Some(shift),
                    at_ms: START + minute * MINUTE,
                },
            );
        }
    }

    let untouched = |lot: &str| {
        let rows = ledger_rows(&dir, lot);
        let at_shift: Vec<_> = rows.iter().filter(|row| row.0 == "shift").collect();
        assert_eq!(at_shift.len(), 1, "{lot}: {rows:?}");
        assert_eq!(at_shift[0].1, "open", "{lot} is still at Shift: {rows:?}");
        assert!(
            rows.iter().all(|row| row.3.is_none()),
            "{lot} carries no pull request: {rows:?}"
        );
        assert!(
            rows.iter()
                .all(|row| !matches!(row.0.as_str(), "review" | "gate" | "merge" | "stage")),
            "{lot} never went past Shift: {rows:?}"
        );
    };

    // acme/app's shift pull request opens.
    post_as(
        &store,
        "acme",
        Some("acme/app"),
        &Post {
            kind: "shift.pr_opened",
            outcome: None,
            todo: None,
            pr: Some(31),
            shift: Some(shift),
            at_ms: START + 10 * MINUTE,
        },
    );
    for lot in app_lots {
        let rows = ledger_rows(&dir, lot);
        let shift_row = rows.iter().find(|row| row.0 == "shift").unwrap();
        assert_eq!(shift_row.1, "forward", "{lot}: {rows:?}");
        for station in ["review", "gate"] {
            let row = rows.iter().find(|row| row.0 == station).unwrap();
            assert_eq!(row.2.as_deref(), Some("acme/app"), "{lot}: {rows:?}");
            assert_eq!(row.3, Some(31), "{lot}: {rows:?}");
        }
    }
    for lot in [web_lot, globex_lot, no_repo_lot] {
        untouched(lot);
    }
    // The other repository's and family's rows keep their own repository.
    assert!(
        ledger_rows(&dir, web_lot)
            .iter()
            .all(|row| row.2.as_deref() == Some("acme/web"))
    );
    assert!(
        ledger_rows(&dir, globex_lot)
            .iter()
            .all(|row| row.2.as_deref() == Some("globex/app"))
    );

    // The pull request goes green, is approved and merges: both of its lots
    // reach Stage, and nothing else moves.
    for (kind, outcome, minute) in [
        ("gate.finished", Some("success"), 14),
        ("pr.review", Some("approve"), 15),
        ("pr.merged", None, 20),
    ] {
        post_as(
            &store,
            "acme",
            Some("acme/app"),
            &Post {
                kind,
                outcome,
                todo: None,
                pr: Some(31),
                shift: Some(shift),
                at_ms: START + minute * MINUTE,
            },
        );
    }
    for lot in app_lots {
        let rows = ledger_rows(&dir, lot);
        let merge = rows.iter().find(|row| row.0 == "merge").unwrap();
        assert_eq!(merge.1, "forward", "{lot}: {rows:?}");
        let stage = rows.iter().find(|row| row.0 == "stage").unwrap();
        assert_eq!(stage.1, "open", "{lot} is waiting to be staged: {rows:?}");
    }
    for lot in [web_lot, globex_lot, no_repo_lot] {
        untouched(lot);
    }
}

/// An event re-posted under the same `event_id` after the log pruned the
/// first copy is stored in the log again, but the ledger counts it once.
/// A replay that fails leaves the store usable and records nothing, so the
/// next start replays again.
#[test]
fn a_failed_replay_does_not_stop_the_store_from_opening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shift.sqlite");
    {
        let store = EventStore::open(&path).unwrap();
        forward_lot(&store, "20261012-000051-dddddd", 51, START, false);
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "DELETE FROM flow_ledger;
             DELETE FROM flow_ledger_events;
             DELETE FROM flow_ledger_replay;
             CREATE TRIGGER refuse_flow BEFORE INSERT ON flow_ledger
               BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    }
    let store = EventStore::open(&path).expect("a failed replay still opens the store");
    assert!(visits_of(&store).is_empty());
    drop(store);

    let conn = rusqlite::Connection::open(&path).unwrap();
    let replays: i64 = conn
        .query_row("SELECT COUNT(*) FROM flow_ledger_replay", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(replays, 0, "a failed replay is retried at the next start");
    conn.execute_batch("DROP TRIGGER refuse_flow;").unwrap();
    drop(conn);
    let store = EventStore::open(&path).unwrap();
    assert!(!visits_of(&store).is_empty(), "the retry replays the log");
}

#[test]
fn an_event_reposted_after_the_prune_is_projected_once() {
    let (_dir, store) = store();
    let todo = "20261012-000061-aaaaaa";
    let insert = |kind: &str, outcome: Option<&str>, event_id: &str, at_ms: i64| {
        store
            .insert(
                "operator",
                &NewEvent {
                    source: "todoq".to_string(),
                    kind: kind.to_string(),
                    summary: kind.to_string(),
                    event_id: Some(event_id.to_string()),
                    outcome: outcome.map(str::to_string),
                    family: Some("acme".to_string()),
                    repo: Some("acme/widget".to_string()),
                    todo_id: Some(todo.to_string()),
                    ..NewEvent::default()
                },
                at_ms,
            )
            .unwrap()
    };
    insert("todo.filed", None, "filed-1", START);
    insert("todo.claimed", None, "claimed-1", START + MINUTE);
    insert(
        "todo.attempt_finished",
        Some("done"),
        "done-1",
        START + 9 * MINUTE,
    );
    let before = visits_of(&store);
    assert_eq!(before.len(), 3, "queue, work and shift: {before:?}");

    // A month and a day later, an unrelated event triggers the prune.
    let later = START + 31 * 24 * 60 * MINUTE;
    store
        .insert(
            "operator",
            &NewEvent {
                source: "todoq".to_string(),
                kind: "worker.error".to_string(),
                summary: "unrelated".to_string(),
                ..NewEvent::default()
            },
            later,
        )
        .unwrap();
    // The producer posts the claim again under its old id. The log has
    // forgotten it and stores it as new...
    let again = insert("todo.claimed", None, "claimed-1", later + MINUTE);
    assert!(!again.duplicate, "the log pruned the first copy");
    // ...and the same goes for the attempt that finished.
    let again = insert(
        "todo.attempt_finished",
        Some("done"),
        "done-1",
        later + 2 * MINUTE,
    );
    assert!(!again.duplicate);

    let after = visits_of(&store);
    assert_eq!(after, before, "no second Queue, Work or Shift visit");
    for station in [Station::Queue, Station::Work, Station::Shift] {
        assert_eq!(at(&after, station).len(), 1, "{}", station.id());
    }
}

/// The route reads only the lots still on the line or that left a station
/// inside its window, and the report it builds from them is the report the
/// whole ledger would give.
#[test]
fn a_report_reads_only_the_window_and_answers_as_the_whole_ledger_would() {
    let (_dir, store) = store();
    let day = 24 * 60 * MINUTE;
    // Finished sixty days before the report: outside every 28-day window.
    forward_lot(&store, "20261012-000071-aaaaaa", 401, START, true);
    // Filed sixty days ago too, and still waiting for its review.
    let stuck = "20261012-000072-bbbbbb";
    for (kind, outcome, minute, pr) in [
        ("todo.filed", None, 0, None),
        ("todo.claimed", None, 1, None),
        ("todo.attempt_finished", Some("done"), 8, None),
        ("pr.opened", None, 9, Some(402)),
    ] {
        post(
            &store,
            &Post {
                kind,
                outcome,
                todo: Some(stuck),
                pr,
                shift: Some("dayshift/stuck"),
                at_ms: START + minute * MINUTE,
            },
        );
    }
    // A lot that went round the gate sixty days ago and only deployed inside
    // the window: its old loop still counts against first pass.
    let looped = "20261012-000073-cccccc";
    for (kind, outcome, at_ms, pr) in [
        ("todo.filed", None, START, None),
        ("todo.claimed", None, START + MINUTE, None),
        (
            "todo.attempt_finished",
            Some("done"),
            START + 5 * MINUTE,
            None,
        ),
        ("pr.opened", None, START + 6 * MINUTE, Some(403)),
        (
            "gate.finished",
            Some("failure"),
            START + 7 * MINUTE,
            Some(403),
        ),
        ("worker.stage", None, START + 50 * day, None),
        (
            "todo.attempt_finished",
            Some("done"),
            START + 50 * day + MINUTE,
            None,
        ),
        ("pr.opened", None, START + 50 * day + 2 * MINUTE, Some(403)),
        (
            "gate.finished",
            Some("success"),
            START + 50 * day + 3 * MINUTE,
            Some(403),
        ),
        (
            "pr.review",
            Some("approve"),
            START + 50 * day + 4 * MINUTE,
            Some(403),
        ),
        ("pr.merged", None, START + 50 * day + 5 * MINUTE, Some(403)),
        (
            "release.staged",
            None,
            START + 50 * day + 6 * MINUTE,
            Some(403),
        ),
        (
            "deploy.created",
            None,
            START + 50 * day + 7 * MINUTE,
            Some(403),
        ),
        (
            "deploy.status",
            Some("success"),
            START + 50 * day + 8 * MINUTE,
            Some(403),
        ),
    ] {
        post(
            &store,
            &Post {
                kind,
                outcome,
                todo: Some(looped),
                pr,
                shift: Some("dayshift/looped"),
                at_ms,
            },
        );
    }
    for (index, todo) in [
        "20261012-000074-dddddd",
        "20261012-000075-eeeeee",
        "20261012-000076-ffffff",
    ]
    .into_iter()
    .enumerate()
    {
        forward_lot(
            &store,
            todo,
            404 + index as i64,
            START + 55 * day + index as i64 * 60 * MINUTE,
            false,
        );
    }

    let request = Request {
        family: Some("acme".to_string()),
        days: 28,
        now_ms: START + 60 * day,
    };
    let since = window_start(request.now_ms, request.days);
    let bounded = store.flow_visits(Some("acme"), since).unwrap();
    let lots: BTreeSet<&str> = bounded.iter().map(|visit| visit.lot_id.as_str()).collect();
    assert!(
        !lots.contains("20261012-000071-aaaaaa"),
        "a lot that finished before the window is not read"
    );
    assert!(lots.contains(stuck), "work still on the line is read");
    assert_eq!(
        bounded
            .iter()
            .filter(|visit| visit.lot_id == looped)
            .count(),
        visits_of(&store)
            .iter()
            .filter(|visit| visit.lot_id == looped)
            .count(),
        "a lot in the window is read whole, its old loop included"
    );
    assert_eq!(lots.len(), 5, "{lots:?}");

    let whole = visits_of(&store);
    assert!(whole.len() > bounded.len());
    let reached = store.flow_reached(Some("acme")).unwrap();
    assert!(
        reached.contains(&Station::Pin),
        "only the lot outside the window was pinned, and Pin was still reached"
    );
    let every: BTreeSet<Station> = whole.iter().map(|visit| visit.station).collect();
    assert_eq!(reached, every);
    let report = serde_json::to_value(build(&request, &bounded, &reached)).unwrap();
    assert_eq!(
        report,
        serde_json::to_value(build(&request, &whole, &every)).unwrap(),
        "the bound changes what is read, never the answer"
    );
    // Pin's only visit is older than the window: it says the window has no
    // samples, not that nothing ever reached it.
    let pin = station(&report, "pin");
    assert!(
        pin["unknown"]
            .as_array()
            .unwrap()
            .iter()
            .all(|unknown| unknown["metric"] != "all"),
        "{pin}"
    );
    // And the loop sixty days ago still costs the gate its first pass.
    assert_eq!(station(&report, "gate")["first_pass_rate"], 0.75);
    // A family with no lots reached nothing.
    assert!(store.flow_reached(Some("globex")).unwrap().is_empty());
}

fn get(uri: &str, token: &str) -> HttpRequest<axum::body::Body> {
    HttpRequest::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn json_of(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

#[test]
fn closure_reconciliation_preserves_observed_metrics_and_changes_only_current_inventory() {
    let visits: Vec<_> = (1..=3)
        .map(|n| Visit {
            lot_id: format!("lot-{n}"),
            station: Station::Gate,
            entered_ms: START,
            started_ms: Some(START + n * MINUTE),
            left_ms: (n < 3).then_some(START + 10 * MINUTE),
            direction: if n < 3 { "forward" } else { "open" }.into(),
            rework_to: None,
        })
        .collect();
    let request = Request {
        family: None,
        days: 7,
        now_ms: START + 20 * MINUTE,
    };
    let reached = BTreeSet::from([Station::Gate]);
    let before = build(&request, &visits, &reached);
    let after =
        super::report::build_reconciled(&request, &visits, &reached, &|v| v.lot_id != "lot-3");
    assert_eq!(before.line.wip, 1);
    assert_eq!(after.line.wip, 0);
    assert_eq!(before.lots.len(), 1);
    assert!(after.lots.is_empty());
    for (old, new) in before.stations.iter().zip(&after.stations) {
        assert_eq!(old.queue_s, new.queue_s);
        assert_eq!(old.process_s, new.process_s);
        assert_eq!(old.total_s, new.total_s);
        assert_eq!(old.first_pass_rate, new.first_pass_rate);
        assert_eq!(old.throughput_per_hour, new.throughput_per_hour);
        assert_eq!(old.bottleneck, new.bottleneck);
        assert_eq!(old.unknown, new.unknown);
        if old.id == "gate" {
            assert_eq!(
                old.queue_s.unwrap().p50,
                120,
                "three observed queue samples"
            );
            assert_eq!(old.wip.active, 1);
            assert_eq!(new.wip.active, 0);
            assert!(old.oldest_wait_s.is_some());
            assert!(new.oldest_wait_s.is_none());
        }
    }
    assert_eq!(before.line.finished, after.line.finished);
    assert_eq!(before.line.cycle_s, after.line.cycle_s);
    assert_eq!(before.line.x_factor, after.line.x_factor);
}

#[tokio::test]
async fn flow_route_reports_real_closed_todos_and_merged_upstream_count() {
    use crate::web::shift::{TodoStatus, tests::run_git, todo_file::TodoFile};
    use jeryu_core::{
        CreatePullRequestRequest, CreateRepositoryRequest, PullRequestState,
        UpdatePullRequestRequest,
    };
    let root = tempfile::tempdir().unwrap();
    let queue = root.path().join("acme/acme-todo.git");
    std::fs::create_dir_all(queue.join("todos")).unwrap();
    run_git(&queue, &["init", "-q"]);
    run_git(&queue, &["checkout", "-q", "-b", "queue"]);
    std::fs::write(queue.join("family.toml"), "[family]\nname = 'Acme-split'\n").unwrap();
    let mut todo = TodoFile::new(
        "closed-todo".into(),
        "acme-split".into(),
        "unresolved task".into(),
    );
    todo.status = TodoStatus::Closed;
    std::fs::write(queue.join("todos/closed.md"), todo.dump()).unwrap();
    run_git(&queue, &["add", "."]);
    run_git(&queue, &["commit", "-q", "-m", "closed todo"]);
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    let token = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    core.create_repository(
        "acme",
        CreateRepositoryRequest {
            name: "widget".into(),
            private: false,
            description: None,
            default_branch: Some("main".into()),
        },
    )
    .unwrap();
    let pr = core
        .create_pull_request(
            "acme",
            "widget",
            "alice",
            CreatePullRequestRequest {
                title: "already merged".into(),
                head: "feature".into(),
                base: "main".into(),
                head_sha: Some("a".repeat(40)),
                ..Default::default()
            },
        )
        .unwrap();
    core.update_pull_request(
        "acme",
        "widget",
        pr.number,
        UpdatePullRequestRequest {
            state: Some(PullRequestState::Merged),
            ..Default::default()
        },
    )
    .unwrap();
    let state = WebState::new_with_git_storage(core, root.path().to_path_buf())
        .with_auth(true, false, false);
    let now = chrono::Utc::now().timestamp_millis();
    post(
        &state.events,
        &Post {
            kind: "todo.filed",
            outcome: None,
            todo: Some("closed-todo"),
            pr: None,
            shift: None,
            at_ms: now - MINUTE,
        },
    );
    post(
        &state.events,
        &Post {
            kind: "pr.opened",
            outcome: None,
            todo: None,
            pr: Some(pr.number as i64),
            shift: None,
            at_ms: now - MINUTE,
        },
    );
    let original = state.events.flow_visits(Some("acme"), 0).unwrap();
    let events = state.events.clone();
    let router = app(state, root.path());
    let response = router
        .oneshot(get("/api/v1/flow?family=acme-split&days=7", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_of(response).await;
    assert_eq!(body["family"], "acme");
    assert_eq!(body["closure_record_count"], 1);
    assert_eq!(body["closures_truncated"], false);
    assert_eq!(body["closures"][0]["lot_id"], "closed-todo");
    assert_eq!(body["closures"][0]["kind"], "todo");
    assert_eq!(body["closures"][0]["needs_attention"], true);
    assert_eq!(body["closures"][0]["href"], "/work/closed-todo?family=acme");
    assert_eq!(body["lifecycle_problems"], serde_json::json!([]));
    assert!(body["merged_upstream_record_count"].as_u64().unwrap() > 0);
    assert_eq!(body["line"]["wip"], 0);
    assert_eq!(body["lots"], serde_json::json!([]));
    assert_eq!(events.flow_visits(Some("acme"), 0).unwrap(), original);
}

/// The route: admin-only, strict about its window, and answering numbers that
/// are not all zero while the ledger holds visits.
#[tokio::test]
async fn flow_route_is_admin_only_refuses_bad_windows_and_answers_real_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    let admin = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let user = core
        .create_personal_access_token("bob", "t", None)
        .unwrap()
        .secret;
    let state = WebState::new_with_git_storage(core, dir.path().to_path_buf())
        .with_auth(true, false, false);
    let now = chrono::Utc::now().timestamp_millis();
    for (index, todo) in [
        "20261012-000031-aaaaaa",
        "20261012-000032-bbbbbb",
        "20261012-000033-cccccc",
    ]
    .into_iter()
    .enumerate()
    {
        // Anchored to now, so the lots are inside every window the route
        // offers.
        forward_lot(
            &state.events,
            todo,
            300 + index as i64,
            now - (3 - index as i64) * 60 * MINUTE,
            false,
        );
    }
    // One lot still in flight, so the live numbers are not all history.
    state
        .events
        .insert(
            "operator",
            &NewEvent {
                source: "todoq".to_string(),
                kind: "todo.filed".to_string(),
                summary: "filed".to_string(),
                family: Some("acme".to_string()),
                todo_id: Some("20261012-000034-dddddd".to_string()),
                ..NewEvent::default()
            },
            now,
        )
        .unwrap();
    let router = app(state, Path::new("/tmp/jeryu-no-spa"));
    let call = |uri: &str, token: &str| router.clone().oneshot(get(uri, token));

    let refused = call("/api/v1/flow", &user).await.unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_of(refused).await["code"], "permission_denied");

    assert_eq!(
        call("/api/v1/flow/history?station=gate", &user)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call("/api/v1/flow/history?station=queue&days=7", &admin)
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    for bad in [
        "/api/v1/flow/history?repo=missing-owner",
        "/api/v1/flow/history?repo=/empty",
        "/api/v1/flow/history?station=unknown",
        "/api/v1/flow/history?day=not-a-day",
        "/api/v1/flow/history?state=unknown",
        "/api/v1/flow/history?offset=100001",
        "/api/v1/flow?days=14",
        "/api/v1/flow?days=seven",
        "/api/v1/flow?window=30d",
        "/api/v1/flow?days=7&window=28d",
        "/api/v1/flow?days=7d",
        "/api/v1/flow?window=7",
        "/api/v1/flow?dayss=7",
    ] {
        let response = call(bad, &admin).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        let body = json_of(response).await;
        assert_eq!(body["code"], "invalid_query", "{bad}");
        assert!(body["repair_hint"].is_string(), "{bad}: house error shape");
    }
    let unknown = call("/api/v1/flow?family=initech", &admin).await.unwrap();
    assert_eq!(unknown.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json_of(unknown).await["code"], "family_unknown");

    let response = call("/api/v1/flow?family=acme-split&window=7d", &admin)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_of(response).await;
    assert_eq!(body["schema_version"], FLOW_SCHEMA);
    assert_eq!(body["family"], "acme", "either spelling, canonical back");
    assert_eq!(body["days"], 7);
    assert_eq!(body["closures"], serde_json::json!([]));
    assert_eq!(body["closure_record_count"], 0);
    assert_eq!(body["closures_truncated"], false);
    assert_eq!(body["merged_upstream_record_count"], 0);
    assert!(
        !body["lifecycle_problems"].as_array().unwrap().is_empty(),
        "fixture's missing todo source is explicit"
    );
    assert_eq!(body["stations"].as_array().unwrap().len(), 9);
    assert_eq!(
        body["stations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|station| station["id"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        Station::ALL.map(Station::id).to_vec(),
        "the route in order"
    );

    // Events exist, so the answer cannot be nine all-zero stations.
    let alive = body["stations"].as_array().unwrap().iter().any(|station| {
        station["queue_s"]["p50"].as_i64().unwrap_or(0) > 0
            || station["process_s"]["p50"].as_i64().unwrap_or(0) > 0
            || station["wip"]["waiting"].as_u64().unwrap_or(0) > 0
    });
    assert!(
        alive,
        "a projected ledger never reports an idle line: {body}"
    );
    assert_eq!(body["line"]["finished"], 3);
    assert_eq!(body["line"]["wip"], 1, "the lot still in the queue");
    assert_eq!(station(&body, "queue")["wip"]["waiting"], 1);
    assert_eq!(body["lots"][0]["lot_id"], "20261012-000034-dddddd");
    assert_eq!(body["lots"][0]["href"], "/work/20261012-000034-dddddd");

    // A window that ends before anything happened keeps the live state and
    // drops the history.
    let narrow = json_of(call("/api/v1/flow?days=7", &admin).await.unwrap()).await;
    assert_eq!(narrow["window"]["until"], narrow["generated_at"]);
}

#[test]
fn daily_history_keeps_retries_holds_and_family_filters_distinct() {
    let (_dir, store) = store();
    let mut event = NewEvent {
        event_id: Some("one".into()),
        family: Some("acme".into()),
        repo: Some("acme/widget".into()),
        pr: Some(1),
        source: "test".into(),
        kind: "gate.started".into(),
        summary: "run one".into(),
        detail: Some(serde_json::json!({"runner":"runner-1"})),
        ..Default::default()
    };
    store.insert("operator", &event, START).unwrap();
    store.insert("operator", &event, START + 1).unwrap();
    event.event_id = Some("two".into());
    event.kind = "gate.finished".into();
    event.outcome = Some("error".into());
    store.insert("operator", &event, START + MINUTE).unwrap();
    event.event_id = Some("three".into());
    event.kind = "gate.started".into();
    event.outcome = None;
    store
        .insert("operator", &event, START + 2 * MINUTE)
        .unwrap();
    event.event_id = Some("four".into());
    event.kind = "gate.finished".into();
    event.outcome = Some("success".into());
    store
        .insert("operator", &event, START + 3 * MINUTE)
        .unwrap();
    let mut query = super::history::Query {
        family: Some("acme".into()),
        station: Some("gate".into()),
        days: Some(7),
        ..Default::default()
    };
    let history = store.flow_history(&query, START + 4 * MINUTE).unwrap();
    let body = serde_json::to_value(history).unwrap();
    let day = &crate::web::shift::rfc3339_ms(START)[..10];
    assert_eq!(body["daily"][day]["arrived"], 2);
    assert_eq!(body["daily"][day]["started"], 2);
    assert_eq!(body["daily"][day]["failed"], 1);
    assert_eq!(body["daily"][day]["passed"], 1);
    assert_eq!(body["daily"].as_object().unwrap().len(), 7);
    query.state = Some("failed".into());
    query.day = Some(day.into());
    let body =
        serde_json::to_value(store.flow_history(&query, START + 4 * MINUTE).unwrap()).unwrap();
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["runner"], "runner-1");
    query.family = Some("globex".into());
    assert_eq!(
        store
            .flow_history(&query, START + 4 * MINUTE)
            .unwrap()
            .total,
        0
    );
    query.family = Some("acme".into());
    query.repo = Some("acme/another".into());
    assert_eq!(
        store
            .flow_history(&query, START + 4 * MINUTE)
            .unwrap()
            .total,
        0
    );
}

#[test]
fn verified_delivery_closes_only_member_lots_and_replay_is_idempotent() {
    let (_dir, store) = store();
    for pr in [1, 2] {
        store
            .insert(
                "operator",
                &NewEvent {
                    family: Some("acme".into()),
                    repo: Some("acme/widget".into()),
                    pr: Some(pr),
                    sha: Some(format!("{pr:040x}")),
                    source: "forge".into(),
                    kind: "pr.merged".into(),
                    summary: "merged".into(),
                    ..Default::default()
                },
                START,
            )
            .unwrap();
    }
    let revision = super::delivery::Revision {
        repo: "acme/widget".into(),
        pr: 1,
        sha: format!("{:040x}", 1),
        family: Some("acme".into()),
    };
    let mut delivery = super::delivery::Delivery {
        seq: 100,
        ts: START + MINUTE,
        kind: "release.staged".into(),
        outcome: None,
        family: Some("acme".into()),
        repo: "acme/deploy".into(),
        sha: format!("{:040x}", 3),
        detail: Some(serde_json::json!({"release":"release-1"})),
    };
    store
        .flow_apply_delivery(&delivery, std::slice::from_ref(&revision))
        .unwrap();
    let visits = store.flow_visits(Some("acme"), START).unwrap();
    assert!(visits.iter().any(|v| v.lot_id == "acme/widget#1"
        && v.station == Station::Deploy
        && v.direction == "hold"));
    assert!(visits.iter().any(|v| v.lot_id == "acme/widget#2"
        && v.station == Station::Stage
        && v.left_ms.is_none()));
    delivery.seq = 101;
    delivery.ts += MINUTE;
    delivery.kind = "deploy.created".into();
    store
        .flow_apply_delivery(&delivery, std::slice::from_ref(&revision))
        .unwrap();
    delivery.seq = 102;
    delivery.ts += MINUTE;
    delivery.kind = "deploy.status".into();
    delivery.outcome = Some("success".into());
    store
        .flow_apply_delivery(&delivery, std::slice::from_ref(&revision))
        .unwrap();
    store
        .flow_apply_delivery(&delivery, std::slice::from_ref(&revision))
        .unwrap();
    let visits = store.flow_visits(Some("acme"), START).unwrap();
    assert_eq!(
        visits
            .iter()
            .filter(|v| v.station == Station::Deploy && v.direction == "forward")
            .count(),
        1
    );
    let query = super::history::Query {
        family: Some("acme".into()),
        lot: Some("acme/widget#1".into()),
        days: Some(7),
        ..Default::default()
    };
    let body =
        serde_json::to_value(store.flow_history(&query, START + 10 * MINUTE).unwrap()).unwrap();
    assert_eq!(body["deliveries"].as_array().unwrap().len(), 3);
    assert_eq!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["state"] == "held")
            .count(),
        1
    );
    let other = super::history::Query {
        family: Some("globex".into()),
        ..query
    };
    assert!(
        store
            .flow_history(&other, START + 10 * MINUTE)
            .unwrap()
            .deliveries
            .is_empty()
    );
}

#[test]
fn delivery_reconciliation_is_serialized_and_unreadable_events_back_off() {
    let (_dir, store) = store();
    let clone = store.clone();
    let guard = store.flow_delivery_guard().unwrap();
    assert!(clone.flow_delivery_guard().is_none());
    drop(guard);
    assert!(
        clone.flow_delivery_guard().is_none(),
        "completed reconciliation is throttled too"
    );
    for n in 1..=2 {
        store
            .insert(
                "operator",
                &NewEvent {
                    source: "release".into(),
                    kind: "release.staged".into(),
                    summary: "staged".into(),
                    repo: Some("acme/widget".into()),
                    sha: Some(format!("{n:040x}")),
                    ..Default::default()
                },
                START + n,
            )
            .unwrap();
    }
    let (first, _) = store.flow_delivery_inputs(START + 3).unwrap();
    assert_eq!(first.len(), 2);
    store
        .flow_defer_delivery(first[0].seq, START + 300_000)
        .unwrap();
    let (ready, _) = store.flow_delivery_inputs(START + 4).unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(
        ready[0].seq, first[1].seq,
        "missing objects must not starve newer events"
    );
    assert_eq!(
        store.flow_delivery_inputs(START + 300_001).unwrap().0.len(),
        2
    );
}

#[test]
fn repository_scope_keeps_early_work_after_a_todo_is_bound_to_its_pull() {
    let (_dir, store) = store();
    let todo = "20261012-000007-777777";
    for (kind, minute) in [("todo.filed", 0), ("todo.claimed", 1)] {
        post_as(
            &store,
            "acme",
            None,
            &Post {
                kind,
                outcome: None,
                todo: Some(todo),
                pr: None,
                shift: None,
                at_ms: START + minute * MINUTE,
            },
        );
    }
    post(
        &store,
        &Post {
            kind: "pr.opened",
            outcome: None,
            todo: Some(todo),
            pr: Some(21),
            shift: None,
            at_ms: START + 2 * MINUTE,
        },
    );
    let all = store.flow_visits(Some("acme"), START).unwrap();
    let scoped = store
        .flow_visits_scoped(Some("acme"), START, Some("acme/widget"))
        .unwrap();
    assert_eq!(scoped.len(), all.len());
    assert!(
        scoped
            .iter()
            .any(|v| v.station == Station::Queue && v.entered_ms == START)
    );
    let history = store
        .flow_history(
            &super::history::Query {
                repo: Some("acme/widget".into()),
                days: Some(7),
                ..Default::default()
            },
            START + 3 * MINUTE,
        )
        .unwrap();
    assert!(
        history
            .items
            .iter()
            .any(|i| i.station == "queue" && i.lot_id == todo)
    );
    let wip = store.flow_repositories(Some("acme")).unwrap();
    assert!(
        wip.as_array()
            .unwrap()
            .iter()
            .any(|r| r["repo"] == "acme/widget" && r["station"] == "review" && r["waiting"] == 1)
    );
    assert!(
        store
            .flow_visits_scoped(Some("acme"), START, Some("acme/other"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn real_event_journey_preserves_dwell_without_inventing_execution_efficiency() {
    let (_dir, store) = store();
    for n in 1..=3 {
        forward_lot(
            &store,
            &format!("20261012-00000{n}-aaaaaa"),
            n,
            START,
            false,
        );
    }
    let now = START + 60 * MINUTE;
    let raw = store.scorecard_raw(Some("acme"), now, 7).unwrap();
    let ctx = crate::web::scorecard::compute::Ctx::new(&raw, &[], None, now, 7);
    let metrics = crate::web::scorecard::compute::metrics(&ctx);
    let efficiency = serde_json::to_value(
        &metrics
            .iter()
            .find(|(name, _)| *name == "flow_efficiency")
            .unwrap()
            .1,
    )
    .unwrap();
    assert!(efficiency["value"].is_null());
    assert_eq!(
        efficiency["unknown"]["reason"],
        "execution_starts_not_recorded"
    );
    assert_eq!(efficiency["detail"]["shipped_lots"], 3);
    assert_eq!(efficiency["detail"]["unmeasured_execution_lots"], 3);
    let report = report_of(&store, 60);
    let stage = station(&report, "stage");
    assert_eq!(stage["total_s"]["p50"], 600);
    assert!(stage["process_s"].is_null());
    assert!(stage["queue_s"].is_null());
}

#[test]
fn late_stage_completion_does_not_close_a_newer_visit() {
    let (_dir, store) = store();
    for (kind, offset) in [("pr.merged", 10), ("release.staged", 5)] {
        post(
            &store,
            &Post {
                kind,
                outcome: None,
                todo: None,
                pr: Some(1),
                shift: None,
                at_ms: START + offset * MINUTE,
            },
        );
    }
    let visits = visits_of(&store);
    let stage = at(&visits, Station::Stage)[0];
    assert_eq!(stage.entered_ms, START + 10 * MINUTE);
    assert!(stage.left_ms.is_none());
    assert!(
        visits
            .iter()
            .all(|v| v.left_ms.is_none_or(|left| left >= v.entered_ms))
    );
}

#[test]
fn repeated_hold_refreshes_reason_without_counting_another_transition() {
    let (_dir, store) = store();
    for (offset, outcome) in [(0, "blocked"), (1, "parked")] {
        post(
            &store,
            &Post {
                kind: "todo.action",
                outcome: Some(outcome),
                todo: Some("20261012-000001-aaaaaa"),
                pr: None,
                shift: None,
                at_ms: START + offset * MINUTE,
            },
        );
    }
    let visits = visits_of(&store);
    assert_eq!(at(&visits, Station::Work)[0].direction, "hold");
    let conn = Connection::open(_dir.path().join("shift.sqlite")).unwrap();
    let reason: String = conn
        .query_row(
            "SELECT outcome FROM flow_ledger WHERE station='work' AND left_ms IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "parked");
    let query = super::history::Query {
        family: Some("acme".into()),
        state: Some("held".into()),
        days: Some(7),
        ..Default::default()
    };
    assert_eq!(
        store
            .flow_history(&query, START + 10 * MINUTE)
            .unwrap()
            .total,
        1
    );
}

#[test]
fn lock_replay_migration_only_invalidates_affected_reads_and_preserves_evidence() {
    let (dir, store) = store();
    for repo in ["acme/jeryu-deploy", "acme/widget"] {
        post_as(
            &store,
            "acme",
            Some(repo),
            &Post {
                kind: "release.staged",
                outcome: None,
                todo: None,
                pr: None,
                shift: None,
                at_ms: START,
            },
        );
    }
    let conn = Connection::open(dir.path().join("shift.sqlite")).unwrap();
    conn.execute("INSERT INTO flow_delivery_reads(event_seq,retry_after_ms) SELECT seq,0 FROM pipeline_events", []).unwrap();
    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM pipeline_events", [], |r| r.get(0))
        .unwrap();
    conn.execute_batch(include_str!(
        "../../../../../db/migrations/0021_flow_lock_replay.sql"
    ))
    .unwrap();
    let cached: Vec<String> = conn
        .prepare("SELECT repo FROM pipeline_events JOIN flow_delivery_reads ON event_seq=seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(cached, ["acme/widget"]);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM pipeline_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        events
    );
}
