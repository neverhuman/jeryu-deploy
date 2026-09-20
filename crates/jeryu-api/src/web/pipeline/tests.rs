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
            "event_id",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "event_id": "has space"}),
        ),
        (
            "event_id",
            json!({"source": "todoq", "kind": "a.b", "summary": "x", "event_id": "x".repeat(65)}),
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
        .unwrap()
        .event;
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
    let a = store.insert("alice", &claimed, now).unwrap().event;
    let b = store.insert("gatebot", &gate, now).unwrap().event;
    let c = store.insert("alice", &todo_done, now).unwrap().event;
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
        .unwrap()
        .event;
    assert!(next.seq > c.seq, "AUTOINCREMENT never reuses a sequence");

    // A named event is stored once per reporter; a repeat returns the original.
    let mut named = event("todo.claimed", "named");
    named.event_id = Some("todoq:t1:claim:1".to_string());
    let first = reopened.insert("alice", &named, now).unwrap();
    assert!(!first.duplicate);
    named.summary = "a retry with different text".to_string();
    let again = reopened.insert("alice", &named, now + 5).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.event, first.event, "the stored event comes back");
    let other = reopened.insert("gatebot", &named, now).unwrap();
    assert!(!other.duplicate, "ids are scoped to the reporter");
    assert_eq!(reopened.latest_seq().unwrap(), other.event.seq);
}

