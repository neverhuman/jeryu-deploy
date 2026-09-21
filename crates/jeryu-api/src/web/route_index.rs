//! `GET /api/v1`: the route index for the primary API, the counterpart of
//! the `jeryu_api_routes` list the `/api/v3` edge already answers with.
//!
//! The index is read from the same route list the router mounts
//! ([`super::api_v1_routes`]); each route's methods are read by asking its
//! handlers for a method none of them serve, which makes axum answer 405 with
//! the `Allow` header it built from the registered handlers.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::routing::MethodRouter;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::WebState;

/// Where the index lives; the unknown-route 404 points callers here.
pub(super) const INDEX_PATH: &str = "/api/v1";

pub(super) async fn api_v1(State(state): State<Arc<WebState>>) -> Json<Value> {
    let mut routes = vec![format!("GET {INDEX_PATH}")];
    for (path, handlers) in super::api_v1_routes() {
        let display = display_path(path);
        for method in route_methods(&state, path, handlers).await {
            routes.push(format!("{method} {display}"));
        }
    }
    Json(json!({
        "jeryu_api_routes": routes,
        "errors": "/api/v1/errors",
        "github_compatible_edge": "/api/v3",
    }))
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
    }
}
