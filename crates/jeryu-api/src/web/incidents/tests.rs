//! Incidents: real event sequences through `EventStore::insert`, then the
//! report. Data is invented (family acme, service checkout).

use rusqlite::params;
use serde_json::{Value, json};

use super::{
    AlertAdapter, CAUSED_BY, CLOSED, GenericAlert, OPENED, RESTORED, ReportedIncident, check,
    report,
};
use crate::web::pipeline::{EventStore, NewEvent};

const T0: i64 = 1_760_000_000_000;
const MIN: i64 = 60_000;

fn store() -> (tempfile::TempDir, EventStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = EventStore::open(&dir.path().join("shift.sqlite")).unwrap();
    (dir, store)
}

fn at(minutes: i64) -> String {
    super::rfc3339(T0 + minutes * MIN)
}

fn post(store: &EventStore, kind: &str, detail: Value, minutes: i64) {
    let event = NewEvent {
        source: "alerts".to_string(),
        kind: kind.to_string(),
        summary: format!("{kind} checkout"),
        family: Some("acme".to_string()),
        detail: Some(detail),
        ..NewEvent::default()
    };
    store
        .insert("alert-bot", &event, T0 + minutes * MIN)
        .unwrap();
}

fn deploy(store: &EventStore, lot: &str, left_minutes: i64) {
    store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO flow_ledger (lot_id, family, station, entered_ms, left_ms, direction,
                   source_seq_first, source_seq_last)
                 VALUES (?1, 'acme', 'deploy', ?2, ?3, 'forward', 1, 1)",
                params![lot, T0, T0 + left_minutes * MIN],
            )
            .map_err(|e| e.to_string())
        })
        .unwrap();
}

fn read(store: &EventStore) -> Value {
    store
        .with_conn(|conn| report(conn, Some("acme"), None))
        .unwrap()
}

fn incident<'a>(body: &'a Value, id: &str) -> &'a Value {
    body["incidents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["incident_id"] == id)
        .unwrap()
}

#[test]
fn a_known_cause_gives_time_to_detect_and_restore_and_fails_the_change() {
    let (_dir, store) = store();
    deploy(&store, "acme/widget#7", 0);
    post(
        &store,
        OPENED,
        json!({"incident_id": "inc-1", "service": "checkout",
        "severity": "sev2", "detected_at": at(30)}),
        30,
    );
    post(
        &store,
        CAUSED_BY,
        json!({"incident_id": "inc-1", "changes": ["acme/widget#7"]}),
        35,
    );
    post(
        &store,
        RESTORED,
        json!({"incident_id": "inc-1", "restored_at": at(50),
        "restoring_change": "acme/widget#8"}),
        50,
    );
    let body = read(&store);
    let row = incident(&body, "inc-1");
    assert_eq!(row["time_to_restore_seconds"], 20 * 60);
    assert_eq!(row["causes"][0]["time_to_detect_seconds"], 30 * 60);
    assert_eq!(row["causes"][0]["outcome"], "failed");
    assert_eq!(row["restoring_change"], "acme/widget#8");
    assert_eq!(row["status"], "restored");
    assert_eq!(row["cause_unknown"], false);
    assert_eq!(body["failed_changes"][0]["change"], "acme/widget#7");
}

#[test]
fn restore_before_the_cause_is_known_counts_and_shows_cause_unknown() {
    let (_dir, store) = store();
    post(
        &store,
        OPENED,
        json!({"incident_id": "inc-2", "detected_at": at(0)}),
        0,
    );
    post(
        &store,
        RESTORED,
        json!({"incident_id": "inc-2", "restored_at": at(12)}),
        12,
    );
    let body = read(&store);
    let row = incident(&body, "inc-2");
    assert_eq!(row["time_to_restore_seconds"], 12 * 60);
    assert_eq!(row["cause_unknown"], true);
    assert_eq!(row["cause"], "cause unknown");
    assert_eq!(body["summary"]["cause_unknown"], 1);
    assert_eq!(body["summary"]["mean_time_to_restore_seconds"], 12 * 60);
    assert!(body["failed_changes"].as_array().unwrap().is_empty());
}