pub(crate) async fn body_json(response: axum::response::Response) -> Value {
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

    // A producer that never saw the response retries: same seqs, nothing new.
    let named = json!({"events": [
        {"event_id": "todoq:t1:merged", "source": "todoq", "kind": "todo.merged", "summary": "t1 merged", "todo_id": "t2"},
    ]});
    let first_try = call(
        HttpMethod::POST,
        "/api/v1/events",
        &admin,
        Some(named.clone()),
    )
    .await
    .unwrap();
    assert_eq!(first_try.status(), StatusCode::CREATED);
    let first_try = body_json(first_try).await;
    assert_eq!(first_try["schema_version"], "jeryu.pipeline_events/v1");
    assert_eq!(first_try["duplicates"], 0);
    let retry = call(HttpMethod::POST, "/api/v1/events", &admin, Some(named))
        .await
        .unwrap();
    assert_eq!(retry.status(), StatusCode::OK, "an all-repeat POST is 200");
    let retry = body_json(retry).await;
    assert_eq!(retry["seqs"], first_try["seqs"]);
    assert_eq!(retry["duplicates"], 1);

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
    assert_eq!(page["schema_version"], "jeryu.pipeline_events/v1");
    let events = page["events"].as_array().unwrap();
    assert_eq!(
        events.len(),
        4,
        "the refused batch and the retry stored nothing"
    );
    assert_eq!(page["latest_seq"], events[0]["seq"]);
    assert_eq!(events[0]["event_id"], "todoq:t1:merged");
    let events = &events[1..];
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

    // A route this server does not have is a JSON 404, not the web app's
    // HTML shell with a 200 that reads as success.
    for (method, uri) in [
        (HttpMethod::GET, "/api/v1/notifications"),
        (HttpMethod::GET, "/api/v1/events/12"),
        (HttpMethod::POST, "/api/v1/nope"),
        // Every API version, not only v1.
        (HttpMethod::GET, "/api/v3/nope"),
        (HttpMethod::GET, "/api/nope"),
    ] {
        let missing = call(method, uri, &admin, None).await.unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(body_json(missing).await["code"], "api_route_not_found");
    }

    // Every refusal is a typed JSON error an agent can act on, never HTML.
    let bad_query = call(
        HttpMethod::GET,
        "/api/v1/events?after_seq=soon",
        &admin,
        None,
    )
    .await
    .unwrap();
    assert_eq!(bad_query.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bad_query = body_json(bad_query).await;
    assert_eq!(bad_query["code"], "events_invalid_query");
    assert!(
        bad_query["repair_hint"]
            .as_str()
            .unwrap()
            .contains("after_seq")
    );

    // A kind filter that can only match nothing is a mistake, and says so; a
    // real kind and a prefix ending in a dot are both filters.
    for (uri, status) in [
        (
            "/api/v1/events?kind=NOT%20A%20KIND",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("/api/v1/events?kind=todo", StatusCode::UNPROCESSABLE_ENTITY),
        ("/api/v1/events?kind=.", StatusCode::UNPROCESSABLE_ENTITY),
        ("/api/v1/events?kind=todo.", StatusCode::OK),
        ("/api/v1/events?kind=todo.claimed", StatusCode::OK),
        ("/api/v1/events?kind=", StatusCode::OK),
    ] {
        let response = call(HttpMethod::GET, uri, &admin, None).await.unwrap();
        assert_eq!(response.status(), status, "{uri}");
        if status != StatusCode::OK {
            assert_eq!(body_json(response).await["code"], "events_invalid_query");
        }
    }
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
    let stored = super::record(&state, "alice", claimed).unwrap().event;
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

/// An admin token and a router over the shift fixture (queue repo with two
/// todos, `jeryu-deploy` with a nightshift branch).
pub(crate) fn shift_forge(dir: &Path) -> (axum::Router, String) {
    crate::web::shift::tests::fixture(dir);
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_repository(
        "jeryu",
        jeryu_core::CreateRepositoryRequest {
            name: "jeryu-deploy".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let admin = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new_with_git_storage(core, dir.to_path_buf()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    (router, admin)
}

async fn events_of(router: &axum::Router, token: &str, filter: &str) -> Vec<Value> {
    let page = body_json(
        router
            .clone()
            .oneshot(request(
                HttpMethod::GET,
                &format!("/api/v1/events?after_seq=0&{filter}"),
                token,
                None,
            ))
            .await
            .unwrap(),
    )
    .await;
    page["events"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn shift_writes_and_stage_changes_become_events() {
    let dir = tempfile::tempdir().unwrap();
    let (router, admin) = shift_forge(dir.path());
    let post = |uri: &str, body: Value| {
        router
            .clone()
            .oneshot(request(HttpMethod::POST, uri, &admin, Some(body)))
    };

    let filed = post(
        "/api/v1/shift/todos",
        json!({"family": "jeryu", "text": "Fix the header", "mode": "now"}),
    )
    .await
    .unwrap();
    assert_eq!(filed.status(), StatusCode::CREATED);
    let id = body_json(filed).await["id"].as_str().unwrap().to_string();
    let blocked = post(
        &format!("/api/v1/shift/todos/jeryu/{id}/action"),
        json!({"action": "block", "note": "needs a decision"}),
    )
    .await
    .unwrap();
    assert_eq!(blocked.status(), StatusCode::OK);

    let beat = |state: &str, stage: Option<&str>, todo: Option<&str>| {
        json!({"operator": "alton@xbabe0", "host": "xbabe0", "slot": "w1", "family": "jeryu",
               "state": state, "stage": stage, "todo_id": todo})
    };
    for body in [
        beat("idle", None, None),                  // first idle beat: no event
        beat("idle", None, None),                  // unchanged: no event
        beat("working", Some("agent"), Some(&id)), // idle -> agent
        beat("working", Some("agent"), Some(&id)), // unchanged: no event
        beat("working", Some("gate"), Some(&id)),  // agent -> gate
        beat("idle", None, None),                  // gate -> idle
    ] {
        let response = post("/api/v1/shift/heartbeat", body).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let opened = post(
        "/api/v1/shift/shifts/jeryu/pr",
        json!({"branch": "nightshift/2026-09-18"}),
    )
    .await
    .unwrap();
    assert_eq!(opened.status(), StatusCode::OK);
    // Asking again finds the open PR and opens nothing, so emits nothing.
    post(
        "/api/v1/shift/shifts/jeryu/pr",
        json!({"branch": "nightshift/2026-09-18"}),
    )
    .await
    .unwrap();

    let events = events_of(&router, &admin, "family=jeryu").await;
    let lines: Vec<(&str, &str)> = events
        .iter()
        .map(|e| (e["kind"].as_str().unwrap(), e["summary"].as_str().unwrap()))
        .collect();
    assert_eq!(
        lines,
        [
            ("todo.filed", "filed: Fix the header"),
            ("todo.action", "block by alice: Fix the header"),
            ("worker.stage", &*format!("w1 jeryu: idle -> agent on {id}")),
            ("worker.stage", &*format!("w1 jeryu: agent -> gate on {id}")),
            ("worker.stage", &*format!("w1 jeryu: gate -> idle on {id}")),
            (
                "shift.pr_opened",
                "opened jeryu-deploy#1 for shift nightshift/2026-09-18"
            ),
        ]
    );
    assert_eq!(events[0]["todo_id"], id.as_str());
    assert_eq!(events[0]["reporter"], "forge");
    assert_eq!(events[0]["actor"], "alice/web");
    assert_eq!(events[1]["reason"], "needs a decision");
    assert_eq!(events[2]["actor"], "alton@xbabe0/w1");
    assert_eq!(events[5]["repo"], "jeryu/jeryu-deploy");
    assert_eq!(events[5]["pr"], 1);
    assert_eq!(events[5]["shift"], "nightshift/2026-09-18");
}

fn runner_beat(labels: &[&str], current: Option<Value>, last: Option<Value>) -> Value {
    let reviewer = labels.contains(&"redteam");
    json!({
        "runnerId": if reviewer { "xbabe0/pr-redteam" } else { "xbabe2/slot0" },
        "host": if reviewer { "xbabe0" } else { "xbabe2" },
        "slot": 0,
        "labels": labels,
        "current": current,
        "last": last,
    })
}

#[tokio::test]
async fn runner_heartbeats_emit_gate_and_review_events_only_on_change() {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("gatebot", "gatebot-password", UserRole::User)
        .unwrap();
    core.create_account("pragent", "pragent-password", UserRole::User)
        .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, gatebot, pragent) = (token("alice"), token("gatebot"), token("pragent"));
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let sha = "b761244b76371995527bfe7795e98492703553a8";
    let task = json!({"repo": "jeryu/jeryu-web", "pr": 35, "sha": sha,
                      "recipe": "ops/ci/pr-ci.sh", "startedAt": "2026-09-19T13:06:00Z"});
    let result = |conclusion: &str| {
        json!({"repo": "jeryu/jeryu-web", "pr": 35, "sha": sha, "recipe": "ops/ci/pr-ci.sh",
               "conclusion": conclusion, "seconds": 114, "finishedAt": "2026-09-19T13:08:14Z"})
    };
    let old = json!({"repo": "jeryu/jeryu-web", "pr": 34, "sha": sha, "recipe": "ops/ci/pr-ci.sh",
                     "conclusion": "success", "seconds": 90, "finishedAt": "2026-09-19T12:00:00Z"});
    for (who, body) in [
        // First beat after a forge restart repeats an old result: not news.
        (&gatebot, runner_beat(&["pr-gate"], None, Some(old.clone()))),
        (
            &gatebot,
            runner_beat(&["pr-gate"], Some(task.clone()), Some(old.clone())),
        ),
        (
            &gatebot,
            runner_beat(&["pr-gate"], Some(task.clone()), Some(old.clone())),
        ),
        (
            &gatebot,
            runner_beat(&["pr-gate"], None, Some(result("failure"))),
        ),
        (
            &gatebot,
            runner_beat(&["pr-gate"], None, Some(result("failure"))),
        ),
        (&pragent, runner_beat(&["redteam"], None, None)),
        (
            &pragent,
            runner_beat(&["redteam"], Some(task.clone()), None),
        ),
        (
            &pragent,
            runner_beat(&["redteam"], None, Some(result("too_large"))),
        ),
    ] {
        let response = router
            .clone()
            .oneshot(request(
                HttpMethod::POST,
                "/api/v1/runners/heartbeat",
                who,
                Some(body),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let events = events_of(&router, &admin, "repo=jeryu/jeryu-web").await;
    let lines: Vec<(&str, &str, bool)> = events
        .iter()
        .map(|e| {
            (
                e["kind"].as_str().unwrap(),
                e["outcome"].as_str().unwrap_or("-"),
                e["needs_human"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        lines,
        [
            ("gate.started", "-", false),
            ("gate.finished", "failure", false),
            ("review.started", "-", false),
            ("review.finished", "too_large", true),
        ]
    );
    assert_eq!(events[1]["seconds"], 114);
    assert_eq!(events[1]["pr"], 35);
    assert_eq!(events[1]["actor"], "xbabe2/slot0");
    assert_eq!(
        events[3]["summary"],
        "xbabe0/pr-redteam review of jeryu/jeryu-web#35: too_large in 114s"
    );
}

#[tokio::test]
async fn automation_heartbeats_emit_no_events_and_a_gate_without_a_pr_still_does() {
    let core = ForgeCore::new();
    core.create_account("alton2", "alton2-password", UserRole::Admin)
        .unwrap();
    core.create_account("gatebot", "gatebot-password", UserRole::User)
        .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, gatebot) = (token("alton2"), token("gatebot"));
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let sha = "77dc3310aa5eadc15694dd1434d9f8f99c44a0d3";
    let timer = |last: Value| {
        json!({"runnerId": "xbabe0/auto-pin", "host": "xbabe0", "slot": 0,
               "labels": ["automation"], "intervalSeconds": 300, "last": last})
    };
    let did = |conclusion: &str, pr: Option<u64>| {
        json!({"repo": "jeryu/jeryu-deploy", "pr": pr, "sha": sha, "recipe": "auto-pin",
               "conclusion": conclusion, "seconds": 0, "finishedAt": "2026-09-20T04:10:00Z"})
    };
    let gate_task = json!({"repo": "jeryu/jeryu-deploy", "sha": sha,
                           "recipe": "ops/ci/pr-ci.sh", "startedAt": "2026-09-20T04:11:00Z"});
    for (who, body) in [
        // Every transition a gate or reviewer would announce: none is news here.
        (&admin, timer(Value::Null)),
        (&admin, timer(did("waiting", Some(71)))),
        (&admin, timer(did("opened", Some(74)))),
        (&admin, timer(did("failed", None))),
        (&gatebot, runner_beat(&["pr-gate"], Some(gate_task), None)),
    ] {
        let response = router
            .clone()
            .oneshot(request(
                HttpMethod::POST,
                "/api/v1/runners/heartbeat",
                who,
                Some(body),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let events = events_of(&router, &admin, "repo=jeryu/jeryu-deploy").await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["kind"], "gate.started");
    assert_eq!(events[0]["actor"], "xbabe2/slot0");
    assert!(events[0]["pr"].is_null());
    assert_eq!(
        events[0]["summary"],
        "xbabe2/slot0 gating jeryu/jeryu-deploy@77dc331"
    );
}
