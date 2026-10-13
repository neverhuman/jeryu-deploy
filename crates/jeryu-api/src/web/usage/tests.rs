//! Tests for live token usage: the delta math, the sample store, the
//! `usage.changed` throttle and `GET /api/v1/usage`.

use std::path::Path;

use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::{CreateRepositoryRequest, ForgeCore, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::web::{WebState, app};

const MINUTE: i64 = 60_000;

fn usage(input: i64, output: i64, cache_read: i64, cache_write: i64, cost: f64) -> Usage {
    Usage {
        model: Some("claude-opus-5".to_string()),
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_write,
        cost_usd: Some(cost),
        turns: Some(1),
        updated_at: Some(rfc3339_ms(0)),
    }
}

fn sample(ts_ms: i64, slot: &str, todo: Option<&str>, usage: Usage) -> Sample {
    Sample {
        ts_ms,
        operator: "dana".to_string(),
        host: "build-1.example".to_string(),
        slot: slot.to_string(),
        family: "jeryu".to_string(),
        todo_id: todo.map(str::to_string),
        usage,
    }
}

/// Cumulative samples become per-bucket deltas: a new attempt's counters and
/// a producer that restarted mid-attempt both count the work once, never
/// twice and never as a negative.
#[test]
fn deltas_count_each_sample_once_across_resets() {
    let from = 10 * MINUTE;
    let to = from + 10 * MINUTE;
    // w1 was already working when the window opened: its baseline is what it
    // had spent by then, so only the rise lands in the buckets.
    let baselines = vec![sample(
        from - MINUTE,
        "w1",
        Some("t1"),
        usage(100, 10, 1_000, 0, 0.10),
    )];
    let samples = vec![
        // Same attempt, higher totals: the rise is the spend.
        sample(
            from + MINUTE,
            "w1",
            Some("t1"),
            usage(300, 40, 1_500, 200, 0.25),
        ),
        // The slot moved to another todo, so the counters reset to that
        // attempt's own totals. The drop is not negative spend.
        sample(
            from + 3 * MINUTE,
            "w1",
            Some("t2"),
            usage(50, 5, 0, 0, 0.02),
        ),
        // The producer restarted inside one attempt and reported smaller
        // totals than before: nothing is counted for that step.
        sample(
            from + 4 * MINUTE,
            "w1",
            Some("t2"),
            usage(10, 1, 0, 0, 0.01),
        ),
        sample(
            from + 5 * MINUTE,
            "w1",
            Some("t2"),
            usage(70, 9, 0, 0, 0.03),
        ),
        // A second slot, a different family key but the same shape.
        sample(
            from + 6 * MINUTE,
            "w2",
            Some("t3"),
            usage(20, 2, 0, 0, 0.01),
        ),
    ];
    let latest = vec![samples[3].clone(), samples[4].clone()];
    let answer = build(&samples, &baselines, &latest, from, to, 5 * MINUTE);

    assert_eq!(answer.bucket_seconds, 300);
    assert_eq!(answer.from, rfc3339_ms(from));
    assert_eq!(answer.to, rfc3339_ms(to));
    assert_eq!(
        answer.buckets.len(),
        3,
        "the window's own bucket and the two it reaches into, empty ones kept"
    );
    // The window's own bucket: w1's rise over its baseline (200 input, 30 output, 500
    // cache reads, 200 cache writes) plus t2's first totals (50 and 5).
    assert_eq!(answer.buckets[0].counts.input_tokens, 250);
    assert_eq!(answer.buckets[0].counts.output_tokens, 35);
    assert_eq!(answer.buckets[0].counts.cache_read_input_tokens, 500);
    assert_eq!(answer.buckets[0].counts.cache_creation_input_tokens, 200);
    assert!((answer.buckets[0].counts.cost_usd - 0.17).abs() < 1e-9);
    // The next bucket: the restart counts nothing, then t2 rises to 70/9 from
    // the 10/1 it restarted at, and w2 reports its first totals.
    assert_eq!(answer.buckets[1].counts.input_tokens, 60 + 20);
    assert_eq!(answer.buckets[1].counts.output_tokens, 8 + 2);
    assert_eq!(answer.totals.input_tokens, 330);
    assert_eq!(answer.totals.output_tokens, 45);
    assert_eq!(answer.totals.cache_read_input_tokens, 500);
    assert_eq!(answer.totals.cache_creation_input_tokens, 200);
    assert!(
        answer.totals.cost_usd > 0.0 && answer.totals.cost_usd < 1.0,
        "{:?}",
        answer.totals
    );
    let charted: i64 = answer
        .buckets
        .iter()
        .map(|bucket| bucket.counts.input_tokens)
        .sum();
    assert_eq!(
        charted, answer.totals.input_tokens,
        "every delta is charted"
    );

    // Live rows report the newest totals of each slot, and a rate over the
    // attempt's own samples.
    assert_eq!(answer.live.len(), 2);
    assert_eq!(answer.live[0].slot, "w1");
    assert_eq!(answer.live[0].todo_id.as_deref(), Some("t2"));
    assert_eq!(answer.live[0].usage.input_tokens, 70);
    // t2 flowed 50 + 5 then 60 + 8 fresh tokens over two minutes.
    assert_eq!(answer.live[0].tokens_per_minute, Some(61.5));
    assert_eq!(
        answer.live[1].tokens_per_minute, None,
        "one sample is no rate"
    );
}

/// A window with no samples is a chart of zeroes, not an empty answer, and a
/// slot nobody reported usage for has none.
#[test]
fn an_empty_window_is_still_a_chart() {
    let from = 0;
    let answer = build(&[], &[], &[], from, from + 10 * MINUTE, 5 * MINUTE);
    assert_eq!(answer.buckets.len(), 3);
    assert!(answer.buckets.iter().all(|b| b.counts
        == UsageCounts {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cost_usd: 0.0,
        }));
    assert_eq!(answer.totals, UsageCounts::default());
    assert!(answer.live.is_empty());
}

/// The store keeps a row per change, prunes with the event retention, and
/// lets at most one `usage.changed` frame out a second.
#[test]
fn the_store_writes_on_change_prunes_and_throttles_publishing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shift.sqlite");
    let store = UsageStore::open(&path).unwrap();
    let now = 40 * 24 * 60 * 60 * 1000;

    let old = sample(
        now - EVENT_RETENTION_MS - 1000,
        "w1",
        Some("t0"),
        usage(1, 1, 0, 0, 0.01),
    );
    assert!(store.insert(&old).unwrap());
    let first = sample(now, "w1", Some("t1"), usage(10, 2, 0, 0, 0.02));
    assert!(store.insert(&first).unwrap());
    assert!(
        !store.insert(&first).unwrap(),
        "the same totals for the same todo add no row"
    );
    let moved = sample(now + 1000, "w1", Some("t2"), usage(10, 2, 0, 0, 0.02));
    assert!(
        store.insert(&moved).unwrap(),
        "the same totals for another todo are another attempt"
    );
    assert_eq!(store.count(), 2, "the 31-day-old row is pruned on write");

    assert_eq!(
        store.latest_per_slot().unwrap(),
        vec![moved.clone()],
        "a slot's newest sample is the one it is working now"
    );
    assert_eq!(store.between(now, now + 1000).unwrap(), vec![first, moved]);
    assert_eq!(
        store.baselines(now + 1000).unwrap().len(),
        1,
        "only the t1 attempt was running before the window"
    );

    // At most one frame a second, server-wide, whichever slot changed.
    assert!(store.may_publish(now));
    assert!(!store.may_publish(now + 999));
    assert!(store.may_publish(now + 1000));

    drop(store);
    // Reopening re-checks the applied migration's checksum and keeps the rows.
    let reopened = UsageStore::open(&path).unwrap();
    assert_eq!(reopened.count(), 2);
}