#[test]
fn attribution_arriving_late_fills_in_the_closed_incident() {
    let (_dir, store) = store();
    deploy(&store, "acme/widget#9", 10);
    post(
        &store,
        OPENED,
        json!({"incident_id": "inc-3", "detected_at": at(40)}),
        40,
    );
    post(
        &store,
        RESTORED,
        json!({"incident_id": "inc-3", "restored_at": at(45)}),
        45,
    );
    post(&store, CLOSED, json!({"incident_id": "inc-3"}), 50);
    assert_eq!(incident(&read(&store), "inc-3")["cause_unknown"], true);
    post(
        &store,
        CAUSED_BY,
        json!({"incident_id": "inc-3", "change": "acme/widget#9"}),
        600,
    );
    let body = read(&store);
    let row = incident(&body, "inc-3");
    assert_eq!(row["status"], "closed");
    assert_eq!(row["cause_unknown"], false);
    assert_eq!(row["causes"][0]["time_to_detect_seconds"], 30 * 60);
    assert_eq!(row["time_to_restore_seconds"], 5 * 60);
}

#[test]
fn several_causes_each_fail_and_a_repeat_is_counted_once() {
    let (_dir, store) = store();
    deploy(&store, "acme/widget#1", 0);
    deploy(&store, "acme/gadget#2", 20);
    post(
        &store,
        OPENED,
        json!({"incident_id": "inc-4", "detected_at": at(60)}),
        60,
    );
    post(
        &store,
        CAUSED_BY,
        json!({"incident_id": "inc-4",
        "changes": ["acme/widget#1", "acme/gadget#2"]}),
        61,
    );
    post(
        &store,
        CAUSED_BY,
        json!({"incident_id": "inc-4", "changes": ["acme/widget#1"]}),
        62,
    );
    let body = read(&store);
    let causes = incident(&body, "inc-4")["causes"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(causes.len(), 2);
    assert_eq!(causes[0]["change"], "acme/gadget#2");
    assert_eq!(causes[0]["time_to_detect_seconds"], 40 * 60);
    assert_eq!(causes[1]["time_to_detect_seconds"], 60 * 60);
    assert_eq!(body["failed_changes"].as_array().unwrap().len(), 2);
    assert_eq!(body["summary"]["mean_time_to_detect_seconds"], 50 * 60);
}

#[test]
fn a_restore_posted_before_the_opening_lands_on_the_same_incident() {
    let (_dir, store) = store();
    post(
        &store,
        RESTORED,
        json!({"incident_id": "inc-5", "restored_at": at(20)}),
        20,
    );
    post(
        &store,
        OPENED,
        json!({"incident_id": "inc-5", "detected_at": at(5),
        "service": "checkout"}),
        21,
    );
    let body = read(&store);
    assert_eq!(body["incidents"].as_array().unwrap().len(), 1);
    assert_eq!(incident(&body, "inc-5")["time_to_restore_seconds"], 15 * 60);
    assert_eq!(incident(&body, "inc-5")["service"], "checkout");
}

#[test]
fn incidents_outlive_the_event_log_retention() {
    let (_dir, store) = store();
    post(
        &store,
        OPENED,
        json!({"incident_id": "inc-6", "detected_at": at(0)}),
        0,
    );
    let far = T0 + 90 * 24 * 60 * MIN;
    let event = NewEvent {
        source: "alerts".to_string(),
        kind: "alert.noise".to_string(),
        summary: "later".to_string(),
        ..NewEvent::default()
    };
    store.insert("alert-bot", &event, far).unwrap();
    let kept = store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM pipeline_events WHERE kind = 'incident.opened'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| e.to_string())
        })
        .unwrap();
    assert_eq!(kept, 0, "the log pruned the event");
    assert_eq!(read(&store)["incidents"].as_array().unwrap().len(), 1);
}

#[test]
fn malformed_incident_events_are_refused() {
    assert!(check(OPENED, None).is_err());
    assert!(check(OPENED, Some(&json!({"incident_id": "a b"}))).is_err());
    assert!(
        check(
            OPENED,
            Some(&json!({"incident_id": "x", "detected_at": "yesterday"}))
        )
        .is_err()
    );
    assert!(check(CAUSED_BY, Some(&json!({"incident_id": "x"}))).is_err());
    assert!(
        check(
            CAUSED_BY,
            Some(&json!({"incident_id": "x", "changes": ["c#1"]}))
        )
        .is_ok()
    );
    assert!(check("todo.claimed", None).is_ok());
}

