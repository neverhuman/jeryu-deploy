use std::collections::BTreeSet;
use std::sync::Arc;

use axum::extract::rejection::QueryRejection;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use jeryu_core::{AccountSummary, UserRole};
use jeryu_readmodel::Bottleneck;
use jeryu_readmodel::contracts::{ServerWsMessage, WebEvent};
use serde_json::{Value, json};

use super::pipeline::PIPELINE_SCOPE;
use super::surface::serialize_payload;
use super::workcells_support::{TypedError, typed_error};
use super::{WebState, server_time, workcells};

const DOCS: &str = "docs/pipeline-events.md#websocket";

/// Connect-time cursor: `GET /api/v1/ws?after_seq=<seq>` (or `since=`) replays
/// the stored pipeline events after `<seq>` when the `pipeline` scope is
/// subscribed, so a reconnecting client misses nothing.
#[derive(Debug, Default, serde::Deserialize)]
pub(super) struct WsQuery {
    after_seq: Option<i64>,
    since: Option<i64>,
}

fn ws_error(status: StatusCode, code: &str, reason: &str, hint: &str) -> Response {
    typed_error(TypedError {
        status,
        code,
        purpose: "open the jeryu WebSocket",
        reason,
        common_fixes: &[
            "connect with a WebSocket client (Upgrade: websocket), not a plain GET",
            "pass the durable cursor as ?after_seq=<last payload.seq you saw>",
        ],
        docs_url: DOCS,
        repair_hint: hint,
        message: reason,
    })
}

pub(super) async fn ws(
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    query: Result<Query<WsQuery>, QueryRejection>,
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(rejection) => {
            return ws_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ws_invalid_query",
                &rejection.body_text(),
                "after_seq and since are integers",
            );
        }
    };
    let cursor = match (query.after_seq, query.since) {
        (Some(after), Some(since)) if after != since => {
            return ws_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "ws_invalid_query",
                &format!(
                    "since ({since}) and after_seq ({after}) disagree; they name the same cursor"
                ),
                "send one cursor: after_seq=<last seq you saw>",
            );
        }
        (after, since) => after.or(since),
    };
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => {
            return ws_error(
                StatusCode::UPGRADE_REQUIRED,
                "websocket_upgrade_required",
                &format!(
                    "/api/v1/ws only speaks WebSocket: {}",
                    rejection.body_text()
                ),
                "open it with a WebSocket client; for a plain HTTP read use GET /api/v1/events",
            );
        }
    };
    ws.on_upgrade(move |socket| handle_ws(socket, state, account, cursor))
        .into_response()
}

