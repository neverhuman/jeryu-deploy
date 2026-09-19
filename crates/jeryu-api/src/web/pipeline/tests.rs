//! Tests for the pipeline event log and its routes.

use std::path::Path;

use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use chrono::Utc;
use jeryu_core::{ForgeCore, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::store::{EventStore, RETENTION_MS};
use super::types::{EventsQuery, MAX_LOG_TAIL_BYTES, NewEvent, normalize};
use crate::web::{WebState, app};

fn event(kind: &str, summary: &str) -> NewEvent {
    NewEvent {
        source: "todoq".to_string(),
        kind: kind.to_string(),
        summary: summary.to_string(),
        ..NewEvent::default()
    }
}

#[test]
fn normalize_rejects_bad_identity_fields_and_clips_human_text() {
    for (field, bad) in [
        (
            "source",
            json!({"source": "Todoq", "kind": "a.b", "summary": "x"}),
        ),
        (
            "kind",
            json!({"source": "todoq", "kind": "claimed", "summary": "x"}),
        ),
        (
            "kind",
            json!({"source": "todoq", "kind": "todo.Claimed", "summary": "x"}),
        ),
        (
            "kind",
            json!({"source": "todoq", "kind": "todo..x", "summary": "x"}),
        ),
        (
            "summary",
            json!({"source": "todoq", "kind": "a.b", "summary": "  "}),
        ),
        (
            "repo",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "repo": "norepo"}),
        ),
        (
            "repo",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "repo": "a/b/c"}),
        ),
        (
            "sha",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "sha": "xyz1234"}),
        ),
        (
            "sha",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "sha": "abc"}),
        ),
        (
            "pr",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "pr": 0}),
        ),
        (
            "outcome",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "outcome": "Not Ok"}),
        ),
        (
            "cost_usd",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "cost_usd": -1.0}),
        ),
        (
            "seconds",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "seconds": -5}),
        ),
        (
            "detail",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "detail": [1]}),
        ),
        (
            "detail",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "detail": {"big": "x".repeat(9000)}}),
        ),
        (
            "log_url",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "log_url": "h".repeat(600)}),
        ),
    ] {
        let parsed: NewEvent = serde_json::from_value(bad.clone()).expect("shape parses");
        let err = normalize(parsed).expect_err(&format!("{bad} must be refused"));
        assert!(err.starts_with(field), "{err} should name {field}");
    }

    let long_log = format!("{}ü-tail", "x".repeat(MAX_LOG_TAIL_BYTES + 100));
    let ok = normalize(NewEvent {
        actor: Some(" alton@xbabe0/w1 ".to_string()),
        repo: Some("jeryu/jeryu-web".to_string()),
        sha: Some("ABCDEF1".to_string()),
        outcome: Some("timed_out".to_string()),
        summary: "s".repeat(400),
        reason: Some("r".repeat(1500)),
        log_tail: Some(long_log),
        family: Some("  ".to_string()),
        ..event("todo.attempt_finished", "")
    })
    .expect("valid event");
    assert_eq!(ok.actor.as_deref(), Some("alton@xbabe0/w1"));
    assert_eq!(ok.sha.as_deref(), Some("abcdef1"));
    assert_eq!(ok.family, None, "blank keys become null");
    assert_eq!(ok.summary.chars().count(), 300);
    assert!(ok.summary.ends_with('…'));
    assert_eq!(ok.reason.unwrap().chars().count(), 1000);
    let tail = ok.log_tail.unwrap();
    assert!(tail.len() <= MAX_LOG_TAIL_BYTES);
    assert!(tail.ends_with("ü-tail"), "the END of the log is kept");
}

