//! The HTTP shape of the gateway hop. Mount this only on the mTLS listener
//! that enrolled nodes reach, never on the public API.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;

use crate::Gateway;

pub fn router(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route("/internal/gateway/:session_id/:provider", post(relay))
        .with_state(gateway)
}

async fn relay(
    State(gateway): State<Arc<Gateway>>,
    Path((session_id, provider)): Path<(String, String)>,
    body: Body,
) -> Response {
    if !crate::frames::valid_id(&session_id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let lines = gateway.relay(&session_id, &provider, body.into_data_stream());
    let stream = futures_util::StreamExt::map(lines, Ok::<_, std::convert::Infallible>);
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(stream),
    )
        .into_response()
}
