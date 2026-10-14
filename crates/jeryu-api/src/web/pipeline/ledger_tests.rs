//! The durable cost ledger: the 0010 migration's backfill, and what
//! `EventStore::insert` writes to it.

use std::path::Path;

use rusqlite::{Connection, params};
use serde_json::json;

use super::store::{EventStore, RETENTION_MS};
use super::types::NewEvent;
use crate::web::shift::migrate_shift_store_through;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn ledger_count(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM cost_ledger", [], |row| row.get(0))
        .unwrap()
}

fn events_count(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM pipeline_events", [], |row| row.get(0))
        .unwrap()
}

fn reviewed(event_id: Option<&str>, cost_usd: Option<f64>) -> NewEvent {
    NewEvent {
        source: "pr-review".to_string(),
        kind: "pr.reviewed".to_string(),
        summary: "reviewed widget#7".to_string(),
        event_id: event_id.map(str::to_string),
        repo: Some("acme/widget".to_string()),
        pr: Some(7),
        outcome: Some("approve".to_string()),
        cost_usd,
        seconds: Some(90),
        detail: Some(json!({"model": "model-a", "decision": "approve",
            "usage": {"model-a": {"input": 10, "output": 2, "cost_usd": 0.5}}})),
        ..NewEvent::default()
    }
}

/// A store an older binary left (through 0009) holds costed and uncosted
/// events; opening it with this binary copies exactly the costed ones.
#[test]
fn cost_ledger_migration_copies_existing_costed_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shift.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        migrate_shift_store_through(&conn, 9).unwrap();
        for (event_id, kind, cost) in [
            (Some("a-1"), "todo.attempt_finished", Some(1.25)),
            (None, "todo.attempt_finished", Some(0.5)),
            (Some("c-1"), "todo.claimed", None),
            (Some("r-1"), "pr.reviewed", Some(0.75)),
        ] {
            conn.execute(
                "INSERT INTO pipeline_events (ts_ms, event_id, source, kind, reporter, repo, pr,
                   summary, cost_usd, detail_json)
                 VALUES (?1, ?2, 'todoq', ?3, 'operator', 'acme/widget', 7, 's', ?4, '{\"k\":1}')",
                params![1_000, event_id, kind, cost],
            )
            .unwrap();
        }
        let missing: bool = conn
            .query_row(
                "SELECT COUNT(*) = 0 FROM sqlite_master WHERE name = 'cost_ledger'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(missing, "version 9 has no ledger");
    }
    let store = EventStore::open(&path).unwrap();
    assert_eq!(ledger_count(&path), 3, "only the costed events are copied");
    let reviews = store.ledger("pr.reviewed").unwrap();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].repo.as_deref(), Some("acme/widget"));
    assert_eq!(reviews[0].pr, Some(7));
    assert_eq!(reviews[0].cost_usd, 0.75);
    assert_eq!(reviews[0].detail, Some(json!({"k": 1})));
    drop(store);
    // Reopening re-checks the checksum and copies nothing twice.
    EventStore::open(&path).unwrap();
    assert_eq!(ledger_count(&path), 3);
}

/// Only an event with a cost reaches the ledger; a repeated event_id writes
/// nothing; and the ledger keeps what the event log prunes, without counting
/// a re-posted pruned event twice.
#[test]
fn insert_writes_the_ledger_for_costs_only_and_it_outlives_the_prune() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shift.sqlite");
    let store = EventStore::open(&path).unwrap();
    let start = 1_700_000_000_000;

    store
        .insert("reviewer", &reviewed(Some("r-1"), Some(0.5)), start)
        .unwrap();
    store
        .insert("reviewer", &reviewed(Some("r-2"), None), start)
        .unwrap();
    store
        .insert("reviewer", &reviewed(None, Some(0.25)), start)
        .unwrap();
    assert_eq!(ledger_count(&path), 2, "the uncosted event is not spend");

    let again = store
        .insert("reviewer", &reviewed(Some("r-1"), Some(0.5)), start + 1)
        .unwrap();
    assert!(again.duplicate);
    assert_eq!(ledger_count(&path), 2, "a duplicate stays a duplicate");
    // Another reporter's id is its own.
    store
        .insert("other", &reviewed(Some("r-1"), Some(0.5)), start + 2)
        .unwrap();
    assert_eq!(ledger_count(&path), 3);

    // A month and a day later the log prunes every one of them...
    let later = start + RETENTION_MS + DAY_MS;
    store
        .insert("reviewer", &reviewed(Some("r-3"), None), later)
        .unwrap();
    assert_eq!(events_count(&path), 1, "the log kept only the new event");
    assert_eq!(ledger_count(&path), 3, "the ledger is never pruned");
    // ...and a producer that re-posts r-1 is stored in the log again, but
    // its cost is already in the ledger.
    let reposted = store
        .insert("reviewer", &reviewed(Some("r-1"), Some(0.5)), later + 1)
        .unwrap();
    assert!(!reposted.duplicate);
    assert_eq!(ledger_count(&path), 3);
    let total: f64 = store
        .ledger("pr.reviewed")
        .unwrap()
        .iter()
        .map(|row| row.cost_usd)
        .sum();
    assert!((total - 1.25).abs() < 1e-9, "{total}");
}

/// `ledger_between` reads one kind over a half-open window of time, with the
/// family and the time each row was recorded.
#[test]
fn ledger_between_reads_one_kind_inside_a_half_open_window() {
    let dir = tempfile::tempdir().unwrap();
    let store = EventStore::open(&dir.path().join("shift.sqlite")).unwrap();
    let start = 1_700_000_000_000;
    let session = |id: &str, family: Option<&str>| NewEvent {
        source: "todoq-agents".to_string(),
        kind: "session.costed".to_string(),
        summary: format!("session {id}"),
        event_id: Some(id.to_string()),
        family: family.map(str::to_string),
        cost_usd: Some(0.5),
        detail: Some(json!({"session": "claude:s-1"})),
        ..NewEvent::default()
    };
    store
        .insert("ada", &session("sc:before", None), start - 1)
        .unwrap();
    store
        .insert("ada", &session("sc:first", Some("acme")), start)
        .unwrap();
    store
        .insert("ada", &reviewed(Some("r-1"), Some(0.25)), start + 1)
        .unwrap();
    store
        .insert("ada", &session("sc:last", None), start + DAY_MS - 1)
        .unwrap();
    store
        .insert("ada", &session("sc:after", None), start + DAY_MS)
        .unwrap();

    let rows = store
        .ledger_between("session.costed", start, start + DAY_MS)
        .unwrap();
    assert_eq!(rows.len(), 2, "both ends: since included, until excluded");
    assert_eq!(rows[0].ts_ms, start);
    assert_eq!(rows[0].family.as_deref(), Some("acme"));
    assert_eq!(rows[0].detail, Some(json!({"session": "claude:s-1"})));
    assert_eq!(rows[1].ts_ms, start + DAY_MS - 1);
    assert_eq!(rows[1].family, None);
}