async fn handle_ws(
    mut socket: WebSocket,
    state: Arc<WebState>,
    account: AccountSummary,
    mut connect_cursor: Option<i64>,
) {
    // The hub queues producer events (`WsHub::publish`) onto this channel;
    // the select! loop below drains it onto the socket. One owner for every
    // write keeps snapshot-on-subscribe and pushed deltas strictly ordered.
    let (push_tx, mut push_rx) = tokio::sync::mpsc::unbounded_channel::<ServerWsMessage>();
    let conn_id = state.ws.register(push_tx);
    let _ = send_server_message(&mut socket, hello_message(&state)).await;
    // Per-connection scope subscription set, mirrored into the hub registry.
    let mut scopes: BTreeSet<String> = BTreeSet::new();
    loop {
        let message = tokio::select! {
            pushed = push_rx.recv() => {
                let Some(frame) = pushed else { break };
                if send_server_message(&mut socket, frame).await.is_err() {
                    break;
                }
                continue;
            }
            inbound = socket.next() => {
                let Some(message) = inbound else { break };
                message
            }
        };
        match message {
            Ok(Message::Text(text)) => {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    match value.get("type").and_then(Value::as_str) {
                        Some("ping") => {
                            let nonce = match value.get("nonce").and_then(Value::as_str) {
                                Some(nonce) => nonce.to_string(),
                                None => String::new(),
                            };
                            let _ = send_server_message(
                                &mut socket,
                                ServerWsMessage::Pong {
                                    nonce,
                                    server_time: server_time(),
                                },
                            )
                            .await;
                        }
                        Some("hello") => {
                            // A `hello` may carry an initial subscription set.
                            for scope in requested_scopes(&value) {
                                if authorize_scope(&state, &account, &scope) {
                                    scopes.insert(scope);
                                } else {
                                    send_scope_denied(&mut socket, &scope).await;
                                }
                            }
                            state.ws.set_scopes(conn_id, &scopes);
                            let _ = send_server_message(&mut socket, hello_message(&state)).await;
                            send_scope_snapshots(&mut socket, &state, &scopes).await;
                            if scopes.contains(PIPELINE_SCOPE) {
                                let cursor = pipeline_cursor(&value).or(connect_cursor.take());
                                send_pipeline_replay(&mut socket, &state, cursor).await;
                            }
                        }
                        Some("subscribe") => {
                            // Track the newly requested scopes and immediately push
                            // a snapshot Event frame for each, so the client paints
                            // from live read-model data without waiting for a delta.
                            let added: Vec<String> = requested_scopes(&value);
                            let mut authorized = Vec::new();
                            for scope in &added {
                                if authorize_scope(&state, &account, scope) {
                                    scopes.insert(scope.clone());
                                    authorized.push(scope.clone());
                                } else {
                                    send_scope_denied(&mut socket, scope).await;
                                }
                            }
                            state.ws.set_scopes(conn_id, &scopes);
                            let snapshot_scopes: BTreeSet<String> =
                                authorized.into_iter().collect();
                            send_scope_snapshots(&mut socket, &state, &snapshot_scopes).await;
                            if snapshot_scopes.contains(PIPELINE_SCOPE) {
                                let cursor = pipeline_cursor(&value).or(connect_cursor.take());
                                send_pipeline_replay(&mut socket, &state, cursor).await;
                            }
                        }
                        Some("unsubscribe") => {
                            let dropped = unsubscribe_scopes(&value);
                            for scope in &dropped {
                                scopes.remove(scope);
                            }
                            state.ws.remove_scopes(conn_id, &dropped);
                        }
                        Some("ack") => {}
                        _ => {
                            let _ = send_server_message(
                                &mut socket,
                                ServerWsMessage::Error {
                                    code: "unknown_message".to_string(),
                                    message: "unsupported websocket message type".to_string(),
                                },
                            )
                            .await;
                        }
                    }
                }
            }
            Ok(Message::Ping(payload)) => {
                let _ = socket.send(Message::Pong(payload)).await;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
    state.ws.unregister(conn_id);
}

/// Extract subscription scopes from a `hello`/`subscribe` frame. Both carry
/// `subscriptions: [{ scope, filters }]` per the [`ClientWsMessage`] contract.
pub(super) fn requested_scopes(value: &Value) -> Vec<String> {
    match value.get("subscriptions").and_then(Value::as_array) {
        Some(specs) => specs
            .iter()
            .filter_map(|spec| spec.get("scope").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
        None => Vec::new(),
    }
}

/// The `after_seq` filter of a `pipeline` subscription in a `hello`/`subscribe`
/// frame: `{"scope": "pipeline", "filters": {"after_seq": 41}}`.
pub(super) fn pipeline_cursor(value: &Value) -> Option<i64> {
    value
        .get("subscriptions")?
        .as_array()?
        .iter()
        .filter(|spec| spec.get("scope").and_then(Value::as_str) == Some(PIPELINE_SCOPE))
        .find_map(|spec| spec.get("filters")?.get("after_seq")?.as_i64())
}

/// Replay the stored pipeline events after `cursor`, oldest first. No cursor
/// means no replay: the client only asked for new events.
async fn send_pipeline_replay(socket: &mut WebSocket, state: &WebState, cursor: Option<i64>) {
    let Some(cursor) = cursor else { return };
    match super::pipeline::replay(state, cursor) {
        Ok(events) => {
            for event in events {
                let _ = send_server_message(socket, ServerWsMessage::Event { event }).await;
            }
        }
        Err(reason) => {
            let _ = send_server_message(
                socket,
                ServerWsMessage::Error {
                    code: "events_store_failed".to_string(),
                    message: format!("pipeline replay failed: {reason}"),
                },
            )
            .await;
        }
    }
}

/// Extract the scope list from an `unsubscribe` frame (`scopes: [..]`).
pub(super) fn unsubscribe_scopes(value: &Value) -> Vec<String> {
    match value.get("scopes").and_then(Value::as_array) {
        Some(scopes) => scopes
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        None => Vec::new(),
    }
}

/// Push one snapshot [`ServerWsMessage::Event`] frame per subscribed scope, each
/// stamped with a fresh monotonic sequence from the hub.
async fn send_scope_snapshots(socket: &mut WebSocket, state: &WebState, scopes: &BTreeSet<String>) {
    for scope in scopes {
        if let Some(event) = snapshot_event(state, scope) {
            let _ = send_server_message(socket, ServerWsMessage::Event { event }).await;
        }
    }
}

/// Build a snapshot [`WebEvent`] for a subscribed scope from the read model.
///
/// Supported scopes: `global.activity` (server-wide pool totals + bottlenecks),
/// `pool.{name}` (one pool's rollup), and `system.health` (component health).
/// Unknown scopes yield `None` and are silently ignored (no spurious frame).
pub(super) fn snapshot_event(state: &WebState, scope: &str) -> Option<WebEvent> {
    let activity = &state.tui.pool_activity;
    let seq = state.ws.next_seq();
    let timestamp = server_time();
    if scope == "global.activity" {
        let totals = activity.totals();
        let bottlenecks: Vec<String> = activity
            .bottlenecks()
            .iter()
            .map(Bottleneck::describe)
            .collect();
        return Some(WebEvent {
            seq,
            timestamp,
            scope: scope.to_string(),
            kind: "activity.snapshot".to_string(),
            entity: "global".to_string(),
            summary: format!(
                "{} queued / {} running / {} failed across {} pool(s)",
                totals.queued_jobs, totals.running_jobs, totals.failed_jobs, totals.pools
            ),
            payload: json!({
                "health": activity.health(),
                "totals": totals,
                "bottlenecks": bottlenecks,
            }),
        });
    }
    if let Some(pool_name) = scope.strip_prefix("pool.") {
        let pool = activity.pools.iter().find(|p| p.pool == pool_name)?;
        let payload = serialize_payload(pool).ok()?;
        return Some(WebEvent {
            seq,
            timestamp,
            scope: scope.to_string(),
            kind: "pool.snapshot".to_string(),
            entity: pool.pool.clone(),
            summary: format!(
                "pool '{}': {} queued / {} running, {:.0}% utilized",
                pool.pool,
                pool.queued_jobs,
                pool.running_jobs,
                pool.utilization() * 100.0
            ),
            payload,
        });
    }
    if scope == "system.health" {
        let system = &state.tui.system;
        let payload = serialize_payload(system).ok()?;
        return Some(WebEvent {
            seq,
            timestamp,
            scope: scope.to_string(),
            kind: "system.snapshot".to_string(),
            entity: "system".to_string(),
            summary: "system component health snapshot".to_string(),
            payload,
        });
    }
    if let Some(workcell_id) = scope.strip_prefix("workcell.") {
        return workcells::snapshot_event(state, workcell_id);
    }
    if let Some(agent_run_id) = scope.strip_prefix("agent_run.") {
        return super::agent_runs::snapshot_event(state, agent_run_id);
    }
    if scope == super::tool_finder::SCAN_SCOPE {
        return Some(super::tool_finder::snapshot_event(state, seq, timestamp));
    }
    None
}

pub(super) fn authorize_scope(state: &WebState, account: &AccountSummary, scope: &str) -> bool {
    if account.role == UserRole::Admin {
        return known_scope(scope);
    }
    if let Some(rest) = scope.strip_prefix("repo.") {
        let mut parts = rest.splitn(2, '.');
        if let (Some(owner), Some(repo)) = (parts.next(), parts.next()) {
            return state.core.user_can_read_repo(&account.login, owner, repo);
        }
    }
    false
}

fn known_scope(scope: &str) -> bool {
    scope == "global.activity"
        || scope == "system.health"
        || scope == super::tool_finder::SCAN_SCOPE
        || scope == super::pipeline::PIPELINE_SCOPE
        || scope.starts_with("pool.")
        || scope.starts_with("workcell.")
        || scope.starts_with("agent_run.")
        || scope.starts_with("repo.")
}

async fn send_scope_denied(socket: &mut WebSocket, scope: &str) {
    let _ = send_server_message(
        socket,
        ServerWsMessage::Error {
            code: "subscription_denied".to_string(),
            message: format!("not authorized for websocket scope {scope}"),
        },
    )
    .await;
}

async fn send_server_message(socket: &mut WebSocket, message: ServerWsMessage) -> Result<(), ()> {
    let encoded = serde_json::to_string(&message).map_err(|_| ())?;
    socket.send(Message::Text(encoded)).await.map_err(|_| ())
}

pub(super) fn hello_message(state: &WebState) -> ServerWsMessage {
    ServerWsMessage::Hello {
        server_time: server_time(),
        current_seq: state.ws.current_seq(),
        protocol: super::WS_PROTOCOL.to_string(),
    }
}