#[test]
fn store_assigns_increasing_seq_filters_and_prunes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shift.sqlite");
    // The heartbeat store and the event store share one file and one runner.
    let _heartbeats = crate::web::shift::ShiftState::open(&path);
    let store = EventStore::open(&path).unwrap();
    let now = Utc::now().timestamp_millis();
    assert_eq!(store.latest_seq().unwrap(), 0);

    let old = store
        .insert(
            "alice",
            &event("todo.claimed", "old"),
            now - RETENTION_MS - 1000,
        )
        .unwrap();
    let mut claimed = event("todo.claimed", "claimed");
    claimed.family = Some("jeryu".to_string());
    claimed.todo_id = Some("t1".to_string());
    let mut gate = event("gate.finished", "gate");
    gate.repo = Some("jeryu/jeryu-web".to_string());
    gate.pr = Some(7);
    gate.needs_human = true;
    gate.detail = Some(json!({"exit_code": 1}));
    let mut todo_done = event("todo.attempt_finished", "done");
    todo_done.todo_id = Some("t1".to_string());
    // The first write after an hour prunes the row older than 30 days.
    let a = store.insert("alice", &claimed, now).unwrap();
    let b = store.insert("gatebot", &gate, now).unwrap();
    let c = store.insert("alice", &todo_done, now).unwrap();
    assert!(old.seq < a.seq && a.seq < b.seq && b.seq < c.seq);
    assert_eq!(store.latest_seq().unwrap(), c.seq);

    let newest = store.query(&EventsQuery::default()).unwrap();
    assert_eq!(
        newest.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [c.seq, b.seq, a.seq],
        "default page is newest first and the 31-day-old row is pruned"
    );
    assert_eq!(newest[1].reporter, "gatebot");
    assert_eq!(newest[1].detail, Some(json!({"exit_code": 1})));
    assert!(newest[1].needs_human);

    let tail = store
        .query(&EventsQuery {
            after_seq: Some(a.seq),
            ..EventsQuery::default()
        })
        .unwrap();
    assert_eq!(
        tail.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [b.seq, c.seq],
        "a cursor tail is oldest first"
    );
    let q = |query: EventsQuery| -> Vec<i64> {
        store.query(&query).unwrap().iter().map(|e| e.seq).collect()
    };
    assert_eq!(
        q(EventsQuery {
            kind: Some("todo.".into()),
            ..Default::default()
        }),
        [c.seq, a.seq]
    );
    assert_eq!(
        q(EventsQuery {
            kind: Some("todo.claimed".into()),
            ..Default::default()
        }),
        [a.seq]
    );
    assert_eq!(
        q(EventsQuery {
            kind: Some("%.".into()),
            ..Default::default()
        }),
        Vec::<i64>::new(),
        "a prefix is compared literally, never as a LIKE pattern"
    );
    assert_eq!(
        q(EventsQuery {
            todo_id: Some("t1".into()),
            family: Some("jeryu".into()),
            ..Default::default()
        }),
        [a.seq]
    );
    assert_eq!(
        q(EventsQuery {
            repo: Some("jeryu/jeryu-web".into()),
            pr: Some(7),
            needs_human: Some(true),
            source: Some("todoq".into()),
            ..Default::default()
        }),
        [b.seq]
    );
    assert_eq!(
        q(EventsQuery {
            before_seq: Some(c.seq),
            limit: Some(1),
            ..Default::default()
        }),
        [b.seq]
    );
    assert_eq!(
        store.newest_of_kind("todo.claimed").unwrap().unwrap().seq,
        a.seq
    );

    drop(store);
    let reopened = EventStore::open(&path).unwrap();
    let next = reopened
        .insert("alice", &event("a.b", "again"), now)
        .unwrap();
    assert!(next.seq > c.seq, "AUTOINCREMENT never reuses a sequence");
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

pub(crate) fn request(
    method: HttpMethod,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> Request<axum::body::Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(match body {
            Some(b) => axum::body::Body::from(b.to_string()),
            None => axum::body::Body::empty(),
        })
        .unwrap()
}

