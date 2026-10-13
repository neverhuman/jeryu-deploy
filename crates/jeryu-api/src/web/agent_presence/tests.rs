//! Unit tests for agent presence: the beat checks, the derived state, and
//! the store's expiry, linger and retention rules on an injected clock.

use super::store::{LINGER_MS, PresenceRow, PresenceStore, RETENTION_MS, effective_state};
use super::types::{BeatError, PresenceBeat, PresenceState};

const MINUTE: i64 = 60 * 1000;

#[test]
fn retained_rows_are_bounded_and_beats_have_a_write_budget() {
    let (_dir, store) = store();
    for n in 0..super::store::MAX_REPORTER_ROWS {
        store
            .upsert(&row(&format!("s{n}"), 300, PresenceState::Active), 0)
            .unwrap();
    }
    assert!(matches!(
        store.upsert(&row("overflow", 300, PresenceState::Active), 0),
        Err(super::store::StoreError::Limit)
    ));
    assert_eq!(store.count(), super::store::MAX_REPORTER_ROWS);
    for _ in 65..120 {
        store
            .upsert(&row("s0", 300, PresenceState::Active), 0)
            .unwrap();
    }
    assert!(matches!(
        store.upsert(&row("s0", 300, PresenceState::Active), 0),
        Err(super::store::StoreError::WriteBudget)
    ));
    store
        .upsert(&row("s0", 300, PresenceState::Active), MINUTE)
        .unwrap();
}

#[test]
fn competing_first_registrations_cannot_change_the_winning_owner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shift.sqlite");
    let first = PresenceStore::open(&path).unwrap();
    let second = PresenceStore::open(&path).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let jobs: Vec<_> = [(first.clone(), "ada"), (second, "grace")]
        .into_iter()
        .map(|(store, actor)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                (
                    actor,
                    store.upsert_as(&row("shared", 300, PresenceState::Active), actor, false, 0),
                )
            })
        })
        .collect();
    let results: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(_, result)| matches!(result, Err(super::store::StoreError::Owned)))
            .count(),
        1
    );
    let winner = results.iter().find(|(_, result)| result.is_ok()).unwrap().0;
    assert_eq!(first.get("shared").unwrap().unwrap().reporter, winner);
    assert!(
        first
            .end_as("shared", "outsider", false, 1)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        first.get("shared").unwrap().unwrap().state,
        PresenceState::Active
    );
}

#[test]
fn links_refuse_paths_and_scripts() {
    for pr in [
        "javascript:alert(1)",
        "//evil.invalid/a",
        "../../admin",
        "acme/widgets#0",
    ] {
        assert!(beat(with("links", serde_json::json!({"pr": pr}))).is_err());
    }
    for todo in ["javascript:alert(1)", "//evil.invalid/a", "../../admin"] {
        assert!(beat(with("links", serde_json::json!({"todo": todo}))).is_err());
    }
}

fn beat(value: serde_json::Value) -> Result<super::types::CheckedBeat, BeatError> {
    serde_json::from_value::<PresenceBeat>(value)
        .map_err(|err| BeatError::Invalid(err.to_string()))?
        .check()
}

fn base() -> serde_json::Value {
    serde_json::json!({
        "session_id": "cli:3f2a9c1e",
        "agent": "claude-code",
        "state": "active",
        "repos": ["acme/widgets"],
        "started_at": 1_791_800_000_000_i64,
        "last_active_at": 1_791_800_300_000_i64,
    })
}

fn with(field: &str, value: serde_json::Value) -> serde_json::Value {
    let mut body = base();
    body[field] = value;
    body
}

fn row(session_id: &str, ttl_seconds: i64, state: PresenceState) -> PresenceRow {
    PresenceRow {
        session_id: session_id.to_string(),
        reporter: "ada".to_string(),
        agent: "codex".to_string(),
        model: None,
        host: Some("node-a".to_string()),
        operator: Some("ada".to_string()),
        lane: None,
        branch: Some("ada/widgets".to_string()),
        title: None,
        state,
        repos: vec!["acme/widgets".to_string()],
        families: Vec::new(),
        started_ms: 0,
        last_active_ms: 0,
        ttl_seconds,
        received_ms: 0,
        expires_ms: 0,
        ended_ms: None,
        cost_usd: Some(0.5),
        tokens: None,
        links: None,
    }
}

fn store() -> (tempfile::TempDir, PresenceStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = PresenceStore::open(&dir.path().join("shift.sqlite")).unwrap();
    (dir, store)
}

#[test]
fn a_minimal_beat_passes_with_the_default_ttl() {
    let checked = beat(base()).unwrap();
    assert_eq!(checked.ttl_seconds, 300);
    assert_eq!(checked.state, PresenceState::Active);
    assert_eq!(checked.repos, ["acme/widgets"]);
}

