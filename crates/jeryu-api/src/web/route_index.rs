//! `GET /api/v1`: the route index for the primary API, the counterpart of
//! the `jeryu_api_routes` list the `/api/v3` edge already answers with, and
//! `GET /api/v1/openapi.json`, the OpenAPI document built from the same list.
//!
//! The index is read from the same route list the router mounts
//! ([`super::api_v1_routes`]); each route's methods are read by asking its
//! handlers for a method none of them serve, which makes axum answer 405 with
//! the `Allow` header it built from the registered handlers.
//!
//! A bare list of paths left a caller to guess the rest, so each route is also
//! described: its path parameters, the query keys it reads (taken from the
//! [`StrictFields`](super::strict_query::StrictFields) the handler validates
//! with, so the index and the refusal agree), and what credential it needs.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::routing::MethodRouter;
use serde_json::{Map, Value, json};
use tower::ServiceExt;

use super::WebState;
use super::strict_query::StrictFields;

/// Where the index lives; the unknown-route 404 points callers here.
pub(super) const INDEX_PATH: &str = crate::discovery::TYPED_INDEX_PATH;
const OPENAPI_VERSION: &str = "3.1.0";

/// One route, described.
struct RouteDoc {
    /// The path as a caller types it, `{id}` for a parameter.
    display: String,
    methods: Vec<String>,
    path_params: Vec<String>,
    query_params: &'static [&'static str],
    /// Method -> the credential that method needs.
    auth: Vec<(String, &'static str)>,
}

pub(super) async fn api_v1(State(state): State<Arc<WebState>>) -> Json<Value> {
    let docs = describe(&state).await;
    let mut routes = vec![format!("GET {INDEX_PATH}")];
    for doc in &docs {
        for method in &doc.methods {
            routes.push(format!("{method} {}", doc.display));
        }
    }
    Json(json!({
        "jeryu_api_routes": routes,
        // The same routes, described: what each one takes and who may call it.
        "routes": docs.iter().map(described).collect::<Vec<_>>(),
        "errors": "/api/v1/errors",
        "capabilities": crate::discovery::CAPABILITIES_PATH,
        "docs_url": crate::discovery::DOCS_PATH,
        "openapi": crate::discovery::OPENAPI_PATH,
        "github_compatible_edge": "/api/v3",
    }))
}

/// `GET /api/v1/openapi.json`: the OpenAPI document for the primary API, built
/// from the mounted routes, so it cannot describe a route that is not served.
pub(super) async fn openapi(State(state): State<Arc<WebState>>) -> Json<Value> {
    let mut paths = Map::new();
    for doc in describe(&state).await {
        let mut operations = Map::new();
        for method in &doc.methods {
            let Some(verb) = openapi_verb(method) else {
                continue;
            };
            let credential = doc
                .auth
                .iter()
                .find(|(name, _)| name == method)
                .map_or("token", |(_, credential)| *credential);
            operations.insert(verb.to_string(), operation(&doc, credential));
        }
        if !operations.is_empty() {
            paths.insert(doc.display.clone(), Value::Object(operations));
        }
    }
    Json(json!({
        "openapi": OPENAPI_VERSION,
        "info": {
            "title": "Jeryu API",
            "version": crate::github::JERYU_API_VERSION,
            "description": "The primary Jeryu API. The GitHub-compatible edge is \
                            described at /api/v1/docs/rest; error bodies share one \
                            envelope, published at /api/v1/errors.",
        },
        "servers": [{ "url": "/" }],
        "components": {
            "securitySchemes": {
                "bearer": { "type": "http", "scheme": "bearer" },
                "basic": { "type": "http", "scheme": "basic" },
            },
            "schemas": { "Error": error_schema() },
        },
        "paths": Value::Object(paths),
    }))
}

