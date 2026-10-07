//! `jeryu-mock-provider`: a loopback-only stand-in for provider OAuth and chat
//! endpoints, for connect-once CI (lane A `mock-provider-v1`). It has no HTTP
//! client and no resolver, binds only `127.0.0.1`, and must never be linked
//! into a product crate (see `tests/policy.rs`).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

mod state;

pub use state::{Counters, Event, EventKind, MockConfig, MockState, Refusal};

/// A running mock bound to an ephemeral loopback port.
pub struct MockProvider {
    pub addr: SocketAddr,
    pub state: Arc<MockState>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MockProvider {
    /// Bind `127.0.0.1:0` and serve until dropped.
    pub async fn start(config: MockConfig) -> std::io::Result<Self> {
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
        let addr = listener.local_addr()?;
        if !addr.ip().is_loopback() {
            return Err(std::io::Error::other("mock_provider_requires_loopback"));
        }
        let state = Arc::new(MockState::new(config));
        let app = router(state.clone());
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await;
        });
        Ok(Self {
            addr,
            state,
            shutdown: Some(shutdown),
        })
    }

    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

pub fn router(state: Arc<MockState>) -> Router {
    Router::new()
        .route("/oauth/device", post(device))
        .route("/oauth/device/approve", post(approve))
        .route("/oauth/token", post(token))
        .route("/sessions", post(session))
        .route("/connections/:id/disconnect", post(disconnect))
        .route("/connections/:id/replace", post(replace))
        .route("/v1/messages", post(chat))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/responses", post(chat))
        .route("/counters", get(counters))
        .route("/events", get(events))
        .with_state(state)
}

#[derive(Deserialize)]
struct DeviceRequest {
    provider: String,
    #[serde(default)]
    provider_account: String,
}

#[derive(Deserialize)]
struct ApproveRequest {
    user_code: String,
}

/// OAuth token requests arrive form-encoded, as real device flows send them.
#[derive(Deserialize)]
struct TokenRequest {
    grant_type: String,
    #[serde(default)]
    device_code: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    connection_id: Option<String>,
}

#[derive(Deserialize)]
struct SessionRequest {
    connection_id: String,
    connection_generation: u64,
}

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

async fn device(
    State(state): State<Arc<MockState>>,
    Json(request): Json<DeviceRequest>,
) -> Response {
    match state.start_device(&request.provider, &request.provider_account) {
        Ok(grant) => Json(json!({
            "device_code": grant.device_code,
            "user_code": grant.user_code,
            "interval": 1,
            "expires_in": grant.expires_in,
            "verification_uri": "http://127.0.0.1/mock-device",
        }))
        .into_response(),
        Err(refusal) => refused(refusal),
    }
}

async fn approve(
    State(state): State<Arc<MockState>>,
    Json(request): Json<ApproveRequest>,
) -> Response {
    match state.approve(&request.user_code) {
        Ok(()) => Json(json!({"approved": true})).into_response(),
        Err(refusal) => refused(refusal),
    }
}

async fn token(
    State(state): State<Arc<MockState>>,
    axum::Form(request): axum::Form<TokenRequest>,
) -> Response {
    let grant = match (
        request.grant_type.as_str(),
        request.device_code,
        request.refresh_token,
    ) {
        (DEVICE_GRANT, Some(device_code), _) => state.device_token(&device_code),
        ("refresh_token", _, Some(refresh)) => match request.connection_id {
            Some(connection_id) => state.refresh(&connection_id, &refresh),
            None => Err(Refusal::InvalidRequest),
        },
        _ => Err(Refusal::InvalidRequest),
    };
    match grant {
        Ok(grant) => Json(json!({
            "access_token": grant.access_token,
            "refresh_token": grant.refresh_token,
            "token_type": "Bearer",
            "expires_in": grant.expires_in,
            "connection_id": grant.connection_id,
            "connection_generation": grant.connection_generation,
            "secret_version": grant.secret_version,
        }))
        .into_response(),
        Err(refusal) => refused(refusal),
    }
}

async fn session(
    State(state): State<Arc<MockState>>,
    Json(request): Json<SessionRequest>,
) -> Response {
    match state.lease_session(&request.connection_id, request.connection_generation) {
        Ok(lease) => Json(json!({
            "session_id": lease.session_id,
            "access_token": lease.access_token,
            "expires_in": lease.expires_in,
        }))
        .into_response(),
        Err(refusal) => refused(refusal),
    }
}

/// Mock-only: replace the credential on an existing connection.
async fn replace(State(state): State<Arc<MockState>>, Path(id): Path<String>) -> Response {
    match state.replace(&id) {
        Ok(grant) => Json(json!({
            "access_token": grant.access_token,
            "refresh_token": grant.refresh_token,
            "connection_generation": grant.connection_generation,
            "secret_version": grant.secret_version,
        }))
        .into_response(),
        Err(refusal) => refused(refusal),
    }
}

async fn disconnect(State(state): State<Arc<MockState>>, Path(id): Path<String>) -> Response {
    match state.disconnect(&id) {
        Ok(generation) => {
            Json(json!({"state": "disconnected", "connection_generation": generation}))
                .into_response()
        }
        Err(refusal) => refused(refusal),
    }
}

/// The mock never adds a credential: a 200 here proves the caller injected a
/// live access token.
async fn chat(State(state): State<Arc<MockState>>, headers: HeaderMap) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    match state.authorize_chat(authorization) {
        Ok(()) => Json(json!({"injected": true})).into_response(),
        Err(refusal) => refused(refusal),
    }
}

async fn counters(State(state): State<Arc<MockState>>) -> Response {
    Json(state.counters()).into_response()
}

async fn events(State(state): State<Arc<MockState>>) -> Response {
    Json(state.events()).into_response()
}

fn refused(refusal: Refusal) -> Response {
    let status = match refusal {
        Refusal::ProviderUnknown
        | Refusal::AuthorizationPending
        | Refusal::ExpiredToken
        | Refusal::InvalidRequest
        | Refusal::InvalidGrant => StatusCode::BAD_REQUEST,
        Refusal::ProviderNotConnected
        | Refusal::ConnectionGenerationStale
        | Refusal::AccessExpired => StatusCode::CONFLICT,
        Refusal::RefreshPersistFailed => StatusCode::SERVICE_UNAVAILABLE,
        Refusal::Unauthorized => StatusCode::UNAUTHORIZED,
    };
    (status, Json(json!({"error": refusal}))).into_response()
}
