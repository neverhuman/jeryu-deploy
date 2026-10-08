//! Pool slots: the heartbeat fields an external fleet reports beside the
//! queue's own slots (`kind`, `detail`, `capacity`, `active`), what
//! `/api/v1/shift/workers` gives back, and how a pool's load is counted into
//! the hourly capacity buckets.

use std::path::Path;

use axum::http::{Method as HttpMethod, StatusCode};
use chrono::{TimeZone, Utc};
use jeryu_core::{ForgeCore, UserRole};
use serde_json::json;
use tower::ServiceExt;

use super::heartbeats::{HeartbeatStore, StoredHeartbeat, build_history, migrate_through};
use super::tests::{body_json, request};
use super::types::{Heartbeat, WorkerKind};
use crate::web::{WebState, app};

/// One pool member's beat: `held` jobs of `capacity`, with the short reason
/// behind its state.
fn pool_beat(
    ms: i64,
    slot: &str,
    state: &str,
    detail: Option<&str>,
    capacity: Option<i64>,
    held: Option<i64>,
) -> StoredHeartbeat {
    StoredHeartbeat {
        received_ms: ms,
        heartbeat: Heartbeat {
            operator: "operator".into(),
            host: "node-b".into(),
            slot: slot.into(),
            family: "globex".into(),
            state: state.into(),
            todo_id: None,
            stage: None,
            lease_until: None,
            shift: None,
            planned_slots: None,
            schedule: None,
            version: None,
            kind: WorkerKind::Pool.as_str().into(),
            detail: detail.map(str::to_string),
            capacity,
            active: held,
        },
    }
}

/// A pool member holds several jobs at once, so the hour's `busy` is the jobs
/// it ran, not the one slot that ran them. A member that reports no load at
/// all is still one busy slot while it works.
#[test]
fn capacity_buckets_count_a_pools_jobs_not_its_slots() {
    let hour = 3_600_000;
    let t0 = Utc
        .with_ymd_and_hms(2026, 9, 19, 8, 0, 0)
        .unwrap()
        .timestamp_millis();
    let rows = vec![
        pool_beat(t0, "acct-1", "working", None, Some(4), Some(2)),
        // The same member, busier 30 s later: the hour takes its peak.
        pool_beat(t0 + 30_000, "acct-1", "working", None, Some(4), Some(3)),
        pool_beat(t0 + 30_000, "acct-2", "working", None, Some(4), Some(1)),
        pool_beat(
            t0 + 30_000,
            "acct-3",
            "paused",
            Some("sign-in needed"),
            Some(4),
            Some(0),
        ),
        // A member that reports its state and nothing else counts as one.
        pool_beat(t0 + 30_000, "acct-4", "working", None, None, None),
    ];
    let history = build_history(&rows, t0, t0 + hour);
    assert_eq!(history.capacity[0].at, "2026-09-19T08:00:00Z");
    assert_eq!(history.capacity[0].busy, 5, "3 + 1 + 0 jobs, plus acct-4");
    // Every member is still its own lane, with its reported state.
    assert_eq!(history.slots.len(), 4);
    let paused = history
        .slots
        .iter()
        .find(|slot| slot.slot == "acct-3")
        .expect("acct-3 lane");
    assert_eq!(paused.segments[0].state, "paused");
}

