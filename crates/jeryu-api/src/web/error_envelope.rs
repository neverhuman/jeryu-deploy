//! One error envelope for every `/api/v1` rejection.
//!
//! Handlers, the auth gate, and axum's own extractor rejections (malformed
//! JSON, a missing `Content-Type`, a path parameter of the wrong type, a
//! method the route does not take) each used to answer a different shape, some
//! of them `text/plain`. This middleware rewrites every error response under
//! `/api` (except the GitHub-shaped `/api/v3` edge) into the single shape:
//!
//! `code`, `message`, `reason`, `purpose`, `common_fixes`, `repair_hint`,
//! `docs_url`
//!
//! `code` is always one of [`super::error_codes::ERROR_CODES`], which
//! `GET /api/v1/errors` publishes.

use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use super::error_codes::{ERROR_CODES, for_status, lookup};

/// The fields every error envelope carries, in the order they are documented.
pub(super) const ENVELOPE_FIELDS: [&str; 7] = [
    "code",
    "message",
    "reason",
    "purpose",
    "common_fixes",
    "repair_hint",
    "docs_url",
];
const DOCS_URL: &str = "docs/errors.md";
const DEFAULT_PURPOSE: &str = "complete a jeryu API request";
const DEFAULT_REPAIR_HINT: &str =
    "look the code up at GET /api/v1/errors, fix the request it names, and retry";
/// Error bodies are small; anything larger is not worth buffering to reshape.
const MAX_ERROR_BODY: usize = 256 * 1024;

/// `GET /api/v1/errors`: the closed set of error codes and the envelope shape.
pub(super) async fn catalog() -> Json<Value> {
    Json(json!({
        "schema": "jeryu.api.errors.v1",
        "envelope_fields": ENVELOPE_FIELDS,
        "docs_url": DOCS_URL,
        "codes": ERROR_CODES
            .iter()
            .map(|entry| json!({
                "code": entry.code,
                "status": entry.status,
                "summary": entry.summary,
            }))
            .collect::<Vec<_>>(),
    }))
}

pub(super) async fn normalize(request: Request, next: Next) -> Response {
    let applies = applies(request.uri().path());
    let method = request.method().clone();
    let response = next.run(request).await;
    if !applies || !(response.status().is_client_error() || response.status().is_server_error()) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_ERROR_BODY).await else {
        let envelope = envelope_for(parts.status, &method, &[]);
        return (parts.status, Json(envelope)).into_response();
    };
    let envelope = envelope_for(parts.status, &method, &bytes);
    let body = serde_json::to_vec(&envelope).unwrap_or_default();
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Response::from_parts(parts, Body::from(body))
}

/// Every API version except the GitHub-shaped `/api/v3` edge, case-blind like
/// the unknown-route fallback.
fn applies(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    (path == "/api" || path.starts_with("/api/"))
        && !(path == "/api/v3" || path.starts_with("/api/v3/"))
}

fn envelope_for(status: StatusCode, method: &Method, bytes: &[u8]) -> Value {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(object)) => from_json(status, object),
        _ => from_text(status, method, &String::from_utf8_lossy(bytes)),
    }
}

