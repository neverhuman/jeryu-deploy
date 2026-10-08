//! The HTTP shape of the gateway hop. Mount this only on the mTLS listener
//! that enrolled nodes reach, never on the public API.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;

use crate::{Gateway, PeerIdentity};

/// Only [`crate::listener::serve`] mounts this, and it adds the verified
/// [`PeerIdentity`]; a request without one fails phase 0.
pub(crate) fn router(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route("/internal/gateway/:session_id/:provider", post(relay))
        .with_state(gateway)
}

async fn relay(
    State(gateway): State<Arc<Gateway>>,
    peer: Option<Extension<PeerIdentity>>,
    Path((session_id, provider)): Path<(String, String)>,
    body: Body,
) -> Response {
    if !crate::frames::valid_id(&session_id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let peer = peer.map(|Extension(peer)| peer);
    let lines = gateway.relay(peer, &session_id, &provider, body.into_data_stream());
    let stream = futures_util::StreamExt::map(lines, Ok::<_, std::convert::Infallible>);
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(stream),
    )
        .into_response()
}
