//! Request rules every `/api/v1` route shares, applied once in front of the
//! handlers instead of per route:
//!
//! - a trailing slash names the same route as the path without it;
//! - a CORS preflight (`OPTIONS`) is answered before the auth gate, since a
//!   browser never sends credentials on one;
//! - an `Accept` header that rules out JSON answers `406 not_acceptable`
//!   instead of JSON the client said it cannot read.
//!
//! Unreadable input (body, query or path segment) answers 422 whichever layer
//! notices it; [`super::error_envelope::normalize`] owns that rule.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

const PREFIX: &str = "/api/v1/";
const PREFLIGHT_METHODS: &str = "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS";
const PREFLIGHT_MAX_AGE: &str = "600";

/// Drops one or more trailing slashes from an `/api/v1/...` path before
/// routing. `/api/v1/` itself is routed and stays as it is.
pub(super) fn trim_trailing_slash(mut request: Request) -> Request {
    if let Some(uri) = trimmed_uri(request.uri()) {
        *request.uri_mut() = uri;
    }
    request
}

fn trimmed_uri(uri: &Uri) -> Option<Uri> {
    let path = uri.path();
    if !path.to_ascii_lowercase().starts_with(PREFIX) || !path.ends_with('/') {
        return None;
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.len() < PREFIX.len() {
        return None;
    }
    let path_and_query = match uri.query() {
        Some(query) => format!("{trimmed}?{query}"),
        None => trimmed.to_string(),
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(path_and_query.parse().ok()?);
    Uri::from_parts(parts).ok()
}

/// Middleware between the error envelope and the auth gate.
pub(super) async fn apply(request: Request, next: Next) -> Response {
    if !applies(request.uri().path()) {
        return next.run(request).await;
    }
    if request.method() == Method::OPTIONS {
        return preflight(request.headers());
    }
    let accepts_json = accepts_json(request.headers());
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    // Every write route answers JSON, so a write is refused before it runs.
    if !accepts_json && !safe {
        return not_acceptable();
    }
    let response = next.run(request).await;
    // Reads may stream text (raw files, logs, SSE); only a JSON answer the
    // client ruled out is refused. Errors always carry the JSON envelope.
    if !accepts_json && response.status().is_success() && is_json(response.headers()) {
        return not_acceptable();
    }
    response
}

fn applies(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path == "/api/v1" || path.starts_with(PREFIX)
}

/// `204` with the methods and headers the API takes. No origin is granted, so
/// a cross-origin browser call still stops here; same-origin and non-browser
/// clients no longer see a 401 for a request that carries no credentials.
fn preflight(headers: &HeaderMap) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let out = response.headers_mut();
    out.insert(header::ALLOW, HeaderValue::from_static(PREFLIGHT_METHODS));
    out.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(PREFLIGHT_METHODS),
    );
    if let Some(requested) = headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        out.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested.clone());
    }
    out.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(PREFLIGHT_MAX_AGE),
    );
    response
}

/// No `Accept` header, or any range that admits `application/json` with a
/// non-zero quality, accepts JSON.
fn accepts_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::ACCEPT).iter().peekable();
    if values.peek().is_none() {
        return true;
    }
    values
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(range_admits_json)
}

fn range_admits_json(range: &str) -> bool {
    let mut parts = range.split(';').map(str::trim);
    let media = parts.next().unwrap_or_default().to_ascii_lowercase();
    let refused = parts.any(|param| {
        param
            .strip_prefix("q=")
            .and_then(|q| q.trim().parse::<f32>().ok())
            .is_some_and(|q| q <= 0.0)
    });
    !refused
        && (media.is_empty()
            || matches!(media.as_str(), "*/*" | "application/*" | "application/json")
            || media.ends_with("+json"))
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

fn not_acceptable() -> Response {
    let body = json!({
        "code": "not_acceptable",
        "message": "the Accept header excludes the JSON this route answers",
        "reason": "the request's Accept header does not admit application/json",
        "common_fixes": [
            "send Accept: application/json, or omit the Accept header",
        ],
    });
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = StatusCode::NOT_ACCEPTABLE;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn accept_admits_json_ranges_only() {
        assert!(accepts_json(&HeaderMap::new()));
        for admits in [
            "application/json",
            "*/*",
            "application/*",
            "text/html, application/json;q=0.5",
            "text/html,application/xhtml+xml,*/*;q=0.8",
            "application/problem+json",
        ] {
            assert!(accepts_json(&accept(admits)), "{admits}");
        }
        for refuses in [
            "text/html",
            "text/plain",
            "application/json;q=0",
            "image/png",
        ] {
            assert!(!accepts_json(&accept(refuses)), "{refuses}");
        }
    }

    #[test]
    fn trailing_slashes_trim_under_api_v1_only() {
        let trim = |uri: &str| trimmed_uri(&uri.parse().unwrap()).map(|uri| uri.to_string());
        assert_eq!(trim("/api/v1/work/").as_deref(), Some("/api/v1/work"));
        assert_eq!(
            trim("/api/v1/work//?limit=2").as_deref(),
            Some("/api/v1/work?limit=2")
        );
        assert_eq!(trim("/api/v1/work"), None);
        assert_eq!(trim("/api/v1/"), None);
        assert_eq!(trim("/repos/a/b/"), None);
    }
}