fn operation(doc: &RouteDoc, credential: &str) -> Value {
    let parameters = doc
        .path_params
        .iter()
        .map(|name| {
            json!({ "name": name, "in": "path", "required": true,
                            "schema": { "type": "string" } })
        })
        .chain(doc.query_params.iter().map(|name| {
            json!({ "name": name, "in": "query", "required": false,
                    "schema": { "type": "string" } })
        }))
        .collect::<Vec<_>>();
    let security = if credential == "none" {
        json!([])
    } else {
        json!([{ "bearer": [] }, { "basic": [] }])
    };
    json!({
        "operationId": operation_id(doc),
        "parameters": parameters,
        "security": security,
        "x-jeryu-credential": credential,
        "responses": {
            "200": { "description": "the request succeeded" },
            "4XX": {
                "description": "the shared jeryu error envelope",
                "content": { "application/json": {
                    "schema": { "$ref": "#/components/schemas/Error" },
                } },
            },
        },
    })
}

/// The OpenAPI key for an HTTP method, or `None` for the `ANY` routes the
/// probe cannot enumerate (the GitHub edge forwards, which the REST document
/// describes instead).
fn openapi_verb(method: &str) -> Option<&'static str> {
    match method {
        "GET" => Some("get"),
        "POST" => Some("post"),
        "PUT" => Some("put"),
        "PATCH" => Some("patch"),
        "DELETE" => Some("delete"),
        _ => None,
    }
}

fn operation_id(doc: &RouteDoc) -> String {
    doc.display
        .trim_matches('/')
        .replace(['/', '{', '}', '.', '*'], "-")
        .replace("--", "-")
}

/// The envelope every `/api/v1` error answers with
/// ([`super::error_envelope`]); `GET /api/v1/errors` publishes the codes.
fn error_schema() -> Value {
    let properties = super::error_envelope::ENVELOPE_FIELDS
        .iter()
        .map(|field| {
            let schema = if *field == "common_fixes" {
                json!({ "type": "array", "items": { "type": "string" } })
            } else {
                json!({ "type": "string" })
            };
            ((*field).to_string(), schema)
        })
        .collect::<Map<String, Value>>();
    json!({
        "type": "object",
        "required": super::error_envelope::ENVELOPE_FIELDS,
        "properties": Value::Object(properties),
    })
}

fn described(doc: &RouteDoc) -> Value {
    json!({
        "path": doc.display,
        "methods": doc.methods,
        "path_params": doc.path_params,
        "query_params": doc.query_params,
        "auth": doc
            .auth
            .iter()
            .map(|(method, credential)| (method.clone(), json!(credential)))
            .collect::<Map<String, Value>>(),
    })
}

/// Every mounted route, described.
async fn describe(state: &Arc<WebState>) -> Vec<RouteDoc> {
    let mut docs = Vec::new();
    for (path, handlers) in super::api_v1_routes() {
        let methods = route_methods(state, path, handlers).await;
        let probe = probe_uri(path);
        let auth = methods
            .iter()
            .map(|method| (method.clone(), credential(method, &probe)))
            .collect();
        docs.push(RouteDoc {
            display: display_path(path),
            methods,
            path_params: path_params(path),
            query_params: query_params(path),
            auth,
        });
    }
    docs
}

/// What a caller needs to call `method` on this route: nothing, any login, or
/// the global admin role. Read from the gate's own policy, so the index cannot
/// promise access the gate refuses.
fn credential(method: &str, probe: &str) -> &'static str {
    if super::auth::open_path(probe) {
        return "none";
    }
    let Ok(method) = Method::from_bytes(method.as_bytes()) else {
        return "token";
    };
    if super::auth::admin_only_request(&method, probe) {
        "admin"
    } else {
        "token"
    }
}

