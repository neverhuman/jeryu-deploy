//! Pipeline visibility: the append-only event log (`/api/v1/events`) and the
//! attention inbox (`/api/v1/attention`). Contract: `docs/pipeline-events.md`.
//!
//! Every lifecycle step (todo claimed, gate finished, review posted, queued,
//! merged, staged, deployed) becomes one row in `<data_dir>/shift.sqlite` and
//! one frame on the admin-only `pipeline` WebSocket scope. Producers outside
//! the forge post with `POST /api/v1/events`; the forge emits its own through
//! [`emit`], which never fails the request it rides on.

pub(crate) mod attention;
pub(crate) mod emit;
mod store;
mod types;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::Utc;
use jeryu_core::{AccountSummary, UserRole};
use jeryu_readmodel::contracts::WebEvent;
use serde_json::{Value, json};

use super::WebState;
use super::workcells_support::{TypedError, typed_error};
pub(crate) use store::EventStore;
use store::Inserted;
use types::{EVENTS_SCHEMA, EventsResponse, MAX_BATCH, normalize};
pub(crate) use types::{Event, EventsQuery, NewEvent};

/// WebSocket scope every stored event is published on (admin-only).
pub(crate) const PIPELINE_SCOPE: &str = "pipeline";
const REPORTERS_ENV: &str = "JERYU_EVENT_REPORTERS";
const DEFAULT_REPORTERS: &str = "gatebot,pragent";
const DOCS: &str = "docs/pipeline-events.md";

fn events_error(status: StatusCode, code: &str, reason: &str, hint: &str) -> AxumResponse {
    typed_error(TypedError {
        status,
        code,
        purpose: "record or read the pipeline event log",
        reason,
        common_fixes: &[
            "check the event against docs/pipeline-events.md",
            "post as a global admin or a JERYU_EVENT_REPORTERS identity",
        ],
        docs_url: DOCS,
        repair_hint: hint,
        message: reason,
    })
}

/// Logins that may post events without being a global admin
/// (`JERYU_EVENT_REPORTERS`, comma-separated, default `gatebot,pragent`).
fn reporters() -> &'static [String] {
    static REPORTERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    REPORTERS.get_or_init(|| {
        reporter_list(&std::env::var(REPORTERS_ENV).unwrap_or_else(|_| DEFAULT_REPORTERS.into()))
    })
}

fn reporter_list(configured: &str) -> Vec<String> {
    configured
        .split(',')
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .map(str::to_string)
        .collect()
}

/// Posting an event paints the Activity feed every operator reads, so an
/// ordinary account may not: the caller is a global admin or a named reporter.
fn may_report(account: &AccountSummary, reporters: &[String]) -> bool {
    account.role == UserRole::Admin || reporters.iter().any(|login| login == &account.login)
}

/// Store one event and fan it out on the `pipeline` scope. A repeat of an
/// `event_id` the reporter already stored writes and publishes nothing.
pub(crate) fn record(
    state: &WebState,
    reporter: &str,
    event: NewEvent,
) -> Result<Inserted, String> {
    let event = normalize(event)?;
    let inserted = state
        .events
        .insert(reporter, &event, Utc::now().timestamp_millis())?;
    if !inserted.duplicate {
        publish(state, &inserted.event);
    }
    Ok(inserted)
}

fn publish(state: &WebState, event: &Event) {
    let Ok(payload) = serde_json::to_value(event) else {
        return;
    };
    let entity = event
        .todo_id
        .clone()
        .or_else(|| match (&event.repo, event.pr) {
            (Some(repo), Some(pr)) => Some(format!("{repo}#{pr}")),
            (Some(repo), None) => Some(repo.clone()),
            _ => None,
        })
        .unwrap_or_else(|| event.source.clone());
    let (kind, summary, timestamp) = (event.kind.clone(), event.summary.clone(), event.ts.clone());
    state.ws.publish(PIPELINE_SCOPE, move |seq| WebEvent {
        seq,
        timestamp,
        scope: PIPELINE_SCOPE.to_string(),
        kind,
        entity,
        summary,
        payload,
    });
}