#[tokio::test]
async fn events_routes_enforce_reporter_and_admin_access() {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    // gatebot is a default JERYU_EVENT_REPORTERS identity without admin.
    core.create_account("gatebot", "gatebot-password", UserRole::User)
        .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, user, reporter) = (token("alice"), token("bob"), token("gatebot"));
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let call = |method, uri: &str, token: &str, body| {
        router.clone().oneshot(request(method, uri, token, body))
    };
    let gate_log = json!({
        "source": "pr-gate", "kind": "gate.log", "repo": "jeryu/jeryu-web", "pr": 35,
        "sha": "b761244b76371995527bfe7795e98492703553a8", "outcome": "failure",
        "summary": "gate failed on jeryu/jeryu-web#35", "needs_human": true,
        "log_tail": "error: test failed", "seconds": 114,
        "reporter": "alice", "seq": 999
    });

    // No login, an ordinary account, a reporter, an admin.
    let anonymous = router
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/events")
                .body(axum::body::Body::from(gate_log.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let denied = call(
        HttpMethod::POST,
        "/api/v1/events",
        &user,
        Some(gate_log.clone()),
    )
    .await
    .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(denied).await["code"], "events_reporter_required");

    let posted = call(
        HttpMethod::POST,
        "/api/v1/events",
        &reporter,
        Some(gate_log),
    )
    .await
    .unwrap();
    assert_eq!(posted.status(), StatusCode::CREATED);
    let posted = body_json(posted).await;
    assert_eq!(posted["ok"], true);
    let first = posted["seqs"][0].as_i64().unwrap();
    assert_ne!(first, 999, "a producer cannot choose its sequence");

    let batch = call(
        HttpMethod::POST,
        "/api/v1/events",
        &admin,
        Some(json!({"events": [
            {"source": "todoq", "kind": "todo.claimed", "summary": "w1 claimed t1", "todo_id": "t1", "family": "jeryu"},
            {"source": "todoq", "kind": "todo.attempt_finished", "summary": "t1 done", "todo_id": "t1", "outcome": "done", "cost_usd": 0.33},
        ]})),
    )
    .await
    .unwrap();
    assert_eq!(batch.status(), StatusCode::CREATED);
    assert_eq!(body_json(batch).await["seqs"].as_array().unwrap().len(), 2);

    // One bad event refuses the whole batch and names the entry.
    let bad = call(
        HttpMethod::POST,
        "/api/v1/events",
        &admin,
        Some(json!({"events": [
            {"source": "todoq", "kind": "todo.claimed", "summary": "fine"},
            {"source": "todoq", "kind": "nodot", "summary": "bad"},
        ]})),
    )
    .await
    .unwrap();
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bad = body_json(bad).await;
    assert_eq!(bad["code"], "events_invalid_request");
    assert!(
        bad["message"]
            .as_str()
            .unwrap()
            .starts_with("events[1]: kind")
    );
    let too_many: Vec<Value> = (0..51)
        .map(|i| json!({"source": "todoq", "kind": "a.b", "summary": format!("e{i}")}))
        .collect();
    let refused = call(
        HttpMethod::POST,
        "/api/v1/events",
        &admin,
        Some(json!({"events": too_many})),
    )
    .await
    .unwrap();
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Reads are admin-only: neither an ordinary account nor a reporter may
    // read titles and log tails of repositories it cannot see.
    for token in [&user, &reporter] {
        let read = call(HttpMethod::GET, "/api/v1/events", token, None)
            .await
            .unwrap();
        assert_eq!(read.status(), StatusCode::FORBIDDEN);
    }
    let page = body_json(
        call(HttpMethod::GET, "/api/v1/events", &admin, None)
            .await
            .unwrap(),
    )
    .await;
    let events = page["events"].as_array().unwrap();
    assert_eq!(events.len(), 3, "the refused batch stored nothing");
    assert_eq!(page["latest_seq"], events[0]["seq"]);
    assert_eq!(events[2]["seq"], first);
    assert_eq!(events[2]["reporter"], "gatebot", "reporter is the login");
    assert_eq!(events[2]["needs_human"], true);
    assert_eq!(events[2]["log_tail"], "error: test failed");
    assert_eq!(events[0]["cost_usd"], 0.33);
    assert_eq!(events[0]["reporter"], "alice");

    let tail = body_json(
        call(
            HttpMethod::GET,
            &format!("/api/v1/events?after_seq={first}&kind=todo.&todo_id=t1"),
            &admin,
            None,
        )
        .await
        .unwrap(),
    )
    .await;
    let kinds: Vec<&str> = tail["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["todo.claimed", "todo.attempt_finished"]);
}

#[test]
fn pipeline_scope_is_admin_only() {
    let core = ForgeCore::new();
    let admin = core
        .create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    let user = core
        .create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    let state = WebState::new(core);
    assert!(crate::web::ws::authorize_scope(
        &state,
        &admin,
        super::PIPELINE_SCOPE
    ));
    assert!(!crate::web::ws::authorize_scope(
        &state,
        &user,
        super::PIPELINE_SCOPE
    ));
}

#[test]
fn stored_events_reach_pipeline_scope_subscribers_only() {
    let state = WebState::new(ForgeCore::new());
    let (pipeline_tx, mut pipeline_rx) = tokio::sync::mpsc::unbounded_channel();
    let (other_tx, mut other_rx) = tokio::sync::mpsc::unbounded_channel();
    let subscribed = state.ws.register(pipeline_tx);
    let other = state.ws.register(other_tx);
    state
        .ws
        .set_scopes(subscribed, &[super::PIPELINE_SCOPE.to_string()].into());
    state
        .ws
        .set_scopes(other, &["global.activity".to_string()].into());

    let mut claimed = event("todo.claimed", "w1 claimed t1");
    claimed.todo_id = Some("t1".to_string());
    let stored = super::record(&state, "alice", claimed).unwrap();
    super::emit(&state, event("nodot", "dropped, never panics"));

    let frame = serde_json::to_value(pipeline_rx.try_recv().expect("one frame")).unwrap();
    assert_eq!(frame["event"]["scope"], "pipeline");
    assert_eq!(frame["event"]["kind"], "todo.claimed");
    assert_eq!(frame["event"]["entity"], "t1");
    assert_eq!(frame["event"]["payload"]["seq"], stored.seq);
    assert_eq!(frame["event"]["payload"]["reporter"], "alice");
    assert!(
        pipeline_rx.try_recv().is_err(),
        "the invalid event stored nothing"
    );
    assert!(other_rx.try_recv().is_err());
}