/// The query keys a route reads, declared by the type its handler validates
/// the query with. A route that takes no query has none.
fn query_params(path: &str) -> &'static [&'static str] {
    const PAGE_KEYS: &[&str] = &["limit", "per_page", "page"];
    match path {
        "/api/v1/attention" => keys::<super::pipeline::attention::AttentionQuery>(),
        "/api/v1/events" => keys::<super::pipeline::EventsQuery>(),
        "/api/v1/merge-queue" | "/api/v1/repos/:id/merge-queue" => {
            keys::<super::merge_queue::QueueListQuery>()
        }
        "/api/v1/shift/todos" => keys::<super::shift::TodosQuery>(),
        "/api/v1/shift/workers" => keys::<super::shift::WorkersQuery>(),
        "/api/v1/search" => &["q"],
        "/api/v1/audit" | "/api/v1/agent-runs" => PAGE_KEYS,
        _ => &[],
    }
}

fn keys<T: StrictFields>() -> &'static [&'static str] {
    T::KEYS
}

/// The parameters a route's path takes, in the order they appear.
fn path_params(path: &str) -> Vec<String> {
    path.split('/')
        .filter_map(|segment| segment.strip_prefix([':', '*']).map(ToString::to_string))
        .collect()
}

/// The methods one route serves, `HEAD` left out because axum answers it for
/// every `GET`.
async fn route_methods(
    state: &Arc<WebState>,
    path: &'static str,
    handlers: MethodRouter<Arc<WebState>>,
) -> Vec<String> {
    let probe_method = Method::from_bytes(b"JERYU-ROUTE-INDEX").expect("valid method token");
    let router = axum::Router::new()
        .route(path, handlers)
        .with_state(state.clone());
    let request = Request::builder()
        .method(probe_method)
        .uri(probe_uri(path))
        .body(Body::empty())
        .expect("probe request");
    let Ok(response) = router.oneshot(request).await;
    if response.status() != StatusCode::METHOD_NOT_ALLOWED {
        return vec!["ANY".to_string()];
    }
    response
        .headers()
        .get(header::ALLOW)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|method| !method.is_empty() && *method != "HEAD")
        .map(ToString::to_string)
        .collect()
}

/// `/api/v1/repos/:id` as `/api/v1/repos/{id}`, the shape the `/api/v3`
/// index uses.
fn display_path(path: &str) -> String {
    path.split('/')
        .map(|segment| match segment.strip_prefix([':', '*']) {
            Some(name) => format!("{{{name}}}"),
            None => segment.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A concrete path the route pattern matches.
fn probe_uri(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with([':', '*']) {
                "x"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_path_uses_brace_params() {
        assert_eq!(
            display_path("/api/v1/repos/:id/pulls/:number"),
            "/api/v1/repos/{id}/pulls/{number}"
        );
        assert_eq!(probe_uri("/api/v1/work/:key"), "/api/v1/work/x");
        assert_eq!(
            path_params("/api/v1/repos/:id/pulls/:number"),
            vec!["id".to_string(), "number".to_string()]
        );
    }

    /// Every path the query table names has to be a route the router mounts,
    /// or the index would publish parameters for a path nobody can call.
    #[test]
    fn the_query_table_only_names_mounted_routes() {
        for path in [
            "/api/v1/attention",
            "/api/v1/events",
            "/api/v1/merge-queue",
            "/api/v1/repos/:id/merge-queue",
            "/api/v1/shift/todos",
            "/api/v1/shift/workers",
            "/api/v1/search",
            "/api/v1/audit",
            "/api/v1/agent-runs",
        ] {
            assert!(
                !query_params(path).is_empty(),
                "{path} is in the query table"
            );
            assert!(
                super::super::api_v1_routes()
                    .iter()
                    .any(|(mounted, _)| *mounted == path),
                "{path} is not mounted"
            );
        }
    }

    #[test]
    fn the_credential_comes_from_the_gates_own_policy() {
        assert_eq!(
            credential("GET", crate::discovery::CAPABILITIES_PATH),
            "none"
        );
        assert_eq!(credential("GET", "/api/v1/errors"), "none");
        assert_eq!(credential("GET", "/api/v1/attention"), "admin");
        assert_eq!(credential("GET", "/api/v1/work/x"), "token");
    }
}
