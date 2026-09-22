//! `Idempotency-Key` replay for `POST` writes on `/api/v1` and the
//! GitHub-shaped `/repos` edge.
//!
//! `PUT`, `PATCH` and `DELETE` already name the resource they set, so a
//! repeat lands on the same state. A `POST` usually creates something (an
//! issue, a comment, a pull request, a session), and a client that lost the
//! answer to a timeout cannot tell whether to retry. With a key it can:
//!
//! - the first request with a key runs and its answer is kept for a day;
//! - a repeat with the same key, caller and request answers that kept reply
//!   again, stamped `Idempotent-Replayed: true`, without running the handler;
//! - a repeat while the first is still running answers `409
//!   idempotency_key_in_flight`;
//! - the same key with a different method, path or body answers `422
//!   idempotency_key_reused`.
//!
//! Keys are scoped to the caller's credentials, so two callers never see each
//! other's answers. Server errors (5xx) are not kept: the write may not have
//! happened, and a retry should run it. Answers that are not a bounded,
//! buffered body (event streams) are not kept either. Runs inside the auth
//! gate, so a rejected caller never reserves a key.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use sha2::{Digest, Sha256};

use super::api_error;

pub(crate) const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
pub(crate) const REPLAYED: HeaderName = HeaderName::from_static("idempotent-replayed");
const MAX_KEY_BYTES: usize = 255;
/// Request bodies past this size are refused when they carry a key.
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// Response bodies past this size are passed through and not kept.
const MAX_KEPT_RESPONSE_BYTES: usize = 1024 * 1024;
const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ENTRIES: usize = 4096;

#[derive(Clone, Default)]
pub(crate) struct IdempotencyStore {
    entries: Arc<Mutex<HashMap<String, Entry>>>,
}

struct Entry {
    fingerprint: [u8; 32],
    at: Instant,
    reply: Option<KeptReply>,
}

#[derive(Clone)]
struct KeptReply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

enum Reservation {
    Reserved,
    Replay(KeptReply),
    InFlight,
    Reused,
}

impl IdempotencyStore {
    fn reserve(&self, scope: &str, fingerprint: [u8; 32]) -> Reservation {
        let mut entries = self.lock();
        let now = Instant::now();
        entries.retain(|_, entry| now.duration_since(entry.at) < RETENTION);
        if let Some(entry) = entries.get(scope) {
            if entry.fingerprint != fingerprint {
                return Reservation::Reused;
            }
            return match &entry.reply {
                Some(reply) => Reservation::Replay(reply.clone()),
                None => Reservation::InFlight,
            };
        }
        if entries.len() >= MAX_ENTRIES {
            // Forget the oldest finished answer rather than refusing new keys.
            if let Some(oldest) = entries
                .iter()
                .filter(|(_, entry)| entry.reply.is_some())
                .min_by_key(|(_, entry)| entry.at)
                .map(|(scope, _)| scope.clone())
            {
                entries.remove(&oldest);
            }
        }
        entries.insert(
            scope.to_string(),
            Entry {
                fingerprint,
                at: now,
                reply: None,
            },
        );
        Reservation::Reserved
    }

    fn keep(&self, scope: &str, reply: KeptReply) {
        if let Some(entry) = self.lock().get_mut(scope) {
            entry.at = Instant::now();
            entry.reply = Some(reply);
        }
    }

    fn release(&self, scope: &str) {
        let mut entries = self.lock();
        if entries
            .get(scope)
            .is_some_and(|entry| entry.reply.is_none())
        {
            entries.remove(scope);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Frees a reservation whose request never produced a kept answer, including
/// when the handler panics or the client goes away mid-request.
struct Reserved<'a> {
    store: &'a IdempotencyStore,
    scope: &'a str,
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        self.store.release(self.scope);
    }
}

pub(super) async fn replay(
    State(store): State<IdempotencyStore>,
    request: Request,
    next: Next,
) -> Response {
    let Some(key) = request.headers().get(&IDEMPOTENCY_KEY).cloned() else {
        return next.run(request).await;
    };
    if !applies(request.method(), request.uri().path()) {
        return next.run(request).await;
    }
    if !key_is_valid(&key) {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "Idempotency-Key must be 1 to 255 visible ASCII characters",
        );
    }
    let (parts, body) = request.into_parts();
    let Ok(body) = to_bytes(body, MAX_REQUEST_BYTES).await else {
        return api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "a request with an Idempotency-Key may carry at most 16 MiB",
        );
    };
    let scope = scope(&parts.headers, &key);
    let fingerprint = fingerprint(&parts.method, &parts.uri, &body);
    match store.reserve(&scope, fingerprint) {
        Reservation::Reserved => {}
        Reservation::Replay(reply) => return replayed(reply),
        Reservation::InFlight => {
            return api_error(
                StatusCode::CONFLICT,
                "idempotency_key_in_flight",
                "a request with this Idempotency-Key is still running; retry once it answers",
            );
        }
        Reservation::Reused => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency_key_reused",
                "this Idempotency-Key was already used for a different request",
            );
        }
    }
    let _reserved = Reserved {
        store: &store,
        scope: &scope,
    };
    let response = next.run(Request::from_parts(parts, Body::from(body))).await;
    if response.status().is_server_error() || !is_buffered(response.headers()) {
        return response;
    }
    let (parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_KEPT_RESPONSE_BYTES).await else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "response body could not be read",
        );
    };
    store.keep(
        &scope,
        KeptReply {
            status: parts.status,
            headers: parts.headers.clone(),
            body: bytes.clone(),
        },
    );
    Response::from_parts(parts, Body::from(bytes))
}