#[test]
fn reported_totals_are_checked_and_tidied() {
    for bad in [
        json!({"input_tokens": -1}),
        json!({"output_tokens": -1}),
        json!({"cache_read_input_tokens": -1}),
        json!({"cache_creation_input_tokens": -1}),
        json!({"cost_usd": -0.5}),
        json!({"turns": -2}),
    ] {
        let parsed: Usage = serde_json::from_value(bad.clone()).expect("shape parses");
        let err = parsed
            .checked("usage")
            .expect_err(&format!("{bad} must be refused"));
        assert!(err.starts_with("usage."), "{err} should name the field");
    }
    let tidy = Usage {
        model: Some("  ".to_string()),
        updated_at: Some(String::new()),
        ..usage(1, 2, 3, 4, 0.5)
    }
    .checked("usage")
    .expect("valid usage");
    assert_eq!(
        tidy.model, None,
        "a blank model is nothing, not an empty name"
    );
    assert_eq!(tidy.updated_at, None);
}

#[test]
fn a_bucket_width_is_a_count_and_a_unit() {
    assert_eq!(parse_bucket("30s"), Ok(30));
    assert_eq!(parse_bucket(" 5m "), Ok(300));
    assert_eq!(parse_bucket("1h"), Ok(3600));
    for bad in ["5", "5x", "m", "", "-5m", "1s", "48h", "5 m", "５m"] {
        assert!(parse_bucket(bad).is_err(), "{bad:?} is not a width");
    }
}