/// Posting a pool beat and reading it back, beside a queue beat that says
/// nothing new: the older payload keeps working and reads as a `todoq` slot.
#[tokio::test]
async fn pool_heartbeat_fields_round_trip_and_a_queue_beat_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    let token = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new_with_git_storage(core.clone(), dir.path().to_path_buf())
            .with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let call = |method, uri: &str, body| router.clone().oneshot(request(method, uri, &token, body));
    let beat = |body| call(HttpMethod::POST, "/api/v1/shift/heartbeat", Some(body));

    // A queue slot, exactly as it beats today.
    let queue = beat(
        json!({"operator": "operator", "host": "node-a", "slot": "w1",
                            "family": "acme", "state": "working", "stage": "agent",
                            "todo_id": "20260919-010000-abcdef", "planned_slots": 2}),
    )
    .await
    .unwrap();
    assert_eq!(queue.status(), StatusCode::OK);

    // Two members of an external pool: one working, one waiting on a sign-in.
    for member in [
        json!({"operator": "operator", "host": "node-b", "slot": "acct-1", "family": "globex",
               "state": "working", "kind": "pool", "capacity": 4, "active": 3}),
        json!({"operator": "operator", "host": "node-b", "slot": "acct-2", "family": "globex",
               "state": "paused", "kind": "pool", "detail": "sign-in needed",
               "capacity": 4, "active": 0}),
    ] {
        assert_eq!(beat(member).await.unwrap().status(), StatusCode::OK);
    }

    // A kind nobody reports is a typo, and the refusal names the two kinds.
    let refused = beat(
        json!({"operator": "operator", "host": "node-b", "slot": "acct-3",
                              "family": "globex", "state": "idle", "kind": "fleet"}),
    )
    .await
    .unwrap();
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let message = body_json(refused).await["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        message.contains("todoq") && message.contains("pool"),
        "{message}"
    );
    // So is a negative load.
    let negative = beat(
        json!({"operator": "operator", "host": "node-b", "slot": "acct-3",
                               "family": "globex", "state": "idle", "kind": "pool",
                               "active": -1}),
    )
    .await
    .unwrap();
    assert_eq!(negative.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let workers = body_json(
        call(HttpMethod::GET, "/api/v1/shift/workers", None)
            .await
            .unwrap(),
    )
    .await;
    let rows = workers["workers"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{workers}");
    let row = |slot: &str| {
        rows.iter()
            .find(|row| row["slot"] == slot)
            .unwrap_or_else(|| panic!("no slot {slot} in {workers}"))
            .clone()
    };

    // The queue slot said nothing about the new fields and reads as before,
    // with the default kind.
    let w1 = row("w1");
    assert_eq!(w1["kind"], "todoq");
    assert_eq!(w1["detail"], json!(null));
    assert_eq!(w1["capacity"], json!(null));
    assert_eq!(w1["active"], json!(null));
    assert_eq!(w1["state"], "working");
    assert_eq!(w1["stage"], "agent");
    assert_eq!(w1["planned_slots"], 2);

    let busy = row("acct-1");
    assert_eq!(busy["kind"], "pool");
    assert_eq!(busy["family"], "globex");
    assert_eq!(busy["capacity"], 4);
    assert_eq!(busy["active"], 3);
    assert_eq!(busy["healthy"], true);

    let paused = row("acct-2");
    assert_eq!(paused["kind"], "pool");
    assert_eq!(paused["state"], "paused");
    assert_eq!(paused["detail"], "sign-in needed");
    assert_eq!(paused["active"], 0);

    // The history counts the pool's jobs: 3 held by acct-1, none by acct-2,
    // plus the one queue slot that is working.
    let history = body_json(
        call(
            HttpMethod::GET,
            "/api/v1/shift/workers/history?hours=1",
            None,
        )
        .await
        .unwrap(),
    )
    .await;
    let busiest = history["capacity"]
        .as_array()
        .unwrap()
        .iter()
        .map(|bucket| bucket["busy"].as_i64().unwrap())
        .max()
        .unwrap();
    assert_eq!(busiest, 4, "{history}");
}

/// Migration 0009 on a store an older binary left at version 8: the rows it
/// wrote become todoq slots with no pool fields, and the data survives.
#[test]
fn a_version_8_store_upgrades_and_keeps_its_rows_as_todoq_slots() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shift.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        migrate_through(&conn, 8).unwrap();
        conn.execute(
            "INSERT INTO shift_heartbeats
                 (received_ms, reporter, operator, host, slot, family, state)
             VALUES (?1, 'operator', 'operator', 'node-a', 'w1', 'globex', 'idle')",
            [Utc::now().timestamp_millis()],
        )
        .unwrap();
    }
    let store = HeartbeatStore::open(&path).unwrap();
    let rows = store.latest(0).unwrap();
    assert_eq!(rows.len(), 1);
    let beat = &rows[0].heartbeat;
    assert_eq!(beat.kind, WorkerKind::Todoq.as_str());
    assert_eq!(
        (beat.detail.as_deref(), beat.capacity, beat.active),
        (None, None, None)
    );
}

/// The migration's lock bound (0009_shift_pool_slots.meta.toml): while another
/// writer holds shift.sqlite, opening the store waits out the busy timeout and
/// fails instead of queueing forever, and succeeds once the lock is gone.
#[test]
fn a_held_write_lock_bounds_the_migration_wait() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shift.sqlite");
    let holder = rusqlite::Connection::open(&path).unwrap();
    migrate_through(&holder, 8).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let started = std::time::Instant::now();
    let refused = HeartbeatStore::open(&path)
        .err()
        .expect("open must fail while locked");
    let waited = started.elapsed();
    assert!(refused.contains("locked"), "{refused}");
    assert!(waited >= std::time::Duration::from_secs(4), "{waited:?}");
    assert!(waited < std::time::Duration::from_secs(30), "{waited:?}");

    holder.execute_batch("ROLLBACK").unwrap();
    assert!(HeartbeatStore::open(&path).is_ok());
}
