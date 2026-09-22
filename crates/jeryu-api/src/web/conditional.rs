//! Conditional reads for `/api/v1`: every successful JSON answer to a `GET`
//! carries a strong `ETag` (a digest of the body) and `Cache-Control:
//! private, no-cache`, and a request whose `If-None-Match` names that tag
//! answers `304 Not Modified` with no body. A polling client still asks each
//! tick, but an unchanged document costs a header exchange, not a download.
//!
//! Runs inside the auth gate, so a 304 is only ever sent to a caller allowed
//! to read the body it stands for. Streams (SSE, logs, raw files) are not
//! JSON and pass through untouched.

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

const PREFIX: &str = "/api/v1";
const CACHE_CONTROL: &str = "private, no-cache";
/// Bodies past this size are passed through without a tag rather than held.
const MAX_TAGGED_BODY_BYTES: usize = 64 * 1024 * 1024;

pub(super) async fn etag(request: Request, next: Next) -> Response {
    let applies = matches!(*request.method(), Method::GET | Method::HEAD)
        && request.uri().path().starts_with(PREFIX);
    if !applies {
        return next.run(request).await;
    }
    let if_none_match = request.headers().get(header::IF_NONE_MATCH).cloned();
    let response = next.run(request).await;
    if response.status() != StatusCode::OK
        || !is_json(response.headers())
        || response.headers().contains_key(header::ETAG)
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_TAGGED_BODY_BYTES).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "response body could not be read",
        )
            .into_response();
    };
    let tag = entity_tag(&bytes);
    parts.headers.insert(header::ETAG, tag.clone());
    parts.headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    if if_none_match.is_some_and(|value| matches_tag(&value, &tag)) {
        parts.status = StatusCode::NOT_MODIFIED;
        parts.headers.remove(header::CONTENT_LENGTH);
        return Response::from_parts(parts, Body::empty());
    }
    Response::from_parts(parts, Body::from(bytes))
}

fn entity_tag(body: &[u8]) -> HeaderValue {
    let digest = Sha256::digest(body);
    HeaderValue::from_str(&format!("\"{}\"", hex::encode(&digest[..16])))
        .expect("a quoted hex digest is a valid header value")
}

/// `*`, or any listed tag equal to `tag` under the weak comparison RFC 9110
/// prescribes for `If-None-Match`.
fn matches_tag(if_none_match: &HeaderValue, tag: &HeaderValue) -> bool {
    let Ok(listed) = if_none_match.to_str() else {
        return false;
    };
    let tag = tag.to_str().unwrap_or_default();
    listed.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == tag
    })
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let media = value.split(';').next().unwrap_or_default().trim();
            media.eq_ignore_ascii_case("application/json") || media.ends_with("+json")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::Router;
    use axum::middleware::from_fn;
    use axum::routing::get;
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route(
                "/api/v1/work",
                get(|| async { Json(serde_json::json!({"items": [1, 2]})) }),
            )
            .route("/api/v1/log", get(|| async { "plain text" }))
            .route(
                "/health",
                get(|| async { Json(serde_json::json!({"ok": true})) }),
            )
            .layer(from_fn(etag))
    }

    async fn send(uri: &str, if_none_match: Option<&str>) -> Response {
        let mut request = Request::builder().uri(uri);
        if let Some(tag) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, tag);
        }
        app()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn json_reads_carry_a_tag_and_a_matching_tag_answers_304() {
        let first = send("/api/v1/work", None).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.headers()[header::CACHE_CONTROL], CACHE_CONTROL);
        let tag = first.headers()[header::ETAG].to_str().unwrap().to_string();

        let again = send("/api/v1/work", Some(&tag)).await;
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(again.headers()[header::ETAG], tag.as_str());
        let body = to_bytes(again.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty());

        let listed = send("/api/v1/work", Some(&format!("\"other\", W/{tag}"))).await;
        assert_eq!(listed.status(), StatusCode::NOT_MODIFIED);

        let stale = send("/api/v1/work", Some("\"stale\"")).await;
        assert_eq!(stale.status(), StatusCode::OK);
        let body = to_bytes(stale.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], br#"{"items":[1,2]}"#);
    }

    #[tokio::test]
    async fn non_json_and_non_api_answers_are_untouched() {
        let text = send("/api/v1/log", Some("*")).await;
        assert_eq!(text.status(), StatusCode::OK);
        assert!(!text.headers().contains_key(header::ETAG));
        let health = send("/health", None).await;
        assert!(!health.headers().contains_key(header::ETAG));
    }
}
