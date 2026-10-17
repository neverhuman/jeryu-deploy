//! A newly listed waitlist address is forwarded, signed, to a configured
//! receiver; a repeat is not; a receiver that keeps failing is retried and
//! then recorded, and the join's answer never depends on any of it.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::ForgeCore;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{WebState, app};
use crate::web::waitlist_forward::WaitlistForward;

const SECRET: &str = "a-shared-secret-of-at-least-32-characters";

#[derive(Clone, Default)]
struct Received {
    requests: Arc<Mutex<Vec<(HeaderMap, String)>>>,
}

/// A receiver on a free loopback port that answers `status` to every POST.
async fn receiver(status: StatusCode) -> (String, Received) {
    let received = Received::default();
    let kept = received.clone();
    let router = axum::Router::new().route(
        "/intake",
        axum::routing::post(move |headers: HeaderMap, body: String| {
            let kept = kept.clone();
            async move {
                kept.requests.lock().unwrap().push((headers, body));
                status
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}/intake"), received)
}

fn router(core: ForgeCore, url: &str) -> axum::Router {
    let mut state = WebState::new(core).with_auth(true, false, false);
    state.waitlist_forward = Some(WaitlistForward::for_test(url, SECRET));
    app(state, Path::new("/tmp/jeryu-no-spa"))
}

async fn join(router: &axum::Router, body: Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/waitlist")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

async fn wait_for(received: &Received, count: usize) -> Vec<(HeaderMap, String)> {
    for _ in 0..300 {
        if received.requests.lock().unwrap().len() >= count {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    received.requests.lock().unwrap().clone()
}

#[tokio::test]
async fn a_new_address_is_forwarded_signed_and_a_repeat_is_not() {
    let (url, received) = receiver(StatusCode::ACCEPTED).await;
    let router = router(ForgeCore::new(), &url);
    join(
        &router,
        json!({ "email": "Ada@Example.com", "name": "Ada", "note": "gated merges" }),
    )
    .await;
    let requests = wait_for(&received, 1).await;
    assert_eq!(requests.len(), 1, "one forward for a new address");
    let (headers, body) = &requests[0];
    let timestamp: i64 = headers["x-waitlist-timestamp"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((chrono::Utc::now().timestamp() - timestamp).abs() < 60);
    assert_eq!(
        headers["x-waitlist-signature"].to_str().unwrap(),
        WaitlistForward::signature(SECRET.as_bytes(), timestamp, body),
        "the signature covers exactly the body that was sent"
    );
    assert_eq!(headers["content-type"], "application/json");
    let body: Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["product"], "jeryu");
    assert_eq!(body["email"], "ada@example.com");
    assert_eq!(body["name"], "Ada");
    assert_eq!(body["note"], "gated merges");
    assert_eq!(body["origin"], "https://forge.example");
    assert!(
        chrono::DateTime::parse_from_rfc3339(body["joinedAt"].as_str().unwrap()).is_ok(),
        "{body}"
    );

    join(&router, json!({ "email": "ada@example.com" })).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        received.requests.lock().unwrap().len(),
        1,
        "a repeat join is not forwarded"
    );
}

#[tokio::test]
async fn a_failing_receiver_is_retried_then_recorded() {
    let (url, received) = receiver(StatusCode::INTERNAL_SERVER_ERROR).await;
    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).unwrap();
    let router = router(core.clone(), &url);
    join(&router, json!({ "email": "grace@example.com" })).await;
    assert_eq!(wait_for(&received, 3).await.len(), 3, "three attempts");
    let mut records = Vec::new();
    for _ in 0..300 {
        records = core.list_audit("waitlist_forward").unwrap();
        if !records.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let record = records.first().expect("the outcome is recorded");
    assert_eq!(record.action, "waitlist.forwarded");
    assert_eq!(record.phase, "failed");
    let printed = format!("{record:?}");
    assert!(printed.contains("answered 500"), "{printed}");
    assert!(
        !printed.contains("grace@example.com"),
        "no address: {printed}"
    );
    assert!(!printed.contains("127.0.0.1"), "no receiver URL: {printed}");
    assert_eq!(core.list_waitlist().len(), 1, "the forge keeps its own row");
}

#[tokio::test]
async fn without_a_forward_nothing_is_sent() {
    let state = WebState::new(ForgeCore::new());
    assert!(state.waitlist_forward.is_none());
}
