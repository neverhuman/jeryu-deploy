//! Tests for `GET /api/v1/events/histogram`.

use std::path::Path;

use axum::http::{Method as HttpMethod, StatusCode};
use chrono::Utc;
use jeryu_core::{ForgeCore, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::tests::{body_json, request};
use crate::web::{WebState, app};

/// An admin, an ordinary account and a router over its own event store.
fn forge() -> (axum::Router, String, String) {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, user) = (token("alice"), token("bob"));
    let state = WebState::new(core).with_auth(true, false, false);
    (app(state, Path::new("/tmp/jeryu-no-spa")), admin, user)
}

async fn get(router: &axum::Router, uri: &str, token: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(request(HttpMethod::GET, uri, token, None))
        .await
        .unwrap();
    (response.status(), body_json(response).await)
}

async fn post(router: &axum::Router, token: &str, events: Value) {
    let response = router
        .clone()
        .oneshot(request(
            HttpMethod::POST,
            "/api/v1/events",
            token,
            Some(json!({"events": events})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn an_empty_window_is_zero_filled_and_has_no_gaps() {
    let (router, admin, user) = forge();

    let (status, page) = get(&router, "/api/v1/events/histogram", &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["schema_version"], "jeryu.pipeline_events/v1");
    assert_eq!(page["bucket_seconds"], 3600, "24h draws hourly bars");
    let buckets = page["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 25, "24 whole hours plus the current one");
    assert!(
        buckets.iter().all(|bucket| bucket["total"] == 0
            && bucket["failures"] == 0
            && bucket["needs_human"] == 0
            && bucket["cost_usd"] == 0.0
            && bucket["seconds_p50"].is_null()
            && bucket["by_group"]["todos"] == 0),
        "every bucket is a zero, not a gap: {buckets:?}"
    );
    // The bars are whole buckets apart, in order, from `from` to `to`.
    let starts: Vec<i64> = buckets
        .iter()
        .map(|bucket| {
            chrono::DateTime::parse_from_rfc3339(bucket["start"].as_str().unwrap())
                .unwrap()
                .timestamp()
        })
        .collect();
    assert!(starts.windows(2).all(|pair| pair[1] - pair[0] == 3600));
    let from = chrono::DateTime::parse_from_rfc3339(page["from"].as_str().unwrap()).unwrap();
    let to = chrono::DateTime::parse_from_rfc3339(page["to"].as_str().unwrap()).unwrap();
    assert_eq!(starts[0], from.timestamp());
    assert_eq!(starts[24] + 3600, to.timestamp(), "to is the exclusive end");
    assert_eq!(from.timestamp() % 3600, 0, "bars start on whole hours");
    assert_eq!(
        page["totals"],
        json!({"todos_finished": 0, "blocked": 0, "prs_merged": 0, "deploys": 0, "spent_usd": 0.0})
    );

    // Reading it is admin-only, like reading the log it counts.
    let (status, denied) = get(&router, "/api/v1/events/histogram", &user).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["code"], "permission_denied");
}

#[tokio::test]
async fn events_land_in_their_bucket_grouped_by_chip_with_totals() {
    let (router, admin, _) = forge();
    post(
        &router,
        &admin,
        json!([
            {"source": "todoq", "kind": "todo.claimed", "summary": "claimed", "todo_id": "t1"},
            {"source": "todoq", "kind": "todo.attempt_finished", "summary": "done", "todo_id": "t1",
             "outcome": "done", "cost_usd": 0.25, "seconds": 10},
            {"source": "todoq", "kind": "todo.attempt_finished", "summary": "blocked", "todo_id": "t2",
             "outcome": "blocked", "cost_usd": 0.5, "seconds": 30, "needs_human": true},
            {"source": "pr-gate", "kind": "gate.log", "summary": "gate", "repo": "jeryu/jeryu-web",
             "sha": "b761244", "outcome": "failure", "seconds": 20},
            {"source": "reviewer", "kind": "pr.review", "summary": "verdict", "repo": "jeryu/jeryu-web",
             "pr": 7, "sha": "b761244", "outcome": "approve"},
            {"source": "forge", "kind": "pr.merged", "summary": "merged", "repo": "jeryu/jeryu-web", "pr": 7},
            {"source": "forge", "kind": "queue.landed", "summary": "landed", "repo": "jeryu/jeryu-web", "pr": 7},
            {"source": "auto-stage", "kind": "release.staged", "summary": "staged", "repo": "jeryu/jeryu-web"},
            {"source": "forge", "kind": "deploy.status", "summary": "deployed", "repo": "jeryu/jeryu-web",
             "outcome": "success"},
            {"source": "todoq", "kind": "worker.error", "summary": "worker", "family": "jeryu"},
        ]),
    )
    .await;

    let (status, page) = get(
        &router,
        "/api/v1/events/histogram?hours=1&bucket=1h",
        &admin,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let buckets = page["buckets"].as_array().unwrap();
    // Everything was posted now, so it is all in the last bucket.
    let now = buckets.last().unwrap();
    assert_eq!(now["total"], 10);
    assert_eq!(
        now["by_group"],
        json!({"todos": 3, "gates": 1, "reviews": 1, "pulls": 1, "merges": 1,
               "releases": 1, "deploys": 1, "other": 1}),
        "pr.review is a review, pr.merged a pull request"
    );
    assert_eq!(now["failures"], 2, "a failed gate and a blocked attempt");
    assert_eq!(now["needs_human"], 1);
    assert_eq!(now["cost_usd"], 0.75);
    assert_eq!(now["seconds_p50"], 20, "10, 20, 30");
    assert_eq!(buckets[0]["total"], 0, "the earlier bucket stays a zero");
    assert_eq!(
        page["totals"],
        json!({"todos_finished": 1, "blocked": 1, "prs_merged": 1, "deploys": 1, "spent_usd": 0.75})
    );

    // Filters narrow the same window; the series keeps its shape.
    for (filter, total) in [
        ("kind=todo.", 3),
        ("kind=todo.claimed", 1),
        ("repo=jeryu/jeryu-web", 6),
        ("source=todoq", 4),
        ("needs_human=true", 1),
        ("family=jeryu", 1),
    ] {
        let uri = format!("/api/v1/events/histogram?hours=1&bucket=1h&{filter}");
        let (status, page) = get(&router, &uri, &admin).await;
        assert_eq!(status, StatusCode::OK, "{filter}");
        let buckets = page["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 2, "{filter}");
        assert_eq!(buckets.last().unwrap()["total"], total, "{filter}");
    }
}

/// The forge and the runner both report a finished gate, and the reviewer's
/// beat echoes its own verdict. A chart that counted both would draw twice the
/// volume the feed shows, so the histogram folds them exactly as the Activity
/// page does.
#[tokio::test]
async fn one_happening_reported_twice_is_counted_once() {
    let (router, admin, _) = forge();
    let sha = "b761244b76371995527bfe7795e98492703553a8";
    post(
        &router,
        &admin,
        json!([
            {"source": "forge", "kind": "gate.finished", "summary": "gate finished",
             "repo": "jeryu/jeryu-web", "pr": 7, "sha": sha, "outcome": "failure"},
            {"source": "pr-gate", "kind": "gate.log", "summary": "gate log",
             "repo": "jeryu/jeryu-web", "pr": 7, "sha": sha, "outcome": "failure"},
            // The heartbeat flattens timed_out to error; the log tells the truth.
            {"source": "forge", "kind": "gate.finished", "summary": "other gate finished",
             "repo": "jeryu/jeryu-core", "pr": 3, "sha": sha, "outcome": "error"},
            {"source": "pr-gate", "kind": "gate.log", "summary": "other gate log",
             "repo": "jeryu/jeryu-core", "pr": 3, "sha": sha, "outcome": "timed_out"},
            // A gate whose log never arrived is still one happening.
            {"source": "forge", "kind": "gate.finished", "summary": "lone gate",
             "repo": "jeryu/jeryu-jira", "pr": 1, "sha": sha, "outcome": "success"},
            {"source": "forge", "kind": "review.finished", "summary": "review finished",
             "repo": "jeryu/jeryu-web", "pr": 7, "sha": sha, "outcome": "approve"},
            {"source": "reviewer", "kind": "pr.review", "summary": "verdict",
             "repo": "jeryu/jeryu-web", "pr": 7, "sha": sha, "outcome": "approve"},
            {"source": "forge", "kind": "review.finished", "summary": "lone review",
             "repo": "jeryu/jeryu-jira", "pr": 1, "sha": sha, "outcome": "approve"},
        ]),
    )
    .await;
    let (status, page) = get(
        &router,
        "/api/v1/events/histogram?hours=1&bucket=1h",
        &admin,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let now = page["buckets"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        now["by_group"]["gates"], 3,
        "two folded echoes and one lone gate.finished"
    );
    assert_eq!(
        now["by_group"]["reviews"], 2,
        "the verdict and the review nobody echoed"
    );
    assert_eq!(now["total"], 5);
}

#[tokio::test]
async fn a_query_it_cannot_answer_is_refused_not_guessed() {
    let (router, admin, _) = forge();
    for (uri, reason) in [
        ("/api/v1/events/histogram?hours=0", "hours"),
        ("/api/v1/events/histogram?hours=721", "hours"),
        (
            "/api/v1/events/histogram?hours=abc",
            "hours is not a number",
        ),
        (
            "/api/v1/events/histogram?bucket=30m",
            "bucket is a closed set",
        ),
        (
            "/api/v1/events/histogram?kind=todo",
            "kind is a kind or a prefix",
        ),
        (
            "/api/v1/events/histogram?after_seq=1",
            "a key it does not read",
        ),
        (
            "/api/v1/events/histogram?hours=720&bucket=5m",
            "too many buckets",
        ),
    ] {
        let (status, refusal) = get(&router, uri, &admin).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reason}: {uri}");
        assert_eq!(refusal["code"], "events_invalid_query", "{reason}");
        assert!(
            refusal["message"].as_str().unwrap_or_default().len() > 10,
            "{reason}: the refusal says what to send instead"
        );
    }
    let (status, refusal) = get(&router, "/api/v1/events/histogram?family=nope", &admin).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refusal["code"], "family_unknown");
    // The suggestion a too-fine bucket is refused with is one that answers.
    let (status, page) = get(
        &router,
        "/api/v1/events/histogram?hours=720&bucket=1h",
        &admin,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["buckets"].as_array().unwrap().len(), 721);
}

/// Bucket boundaries come from the store, not from when the test runs: events
/// are inserted at chosen times and the window is asked for straight.
#[test]
fn an_event_counts_in_the_bucket_its_timestamp_falls_in() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shift.sqlite");
    let store = super::store::EventStore::open(&path).unwrap();
    let event = |kind: &str| super::types::NewEvent {
        source: "todoq".to_string(),
        kind: kind.to_string(),
        summary: "x".to_string(),
        ..super::types::NewEvent::default()
    };
    let now = Utc::now().timestamp_millis();
    let bucket_ms = 900_000;
    let (from_ms, to_ms) = super::histogram::window(now, 3, bucket_ms);
    // The first millisecond of a bucket, its last, and one millisecond before
    // the window opens.
    store
        .insert("alice", &event("todo.claimed"), from_ms)
        .unwrap();
    store
        .insert("alice", &event("todo.claimed"), from_ms + bucket_ms - 1)
        .unwrap();
    store
        .insert("alice", &event("todo.claimed"), from_ms + bucket_ms)
        .unwrap();
    store
        .insert("alice", &event("todo.claimed"), from_ms - 1)
        .unwrap();
    let window = super::store::HistogramWindow {
        from_ms,
        to_ms,
        bucket_ms,
        filter: &super::types::EventsQuery::default(),
    };
    let rows = store.histogram(&window).unwrap();
    let counted: Vec<(i64, i64)> = rows.iter().map(|row| (row.bucket, row.total)).collect();
    assert_eq!(
        counted,
        [(0, 2), (1, 1)],
        "a bucket holds its start and ends one millisecond before the next"
    );
    assert!(rows.iter().all(|row| row.group == "todos"));
}