#[test]
fn malformed_beats_are_refused_with_the_field_named() {
    for (body, needle) in [
        (with("session_id", "has space".into()), "session_id"),
        (with("session_id", "x".repeat(65).into()), "session_id"),
        (with("agent", " ".into()), "agent"),
        (with("agent", "a".repeat(33).into()), "agent"),
        (with("state", "stale".into()), "state"),
        (with("ttl_seconds", 29.into()), "ttl_seconds"),
        (with("ttl_seconds", 3601.into()), "ttl_seconds"),
        (with("lane", "work/lanes/presence".into()), "lane"),
        (with("title", "t".repeat(161).into()), "title"),
        (with("branch", "b".repeat(201).into()), "branch"),
        (with("cost_usd", (-1.0).into()), "cost_usd"),
        (with("started_at", (-5).into()), "started_at"),
        (with("surprise", true.into()), "surprise"),
        (
            with("links", serde_json::json!({"agent_run": "r1"})),
            "agent_run",
        ),
        (
            with("tokens", serde_json::json!({"input": 1, "extra": 2})),
            "extra",
        ),
    ] {
        match beat(body) {
            Err(BeatError::Invalid(reason)) => {
                assert!(reason.contains(needle), "{needle}: {reason}")
            }
            other => panic!("{needle}: expected a refusal, got {other:?}"),
        }
    }
}

#[test]
fn a_beat_without_repos_or_families_is_scope_empty() {
    let mut body = base();
    body["repos"] = serde_json::json!([" ", ""]);
    assert!(matches!(beat(body), Err(BeatError::ScopeEmpty)));
}

#[test]
fn too_many_repos_or_families_are_refused() {
    let repos: Vec<String> = (0..65).map(|n| format!("acme/r{n}")).collect();
    assert!(matches!(
        beat(with("repos", repos.into())),
        Err(BeatError::Invalid(_))
    ));
    let families: Vec<String> = (0..9).map(|n| format!("f{n}")).collect();
    assert!(matches!(
        beat(with("families", families.into())),
        Err(BeatError::Invalid(_))
    ));
}

#[test]
fn a_heartbeat_replaces_the_row_and_moves_the_expiry() {
    let (_dir, store) = store();
    let first = store
        .upsert(&row("s1", 300, PresenceState::Active), 1_000)
        .unwrap();
    assert_eq!(first.expires_ms, 1_000 + 300_000);
    let mut next = row("s1", 60, PresenceState::Idle);
    next.title = Some("second beat".to_string());
    let second = store.upsert(&next, 2_000).unwrap();
    assert_eq!(second.expires_ms, 2_000 + 60_000);
    assert_eq!(store.count(), 1);
    let stored = store.get("s1").unwrap().unwrap();
    assert_eq!(stored.state, PresenceState::Idle);
    assert_eq!(stored.title.as_deref(), Some("second beat"));
    assert_eq!(stored.host.as_deref(), Some("node-a"));
}

#[test]
fn past_its_ttl_a_session_reads_stale_and_ten_minutes_later_is_gone() {
    let (_dir, store) = store();
    let stored = store
        .upsert(&row("s1", 30, PresenceState::Active), 0)
        .unwrap();
    assert_eq!(effective_state(&stored, 30_000), "active");
    assert_eq!(effective_state(&stored, 30_001), "stale");
    assert_eq!(store.visible(30_000 + LINGER_MS).unwrap().len(), 1);
    assert!(store.visible(30_001 + LINGER_MS).unwrap().is_empty());
}

#[test]
fn an_ended_session_is_listed_for_ten_minutes_after_it_ended() {
    let (_dir, store) = store();
    store
        .upsert(&row("s1", 3600, PresenceState::Active), 0)
        .unwrap();
    let ended = store.end("s1", 5 * MINUTE).unwrap().unwrap();
    assert_eq!(ended.ended_ms, Some(5 * MINUTE));
    assert_eq!(effective_state(&ended, 5 * MINUTE), "ended");
    // Ending again keeps the first end time.
    let again = store.end("s1", 7 * MINUTE).unwrap().unwrap();
    assert_eq!(again.ended_ms, Some(5 * MINUTE));
    assert_eq!(store.visible(15 * MINUTE).unwrap().len(), 1);
    assert!(store.visible(15 * MINUTE + 1).unwrap().is_empty());
    assert!(store.end("missing", 0).unwrap().is_none());
}

#[test]
fn an_ended_beat_keeps_its_first_end_and_a_revived_session_clears_it() {
    let (_dir, store) = store();
    let first = store
        .upsert(&row("s1", 300, PresenceState::Ended), MINUTE)
        .unwrap();
    assert_eq!(first.ended_ms, Some(MINUTE));
    let second = store
        .upsert(&row("s1", 300, PresenceState::Ended), 2 * MINUTE)
        .unwrap();
    assert_eq!(second.ended_ms, Some(MINUTE));
    let revived = store
        .upsert(&row("s1", 300, PresenceState::Active), 3 * MINUTE)
        .unwrap();
    assert_eq!(revived.ended_ms, None);
    assert_eq!(effective_state(&revived, 3 * MINUTE), "active");
}

#[test]
fn rows_not_beaten_for_fourteen_days_are_pruned_on_write() {
    let (_dir, store) = store();
    store
        .upsert(&row("old", 300, PresenceState::Active), 0)
        .unwrap();
    store
        .upsert(&row("new", 300, PresenceState::Active), RETENTION_MS + 1)
        .unwrap();
    assert_eq!(store.count(), 1);
    assert!(store.get("old").unwrap().is_none());
}