/// Record a server-emitted event, best-effort: a full disk or a malformed
/// event is reported on stderr and never reaches the caller's response.
pub(crate) fn emit(state: &WebState, event: NewEvent) {
    let kind = event.kind.clone();
    if let Err(reason) = record(state, "forge", event) {
        eprintln!("jeryu-api pipeline: dropped {kind} event: {reason}");
    }
}

fn events_from_body(body: &Bytes) -> Result<Vec<NewEvent>, String> {
    let value: Value = serde_json::from_slice(body).map_err(|err| err.to_string())?;
    let items = match value {
        Value::Object(mut object) if object.contains_key("events") => {
            match object.remove("events") {
                Some(Value::Array(items)) => items,
                _ => return Err("events: must be an array".to_string()),
            }
        }
        single @ Value::Object(_) => vec![single],
        _ => return Err("send one event object or {\"events\": [...]}".to_string()),
    };
    if items.is_empty() {
        return Err("events: must not be empty".to_string());
    }
    if items.len() > MAX_BATCH {
        return Err(format!("events: at most {MAX_BATCH} per request"));
    }
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            serde_json::from_value::<NewEvent>(item)
                .map_err(|err| err.to_string())
                .and_then(normalize)
                .map_err(|reason| format!("events[{index}]: {reason}"))
        })
        .collect()
}

/// `POST /api/v1/events`: one event, or `{"events": [...]}` (at most 50).
/// The whole batch is validated before any event is stored. Safe to retry for
/// events that carry an `event_id`: a repeat answers with the original `seq`
/// (`201` when anything new was stored, `200` when every event was a repeat).
pub(crate) async fn post_events(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    body: Bytes,
) -> AxumResponse {
    if !may_report(&account, reporters()) {
        return events_error(
            StatusCode::FORBIDDEN,
            "events_reporter_required",
            "this account may not post pipeline events (JERYU_EVENT_REPORTERS)",
            "post with a global-admin token or add the login to JERYU_EVENT_REPORTERS",
        );
    }
    let events = match events_from_body(&body) {
        Ok(events) => events,
        Err(reason) => {
            return events_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "events_invalid_request",
                &reason,
                "fix the field named in the message and retry",
            );
        }
    };
    let mut seqs = Vec::with_capacity(events.len());
    let mut duplicates = 0;
    for event in events {
        match record(&state, &account.login, event) {
            Ok(inserted) => {
                seqs.push(inserted.event.seq);
                duplicates += usize::from(inserted.duplicate);
            }
            Err(reason) => {
                return events_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "events_store_failed",
                    &reason,
                    "check <data_dir>/shift.sqlite",
                );
            }
        }
    }
    let status = if duplicates == seqs.len() {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let body = json!({
        "schema_version": EVENTS_SCHEMA,
        "ok": true,
        "seqs": seqs,
        "duplicates": duplicates,
    });
    (status, Json(body)).into_response()
}

/// `GET /api/v1/events` (admin-only by path, see `auth::admin_only_path`).
pub(crate) async fn list_events(
    State(state): State<Arc<WebState>>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> AxumResponse {
    let query = match query {
        Ok(Query(query)) => query,
        Err(rejection) => {
            return events_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "events_invalid_query",
                &rejection.body_text(),
                "after_seq, before_seq, limit and pr are integers; needs_human is true or false",
            );
        }
    };
    let page = state
        .events
        .query(&query)
        .and_then(|events| Ok((events, state.events.latest_seq()?)));
    match page {
        Ok((events, latest_seq)) => Json(EventsResponse {
            schema_version: EVENTS_SCHEMA,
            events,
            latest_seq,
        })
        .into_response(),
        Err(reason) => events_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "events_store_failed",
            &reason,
            "check <data_dir>/shift.sqlite",
        ),
    }
}