/// Folds the older JSON shapes (`{code, message}`, `{error: {code, message}}`,
/// a nested `jeryu_repair_hint`) into the envelope, keeping any other fields.
fn from_json(status: StatusCode, mut object: Map<String, Value>) -> Value {
    let nested_error = object.get("error").cloned();
    let hint = match object.remove("jeryu_repair_hint") {
        Some(Value::Object(hint)) => hint,
        _ => Map::new(),
    };
    let text = |object: &Map<String, Value>, key: &str| {
        object
            .get(key)
            .or_else(|| hint.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
    };
    let code = text(&object, "code")
        .or_else(|| {
            nested_error
                .as_ref()
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| for_status(status.as_u16()).to_string());
    let summary = lookup(&code).map_or("the request failed", |entry| entry.summary);
    let message = text(&object, "message")
        .or_else(|| match &nested_error {
            Some(Value::String(message)) => Some(message.clone()),
            Some(error) => error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string),
            None => None,
        })
        .unwrap_or_else(|| summary.to_string());
    let reason = text(&object, "reason").unwrap_or_else(|| message.clone());
    let common_fixes = object
        .get("common_fixes")
        .or_else(|| hint.get("common_fixes"))
        .and_then(Value::as_array)
        .filter(|fixes| !fixes.is_empty() && fixes.iter().all(Value::is_string))
        .cloned()
        .map_or_else(|| json!(default_fixes(&code)), Value::Array);
    let purpose = text(&object, "purpose").unwrap_or_else(|| DEFAULT_PURPOSE.to_string());
    let repair_hint =
        text(&object, "repair_hint").unwrap_or_else(|| DEFAULT_REPAIR_HINT.to_string());
    let docs_url = text(&object, "docs_url").unwrap_or_else(|| DOCS_URL.to_string());
    for (key, value) in [
        ("code", json!(code)),
        ("message", json!(message)),
        ("reason", json!(reason)),
        ("purpose", json!(purpose)),
        ("common_fixes", common_fixes),
        ("repair_hint", json!(repair_hint)),
        ("docs_url", json!(docs_url)),
    ] {
        object.insert(key.to_string(), value);
    }
    Value::Object(object)
}

/// Shapes a `text/plain` or empty body, which is what axum's own extractor
/// rejections and a 405 answer with.
fn from_text(status: StatusCode, method: &Method, text: &str) -> Value {
    let text = text.trim();
    let (code, reason) = if let Some(detail) = strip_any(
        text,
        &[
            "Failed to parse the request body as JSON:",
            "Failed to deserialize the JSON body into the target type:",
        ],
    ) {
        ("invalid_json_body", detail.to_string())
    } else if status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
        (
            "unsupported_media_type",
            "the request body was sent without Content-Type: application/json".to_string(),
        )
    } else if text.starts_with("Invalid URL")
        || text.contains("path argument")
        || text.contains("paths parameters")
    {
        // The rejection text names Rust types (`u64`, `Uuid`); keep those out.
        (
            "invalid_path_parameter",
            "a path segment could not be read as the value this route expects".to_string(),
        )
    } else if let Some(detail) = strip_any(text, &["Failed to deserialize query string:"]) {
        ("invalid_query", detail.to_string())
    } else if status == StatusCode::METHOD_NOT_ALLOWED {
        // axum adds the `Allow` header outside this middleware; it stays on
        // the response.
        (
            "method_not_allowed",
            format!("this route does not accept {method}; the Allow header lists what it does"),
        )
    } else {
        let code = for_status(status.as_u16());
        let summary = lookup(code).map_or("the request failed", |entry| entry.summary);
        let reason = if text.is_empty() { summary } else { text };
        (code, reason.to_string())
    };
    let summary = lookup(code).map_or("the request failed", |entry| entry.summary);
    json!({
        "code": code,
        "message": summary,
        "reason": reason,
        "purpose": DEFAULT_PURPOSE,
        "common_fixes": default_fixes(code),
        "repair_hint": DEFAULT_REPAIR_HINT,
        "docs_url": DOCS_URL,
    })
}

fn strip_any<'a>(text: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))
        .map(str::trim)
}