fn applies(method: &Method, path: &str) -> bool {
    *method == Method::POST
        && ["/api/v1/", "/repos/", "/api/v3/repos/"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

fn key_is_valid(key: &HeaderValue) -> bool {
    let bytes = key.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_KEY_BYTES
        && bytes.iter().all(|byte| byte.is_ascii_graphic())
}

/// A declared, bounded length or no body at all: the answers worth keeping.
/// Event streams and chunked downloads have neither.
fn is_buffered(headers: &HeaderMap) -> bool {
    let streaming = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    !streaming && length.is_none_or(|length| length <= MAX_KEPT_RESPONSE_BYTES)
}

/// The key, bound to whoever presented it: a digest of the credentials the
/// auth gate reads, so one caller's key never replays another caller's answer.
fn scope(headers: &HeaderMap, key: &HeaderValue) -> String {
    let mut digest = Sha256::new();
    for name in [header::AUTHORIZATION, header::COOKIE] {
        for value in headers.get_all(name) {
            digest.update(value.as_bytes());
            digest.update([0]);
        }
        digest.update([1]);
    }
    digest.update(key.as_bytes());
    hex::encode(digest.finalize())
}

fn fingerprint(method: &Method, uri: &axum::http::Uri, body: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(method.as_str());
    digest.update([0]);
    digest.update(uri.to_string());
    digest.update([0]);
    digest.update(body);
    digest.finalize().into()
}

fn replayed(reply: KeptReply) -> Response {
    let mut response = Response::new(Body::from(reply.body));
    *response.status_mut() = reply.status;
    *response.headers_mut() = reply.headers;
    response
        .headers_mut()
        .insert(REPLAYED, HeaderValue::from_static("true"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::middleware::from_fn_with_state;
    use axum::routing::post;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    fn app(store: IdempotencyStore, calls: Arc<AtomicUsize>) -> Router {
        let handler = move |body: String| {
            let calls = calls.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if body == "fail" {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "boom".to_string());
                }
                (StatusCode::CREATED, format!("created #{n}"))
            }
        };
        Router::new()
            .route("/api/v1/things", post(handler.clone()))
            .route("/elsewhere", post(handler))
            .layer(from_fn_with_state(store, replay))
    }

    async fn send(app: &Router, uri: &str, key: Option<&str>, auth: &str, body: &str) -> Response {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::AUTHORIZATION, auth);
        if let Some(key) = key {
            request = request.header(IDEMPOTENCY_KEY, key);
        }
        app.clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    async fn text(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn repeat_with_the_same_key_replays_without_running_the_handler() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(IdempotencyStore::default(), calls.clone());
        let first = send(&app, "/api/v1/things", Some("k1"), "token a", "{}").await;
        assert_eq!(first.status(), StatusCode::CREATED);
        assert!(!first.headers().contains_key(REPLAYED));
        assert_eq!(text(first).await, "created #1");

        let again = send(&app, "/api/v1/things", Some("k1"), "token a", "{}").await;
        assert_eq!(again.status(), StatusCode::CREATED);
        assert_eq!(again.headers()[REPLAYED], "true");
        assert_eq!(text(again).await, "created #1");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn without_a_key_or_outside_scope_every_request_runs() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(IdempotencyStore::default(), calls.clone());
        send(&app, "/api/v1/things", None, "token a", "{}").await;
        send(&app, "/api/v1/things", None, "token a", "{}").await;
        send(&app, "/elsewhere", Some("k1"), "token a", "{}").await;
        send(&app, "/elsewhere", Some("k1"), "token a", "{}").await;
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn a_different_request_under_the_same_key_is_refused() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(IdempotencyStore::default(), calls.clone());
        send(&app, "/api/v1/things", Some("k1"), "token a", "{\"a\":1}").await;
        let reused = send(&app, "/api/v1/things", Some("k1"), "token a", "{\"a\":2}").await;
        assert_eq!(reused.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(text(reused).await.contains("idempotency_key_reused"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn keys_are_scoped_to_the_caller() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(IdempotencyStore::default(), calls.clone());
        send(&app, "/api/v1/things", Some("k1"), "token a", "{}").await;
        let other = send(&app, "/api/v1/things", Some("k1"), "token b", "{}").await;
        assert!(!other.headers().contains_key(REPLAYED));
        assert_eq!(text(other).await, "created #2");
    }

    #[tokio::test]
    async fn server_errors_are_not_kept_so_a_retry_runs_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(IdempotencyStore::default(), calls.clone());
        let failed = send(&app, "/api/v1/things", Some("k1"), "token a", "fail").await;
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
        send(&app, "/api/v1/things", Some("k1"), "token a", "fail").await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_malformed_key_is_rejected() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(IdempotencyStore::default(), calls.clone());
        let long = "x".repeat(MAX_KEY_BYTES + 1);
        for key in ["has space", long.as_str()] {
            let response = send(&app, "/api/v1/things", Some(key), "token a", "{}").await;
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_running_request_holds_its_key_until_released() {
        let store = IdempotencyStore::default();
        let print = [7; 32];
        assert!(matches!(store.reserve("s", print), Reservation::Reserved));
        assert!(matches!(store.reserve("s", print), Reservation::InFlight));
        drop(Reserved {
            store: &store,
            scope: "s",
        });
        assert!(matches!(store.reserve("s", print), Reservation::Reserved));
    }
}
