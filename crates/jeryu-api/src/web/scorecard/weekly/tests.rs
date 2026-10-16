//! The weekly digest job: its cadence and target configuration, and the path
//! it really takes -- compute, record, show in the attention inbox, deliver to
//! a webhook.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use jeryu_core::ForgeCore;
use serde_json::Value;

use super::*;
use crate::web::WebState;
use crate::web::pipeline::EventsQuery;
use crate::web::pipeline::attention;

const HOUR: Duration = Duration::from_secs(60 * 60);

#[test]
fn the_cadence_defaults_to_a_week_and_zero_disables() {
    assert_eq!(interval_from(None), Some(168 * HOUR));
    assert_eq!(interval_from(Some("  ")), Some(168 * HOUR));
    assert_eq!(interval_from(Some("24")), Some(24 * HOUR));
    assert_eq!(interval_from(Some("0")), None);
    assert_eq!(interval_from(Some("weekly")), None);
}

#[test]
fn a_digest_is_due_only_when_every_known_one_is_stale() {
    let week = 168 * HOUR;
    assert!(due(week, None, None));
    assert!(due(week, Some(200 * HOUR), None));
    assert!(!due(week, Some(2 * HOUR), None));
    // One this process just built, not yet found in the log, blocks another.
    assert!(!due(week, None, Some(HOUR)));
    assert!(due(week, Some(200 * HOUR), Some(169 * HOUR)));
}

/// There is no default target: with nothing configured the digest stays in
/// the attention inbox, and a target that is not an http(s) URL is refused by
/// name rather than posted to.
#[test]
fn the_webhook_target_has_no_default_and_is_checked() {
    assert_eq!(target_from(None), Target::None);
    assert_eq!(target_from(Some("   ")), Target::None);
    assert_eq!(
        target_from(Some("https://example.invalid/hooks/scorecard")),
        Target::Url("https://example.invalid/hooks/scorecard".to_string())
    );
    for refused in [
        "file:///etc/passwd",
        "/hooks/scorecard",
        "example.invalid/hooks",
        "ftp://example.invalid/hooks",
    ] {
        assert!(
            matches!(target_from(Some(refused)), Target::Malformed(_)),
            "{refused} was not refused"
        );
    }
}

#[test]
fn the_startup_line_states_the_cadence_and_where_it_posts() {
    let line = startup_line(None, interval_from(None), Target::None);
    assert!(line.contains("every 168h"), "{line}");
    assert!(line.contains("no JERYU_SCORECARD_DIGEST_WEBHOOK"), "{line}");
    let line = startup_line(
        Some("24"),
        interval_from(Some("24")),
        Target::Url("https://example.invalid/hooks".to_string()),
    );
    assert!(line.contains("every 24h"), "{line}");
    assert!(line.contains("https://example.invalid/hooks"), "{line}");
    let line = startup_line(Some("0"), interval_from(Some("0")), Target::None);
    assert!(line.contains("disabled"), "{line}");
}

fn state() -> (Arc<WebState>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let state = WebState::new_with_git_storage(ForgeCore::new(), dir.path().to_path_buf());
    (Arc::new(state), dir)
}

fn digest_events(state: &WebState) -> Vec<crate::web::pipeline::Event> {
    state
        .events
        .query(&EventsQuery {
            kind: Some(DIGEST_KIND.to_string()),
            limit: Some(10),
            ..Default::default()
        })
        .expect("read the event log")
}