fn default_fixes(code: &str) -> &'static [&'static str] {
    match code {
        "invalid_json_body" => &[
            "send a body that parses as JSON",
            "match the field names and value types the route documents",
        ],
        "unsupported_media_type" => &["send the JSON body with Content-Type: application/json"],
        "invalid_path_parameter" => &[
            "check each path segment against the route listed at GET /api/v1",
            "numeric ids must be digits only",
        ],
        "invalid_query" => &["check the query parameter names and value types"],
        "method_not_allowed" => &["retry with one of the methods in the Allow header"],
        "payload_too_large" => &["send a smaller request body"],
        _ => &[
            "look the code up at GET /api/v1/errors",
            "check the request against the route index at GET /api/v1",
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::super::error_codes::ERROR_CODES;
    use super::*;
    use std::collections::BTreeSet;

    fn assert_envelope(value: &Value, code: &str) {
        for field in ENVELOPE_FIELDS {
            assert!(value.get(field).is_some(), "{field} missing from {value}");
        }
        assert_eq!(value["code"], code, "{value}");
        assert!(
            value["common_fixes"]
                .as_array()
                .is_some_and(|f| !f.is_empty())
        );
    }

    #[test]
    fn scope_skips_the_github_edge() {
        assert!(applies("/api/v1/work"));
        assert!(applies("/API/V1/work"));
        assert!(applies("/api/nope"));
        assert!(!applies("/api/v3/repos"));
        assert!(!applies("/repos/a/b"));
    }

    #[test]
    fn older_json_shapes_fold_into_the_envelope() {
        let flat = from_json(
            StatusCode::NOT_FOUND,
            json!({"code": "not_found", "message": "work not found"})
                .as_object()
                .cloned()
                .unwrap(),
        );
        assert_envelope(&flat, "not_found");
        assert_eq!(flat["message"], "work not found");

        let nested = from_json(
            StatusCode::NOT_FOUND,
            json!({"error": {"code": "not_found", "message": "agent run x not found"}})
                .as_object()
                .cloned()
                .unwrap(),
        );
        assert_envelope(&nested, "not_found");
        assert_eq!(nested["message"], "agent run x not found");

        let hinted = from_json(
            StatusCode::NOT_FOUND,
            json!({"code": "not_found", "message": "m", "jeryu_repair_hint": {
                "purpose": "load repository README", "common_fixes": ["a"],
                "docs_url": "docs/errors.md#not-found", "repair_hint": "h", "reason": "r"}})
            .as_object()
            .cloned()
            .unwrap(),
        );
        assert_envelope(&hinted, "not_found");
        assert_eq!(hinted["purpose"], "load repository README");
        assert!(hinted.get("jeryu_repair_hint").is_none());
    }

    #[test]
    fn registry_is_sorted_unique_and_covers_status_fallbacks() {
        let codes: Vec<_> = ERROR_CODES.iter().map(|entry| entry.code).collect();
        let mut sorted = codes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(codes, sorted, "ERROR_CODES must stay sorted and unique");
        for status in 400..600 {
            assert!(lookup(for_status(status)).is_some(), "status {status}");
        }
    }

    /// The published set is closed: every code a handler under `web/` can
    /// answer is in [`ERROR_CODES`].
    #[test]
    fn every_code_in_the_sources_is_published() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = vec![root.join("web.rs")];
        let mut dirs = vec![root.join("web")];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|name| name != "tests") {
                        dirs.push(path);
                    }
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let name = path.file_name().unwrap().to_string_lossy().to_string();
                    // Test files, and the WebSocket protocol's own codes.
                    if !name.ends_with("tests.rs") && name != "ws.rs" {
                        files.push(path);
                    }
                }
            }
        }
        let published: BTreeSet<_> = ERROR_CODES.iter().map(|entry| entry.code).collect();
        let mut missing = BTreeSet::new();
        for file in files {
            let source = std::fs::read_to_string(&file).unwrap();
            for code in code_literals(&source) {
                if !published.contains(code.as_str()) {
                    missing.insert(format!("{code} ({})", file.display()));
                }
            }
        }
        assert!(missing.is_empty(), "unpublished error codes: {missing:?}");
    }

    /// Snake-case literals in code position: `code: "x"`, `"code": "x"`, and
    /// the first string after a `StatusCode::X,` argument.
    fn code_literals(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        for marker in ["code: \"", "\"code\": \""] {
            for (index, _) in source.match_indices(marker) {
                found.extend(snake_literal(&source[index + marker.len()..]));
            }
        }
        for (index, _) in source.match_indices("StatusCode::") {
            let rest = &source[index + "StatusCode::".len()..];
            let rest = rest.trim_start_matches(|ch: char| ch.is_ascii_uppercase() || ch == '_');
            let Some(rest) = rest.strip_prefix(',') else {
                continue;
            };
            if let Some(rest) = rest.trim_start().strip_prefix('"') {
                found.extend(snake_literal(rest));
            }
        }
        found
    }

    fn snake_literal(rest: &str) -> Option<String> {
        let end = rest.find('"')?;
        let literal = &rest[..end];
        (literal.contains('_')
            && literal
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'))
        .then(|| literal.to_string())
    }

    #[test]
    fn docs_list_every_code() {
        let docs = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/errors.md"),
        )
        .unwrap();
        for entry in ERROR_CODES {
            assert!(
                docs.contains(&format!("| `{}` |", entry.code)),
                "docs/errors.md is missing {}",
                entry.code
            );
        }
    }
    async fn send(
        state: crate::web::WebState,
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        body: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
        use tower::ServiceExt;
        let app = crate::web::app(state, std::path::Path::new("/tmp/jeryu-no-spa"));
        let mut request = axum::http::Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let response = app
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("{uri}: not JSON: {}", String::from_utf8_lossy(&bytes)));
        (status, headers, value)
    }

    fn open_state() -> crate::web::WebState {
        crate::web::WebState::new(jeryu_core::ForgeCore::new())
    }

    #[tokio::test]
    async fn framework_rejections_answer_the_envelope() {
        let json = Some("application/json");
        let cases = [
            (
                "POST",
                "/api/v1/work",
                json,
                "{not json",
                "invalid_json_body",
            ),
            ("POST", "/api/v1/work", None, "{}", "unsupported_media_type"),
            (
                "GET",
                "/api/v1/repos/r/pulls/abc",
                None,
                "",
                "invalid_path_parameter",
            ),
            (
                "DELETE",
                "/api/v1/bootstrap",
                None,
                "",
                "method_not_allowed",
            ),
            ("GET", "/api/v1/nope", None, "", "api_route_not_found"),
        ];
        for (method, uri, content_type, body, code) in cases {
            let (status, headers, value) =
                send(open_state(), method, uri, content_type, body).await;
            assert!(status.is_client_error(), "{method} {uri}: {status}");
            assert_eq!(
                headers[header::CONTENT_TYPE],
                "application/json",
                "{method} {uri}"
            );
            assert_envelope(&value, code);
            assert!(lookup(code).is_some());
            if code == "method_not_allowed" {
                assert!(headers.contains_key(header::ALLOW));
                assert!(
                    value["reason"].as_str().unwrap().contains("DELETE"),
                    "{value}"
                );
            }
            if code == "invalid_path_parameter" {
                assert!(!value.to_string().contains("u64"), "{value}");
            }
        }
    }

    #[tokio::test]
    async fn auth_gate_and_handler_errors_answer_the_envelope() {
        let mut state = open_state();
        state.auth_required = true;
        let (status, _, value) = send(state, "GET", "/api/v1/bootstrap", None, "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_envelope(&value, "unauthorized");

        let (status, _, value) = send(open_state(), "GET", "/api/v1/work/JRY-999", None, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{value}");
        assert_envelope(&value, "not_found");
    }

    #[tokio::test]
    async fn the_code_list_is_published_without_a_login() {
        let mut state = open_state();
        state.auth_required = true;
        let (status, _, value) = send(state, "GET", "/api/v1/errors", None, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["envelope_fields"], json!(ENVELOPE_FIELDS));
        let codes = value["codes"].as_array().unwrap();
        assert_eq!(codes.len(), ERROR_CODES.len());
        assert!(
            codes
                .iter()
                .any(|entry| entry["code"] == "api_route_not_found")
        );
    }

    #[tokio::test]
    async fn api_v1_publishes_its_route_index_without_a_login() {
        let mut state = open_state();
        state.auth_required = true;
        let (status, _, value) = send(state, "GET", "/api/v1", None, "").await;
        assert_eq!(status, StatusCode::OK, "{value}");
        let routes: Vec<&str> = value["jeryu_api_routes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|route| route.as_str().unwrap())
            .collect();
        for expected in [
            "GET /api/v1",
            "GET /api/v1/errors",
            "GET /api/v1/work",
            "POST /api/v1/work",
            "PATCH /api/v1/work/{key}",
            "DELETE /api/v1/repos/{id}",
        ] {
            assert!(routes.contains(&expected), "{expected} missing: {routes:?}");
        }
        assert!(routes.iter().all(|route| !route.starts_with("HEAD ")));
        assert!(
            routes.iter().all(|route| !route.contains(':')),
            "{routes:?}"
        );
    }

    #[tokio::test]
    async fn unknown_api_route_points_at_the_route_index() {
        let (status, _, value) = send(open_state(), "GET", "/api/v1/nope", None, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_envelope(&value, "api_route_not_found");
        assert_eq!(value["docs_url"], "/api/v1", "{value}");
        assert!(!value.to_string().contains("docs/phase7-api.md"), "{value}");
    }
}