#[test]
fn an_actor_names_the_slot_that_reported() {
    assert_eq!(
        actor_slot(Some("dana@build-1.example/w1")),
        ("dana".into(), "build-1.example".into(), "w1".into())
    );
    assert_eq!(
        actor_slot(Some("dev/w2")),
        ("dev".into(), String::new(), "w2".into())
    );
    assert_eq!(
        actor_slot(None),
        (String::new(), String::new(), String::new())
    );
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn request(
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

/// A slot's heartbeat usage shows up on its worker row and in the chart, an
/// attempt's final totals arrive on the event, and a mistyped query is
/// refused rather than answered with an empty chart.
#[tokio::test]
async fn the_usage_route_serves_heartbeat_totals_and_refuses_a_bad_query() {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "jeryu-deploy".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, user) = (token("alice"), token("bob"));
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let call = |method, uri: &str, token: &str, body| {
        router.clone().oneshot(request(method, uri, token, body))
    };

    // Reading usage is admin-only, like the other pipeline reads.
    let refused = call(HttpMethod::GET, "/api/v1/usage", &user, None)
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    let beat = |slot: &str, todo: &str, usage: Value| {
        json!({"operator": "dana", "host": "build-1.example", "slot": slot, "family": "jeryu",
               "state": "working", "todo_id": todo, "stage": "agent", "usage": usage})
    };
    let posted = call(
        HttpMethod::POST,
        "/api/v1/shift/heartbeat",
        &admin,
        Some(beat(
            "w1",
            "t1",
            json!({"model": "claude-opus-5", "input_tokens": 1_000, "output_tokens": 120,
                   "cache_read_input_tokens": 9_000, "cache_creation_input_tokens": 400,
                   "cost_usd": 0.42, "turns": 7, "updated_at": "2026-10-06T12:00:00Z"}),
        )),
    )
    .await
    .unwrap();
    assert_eq!(posted.status(), StatusCode::OK);
    // A slot that reports no usage keeps a null one; an old producer that
    // never heard of the field is exactly this case.
    let plain = call(
        HttpMethod::POST,
        "/api/v1/shift/heartbeat",
        &admin,
        Some(
            json!({"operator": "dana", "host": "build-1.example", "slot": "w2", "family": "jeryu",
                    "state": "working", "todo_id": "t2"}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(plain.status(), StatusCode::OK);
    // Totals that cannot be true are refused, not stored.
    let bad = call(
        HttpMethod::POST,
        "/api/v1/shift/heartbeat",
        &admin,
        Some(beat("w1", "t1", json!({"input_tokens": -5}))),
    )
    .await
    .unwrap();
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body_json(bad).await["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("usage.input_tokens")
    );

    let workers = body_json(
        call(HttpMethod::GET, "/api/v1/shift/workers", &admin, None)
            .await
            .unwrap(),
    )
    .await;
    let row = |slot: &str| -> Value {
        workers["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["slot"] == slot)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let public = body_json(
        call(HttpMethod::GET, "/api/v1/shift/workers", &user, None)
            .await
            .unwrap(),
    )
    .await;
    assert!(
        public["workers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["usage"].is_null())
    );
    assert_eq!(row("w1")["usage"]["input_tokens"], 1_000);
    assert_eq!(row("w1")["usage"]["model"], "claude-opus-5");
    assert_eq!(row("w1")["usage"]["turns"], 7);
    assert_eq!(
        row("w2")["usage"],
        Value::Null,
        "a producer that reports no usage leaves the slot's usage null"
    );

    // An attempt's final totals ride its todo.attempt_finished event.
    let finished = call(
        HttpMethod::POST,
        "/api/v1/events",
        &admin,
        Some(json!({"source": "todoq", "kind": "todo.attempt_finished",
                    "summary": "t1 done", "family": "jeryu", "todo_id": "t1",
                    "actor": "dana@build-1.example/w1", "outcome": "done", "cost_usd": 0.55,
                    "detail": {"usage": {"model": "claude-opus-5", "input_tokens": 1_400,
                                         "output_tokens": 200, "cost_usd": 0.55}}})),
    )
    .await
    .unwrap();
    assert_eq!(finished.status(), StatusCode::CREATED);
    let malformed = call(
        HttpMethod::POST,
        "/api/v1/events",
        &admin,
        Some(
            json!({"source": "todoq", "kind": "todo.attempt_finished", "summary": "t1 done",
                    "detail": {"usage": {"output_tokens": -2}}}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(malformed.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body_json(malformed).await["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("detail.usage.output_tokens")
    );

    let chart = body_json(
        call(
            HttpMethod::GET,
            "/api/v1/usage?hours=1&bucket=5m",
            &admin,
            None,
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(chart["bucket_seconds"], 300);
    assert_eq!(chart["buckets"].as_array().unwrap().len(), 13);
    assert_eq!(
        chart["totals"]["input_tokens"], 1_400,
        "the attempt's final totals replace the beat's, counted once"
    );
    assert_eq!(chart["totals"]["output_tokens"], 200);
    assert_eq!(chart["totals"]["cache_read_input_tokens"], 9_000);
    let live = chart["live"].as_array().unwrap();
    assert_eq!(live.len(), 1, "only w1 ever reported usage");
    assert_eq!(live[0]["slot"], "w1");
    assert_eq!(live[0]["todo_id"], "t1");
    assert_eq!(live[0]["input_tokens"], 1_400);
    assert!(live[0].get("tokens_per_minute").is_some());

    // A window or a bucket the route cannot answer is a mistake in the
    // request: an empty chart would read as a shift that spent nothing.
    for (uri, named) in [
        ("/api/v1/usage?hours=0", "hours"),
        ("/api/v1/usage?hours=9999", "hours"),
        ("/api/v1/usage?bucket=5x", "bucket"),
        ("/api/v1/usage?hours=720&bucket=10s", "bucket"),
        ("/api/v1/usage?hourz=2", "hours, bucket, family"),
    ] {
        let refused = call(HttpMethod::GET, uri, &admin, None).await.unwrap();
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
        let body = body_json(refused).await;
        assert_eq!(body["code"], "usage_invalid_query", "{uri}");
        let said = format!("{} {}", body["reason"], body["common_fixes"]);
        assert!(said.contains(named), "{uri} does not say {named:?}: {said}");
    }
    // A family nobody hosts is a typo, not a family that spent nothing.
    let unknown = call(HttpMethod::GET, "/api/v1/usage?family=jeryo", &admin, None)
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(unknown).await["code"], "family_unknown");
}

/// todoq reports a finished attempt's usage per model, for the yield report.
/// That object is stored exactly as sent, and its sum over models is the
/// slot's last usage sample.
#[tokio::test]
async fn a_per_model_attempt_usage_is_kept_for_yield_and_summed_for_the_chart() {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    let admin = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let call = |method, uri: &str, body| router.clone().oneshot(request(method, uri, &admin, body));

    let per_model = json!({
        "model-a": {"input": 1_000, "output": 100, "cache_read": 5_000, "cache_write": 40,
                    "thinking": 7, "cost_usd": 0.5, "cost_basis": "pricing@0000001"},
        "model-b": {"input": 200, "output": 20, "cache_read": 0, "cache_write": 0,
                    "cost_usd": 0.25, "cost_basis": "config"}
    });
    let finished = call(
        HttpMethod::POST,
        "/api/v1/events",
        Some(json!({"source": "todoq", "kind": "todo.attempt_finished",
                    "summary": "t3 done", "family": "jeryu", "todo_id": "t3",
                    "actor": "dana@build-1.example/w3", "outcome": "done", "cost_usd": 0.75,
                    "detail": {"usage": per_model.clone(), "model": "model-a"}})),
    )
    .await
    .unwrap();
    assert_eq!(finished.status(), StatusCode::CREATED);

    let events = body_json(
        call(
            HttpMethod::GET,
            "/api/v1/events?kind=todo.attempt_finished",
            None,
        )
        .await
        .unwrap(),
    )
    .await;
    let stored = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["todo_id"] == "t3")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        stored["detail"]["usage"], per_model,
        "the yield report's per-model usage is stored as sent"
    );

    let chart = body_json(
        call(HttpMethod::GET, "/api/v1/usage?hours=1&bucket=5m", None)
            .await
            .unwrap(),
    )
    .await;
    let live = chart["live"].as_array().unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0]["slot"], "w3");
    assert_eq!(live[0]["input_tokens"], 1_200);
    assert_eq!(live[0]["output_tokens"], 120);
    assert_eq!(live[0]["cache_read_input_tokens"], 5_000);
    assert_eq!(live[0]["cache_creation_input_tokens"], 40);
    assert_eq!(live[0]["cost_usd"], 0.75);
    assert_eq!(
        live[0]["model"],
        Value::Null,
        "an attempt on two models names neither"
    );
}