/// The whole path: the job computes a Scorecard report, records the digest,
/// and the attention inbox carries that week's numbers as one row.
#[tokio::test]
async fn the_recorded_digest_reaches_the_attention_inbox() {
    let (state, _dir) = state();
    let now = Utc.with_ymd_and_hms(2026, 10, 16, 9, 0, 0).unwrap();
    post_digest(&state, now).await.expect("build the digest");

    let events = digest_events(&state);
    assert_eq!(events.len(), 1, "{events:?}");
    let stored: Digest =
        serde_json::from_value(events[0].detail.clone().expect("the payload")).unwrap();
    assert_eq!(stored.window, WINDOW);
    assert_eq!(stored.schema_version, "jeryu.scorecard.digest/v1");
    assert_eq!(stored.headline.len(), 8, "{:?}", stored.headline);

    let inbox = attention::collect(&state, now);
    let row = inbox
        .items
        .iter()
        .find(|item| item.kind == "scorecard_digest")
        .expect("the digest row");
    assert_eq!(row.severity, attention::Severity::Watch);
    assert_eq!(row.href, "/scorecard");
    assert_eq!(row.digest.as_ref(), Some(&stored));
    assert!(row.reason.contains("lead time p50"), "{}", row.reason);
    // A digest older than a week and a day is last week's reading.
    let later = now + chrono::Duration::days(9);
    state.attention.invalidate();
    assert!(
        !attention::collect(&state, later)
            .items
            .iter()
            .any(|item| item.kind == "scorecard_digest")
    );
}

/// The day's digest is recorded once: a restart, or an hourly wake that reads
/// a stale timestamp, cannot post the same week twice.
#[tokio::test]
async fn the_same_day_records_one_digest() {
    let (state, _dir) = state();
    let now = Utc.with_ymd_and_hms(2026, 10, 16, 9, 0, 0).unwrap();
    post_digest(&state, now).await.expect("build the digest");
    post_digest(&state, now + chrono::Duration::hours(2))
        .await
        .expect("build the digest again");
    assert_eq!(digest_events(&state).len(), 1);
    // The next week's digest is its own event.
    post_digest(&state, now + chrono::Duration::days(7))
        .await
        .expect("build next week's digest");
    assert_eq!(digest_events(&state).len(), 2);
}

/// A webhook target really receives the payload, as JSON, by POST.
#[tokio::test]
async fn the_payload_is_posted_to_the_target() {
    let received: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = received.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let url = format!("http://{}/hooks/scorecard", listener.local_addr().unwrap());
    let router = axum::Router::new().route(
        "/hooks/scorecard",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(body);
                axum::http::StatusCode::NO_CONTENT
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await });

    let (state, _dir) = state();
    let now = Utc.with_ymd_and_hms(2026, 10, 16, 9, 0, 0).unwrap();
    let report = super::super::report(&state, WINDOW, None, None, now.timestamp_millis()).unwrap();
    let digest = super::super::digest::build(&report);
    send(&url, &digest).await.expect("the target accepts it");
    let posted = received.lock().unwrap().clone();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0]["schema_version"], "jeryu.scorecard.digest/v1");
    assert_eq!(posted[0]["window"], WINDOW);
    assert_eq!(
        posted[0]["headline"][0]["metric"], "lead_time_p50",
        "{:?}",
        posted[0]
    );
    server.abort();
}

/// A target that answers an error leaves the digest in the log and says the
/// post did not happen, rather than being retried into duplicate posts.
#[tokio::test]
async fn a_refusing_target_is_recorded_as_undelivered() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let url = format!("http://{}/hooks/scorecard", listener.local_addr().unwrap());
    let router = axum::Router::new().route(
        "/hooks/scorecard",
        axum::routing::post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await });

    let (state, _dir) = state();
    let now = Utc.with_ymd_and_hms(2026, 10, 16, 9, 0, 0).unwrap();
    let report = super::super::report(&state, WINDOW, None, None, now.timestamp_millis()).unwrap();
    let digest = super::super::digest::build(&report);
    let failure = send(&url, &digest)
        .await
        .expect_err("the target refused it");
    assert!(failure.contains("500"), "{failure}");
    undelivered(&state, &digest, &failure);
    let recorded = state
        .events
        .query(&EventsQuery {
            kind: Some(UNDELIVERED_KIND.to_string()),
            limit: Some(10),
            ..Default::default()
        })
        .expect("read the event log");
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert!(recorded[0].needs_human);
    assert_eq!(recorded[0].reason.as_deref(), Some(failure.as_str()));
    server.abort();
}