#[test]
fn the_generic_alert_adapter_maps_firing_and_resolved() {
    let firing = GenericAlert
        .events(
            &json!({"incident_id": "a-1", "status": "firing", "service": "checkout",
            "severity": "sev1", "summary": "5xx", "changes": ["acme/widget#3"]}),
        )
        .unwrap();
    let kinds: Vec<_> = firing.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, [OPENED, CAUSED_BY]);
    let resolved = GenericAlert
        .events(&json!({"incident_id": "a-1", "status": "resolved"}))
        .unwrap();
    assert_eq!(resolved[0].kind, RESTORED);
    assert!(GenericAlert.events(&json!({"status": "firing"})).is_err());
    assert!(
        GenericAlert
            .events(&json!({"incident_id": "a", "status": "weird"}))
            .is_err()
    );
}

#[test]
fn the_report_form_opens_an_incident_from_one_required_field() {
    let only_summary = ReportedIncident
        .events(&json!({"incident_id": "r-1", "summary": "checkout 5xx"}))
        .unwrap();
    let kinds: Vec<_> = only_summary.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, [OPENED]);
    assert_eq!(only_summary[0].source, "incident-form");
    assert_eq!(
        only_summary[0].event_id.as_deref(),
        Some("report-r-1-opened")
    );
    assert!(
        ReportedIncident
            .events(&json!({"incident_id": "r-1"}))
            .is_err(),
        "a report without a summary says nothing"
    );
}

#[test]
fn one_form_submission_may_open_attribute_restore_and_close() {
    let events = ReportedIncident
        .events(&json!({
            "incident_id": "r-2", "summary": "checkout 5xx", "service": "checkout",
            "severity": "sev2", "family": "acme", "detected_at": at(0),
            "changes": ["acme/widget#9"], "restored_at": at(30), "closed": true,
        }))
        .unwrap();
    let kinds: Vec<_> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, [OPENED, CAUSED_BY, RESTORED, CLOSED]);
    let (_dir, store) = store();
    deploy(&store, "acme/widget#9", 0);
    for event in &events {
        store.insert("reporter", event, T0 + 31 * MIN).unwrap();
    }
    let body = read(&store);
    let row = incident(&body, "r-2");
    assert_eq!(row["status"], "closed");
    assert_eq!(row["time_to_restore_seconds"], 1800);
    assert_eq!(row["cause_unknown"], false);
    assert_eq!(row["causes"][0]["change"], "acme/widget#9");
    assert_eq!(row["causes"][0]["outcome"], "failed");
}

#[test]
fn a_generated_incident_id_is_accepted_by_the_fold() {
    let id = super::new_incident_id();
    assert!(check(OPENED, Some(&json!({"incident_id": id}))).is_ok());
    assert_ne!(id, super::new_incident_id());
}

#[tokio::test]
async fn the_alert_webhook_takes_no_more_alerts_than_an_events_request() {
    use std::sync::Arc;

    use axum::Extension;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::StatusCode;
    use jeryu_core::{AccountStatus, AccountSummary, ForgeCore, UserRole};

    let state = Arc::new(crate::web::WebState::new(ForgeCore::new()));
    let admin = || {
        Extension(AccountSummary {
            login: "ops-admin".to_string(),
            display_name: "ops-admin".to_string(),
            role: UserRole::Admin,
            status: AccountStatus::Active,
            auth_epoch: 0,
            must_change_password: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
    };
    let alert = |n: usize| json!({"incident_id": format!("inc-{n}"), "status": "firing"});
    let cap = crate::web::pipeline::MAX_EVENT_BATCH;

    let too_many: Vec<Value> = (0..=cap).map(alert).collect();
    let refused = super::webhook(
        State(state.clone()),
        admin(),
        Bytes::from(serde_json::to_vec(&too_many).unwrap()),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        state
            .events
            .query(&crate::web::pipeline::EventsQuery::default())
            .unwrap()
            .is_empty(),
        "a refused batch records nothing"
    );

    let at_cap: Vec<Value> = (0..cap).map(alert).collect();
    let accepted = super::webhook(
        State(state.clone()),
        admin(),
        Bytes::from(serde_json::to_vec(&at_cap).unwrap()),
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::CREATED);
}
