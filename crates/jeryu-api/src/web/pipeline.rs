//! Pipeline visibility: the append-only event log (`/api/v1/events`), the
//! attention inbox (`/api/v1/attention`) and one piece of work's trace from
//! todo to deployment (`/api/v1/trace`). Contract: `docs/pipeline-events.md`.
//!
//! Every lifecycle step (todo claimed, gate finished, review posted, queued,
//! merged, staged, deployed) becomes one row in `<data_dir>/shift.sqlite` and
//! one frame on the admin-only `pipeline` WebSocket scope. Producers outside
//! the forge post with `POST /api/v1/events`; the forge emits its own through
//! [`emit`], which never fails the request it rides on.

pub(crate) mod attention;
pub(crate) mod emit;
pub(crate) mod estimate;
pub(crate) mod pins;
mod store;
pub(crate) mod trace;
mod types;

#[cfg(test)]
mod attention_tests;
#[cfg(test)]
mod emit_tests;
#[cfg(test)]
mod pins_tests;
#[cfg(test)]
pub(crate) mod tests;

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, State};
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

/// A refusal of a write to the event log. Its fixes are about the event and
/// who may post one.
fn events_write_error(status: StatusCode, code: &str, reason: &str, hint: &str) -> AxumResponse {
    events_refusal(
        status,
        code,
        "record a pipeline event",
        reason,
        &[
            "check the event against docs/pipeline-events.md",
            "post as a global admin or a JERYU_EVENT_REPORTERS identity",
        ],
        hint,
    )
}

/// A refusal of a read of the event log. A mistyped filter on a `GET` is not
/// repaired by a reporter identity or by rereading the event schema, so the
/// fixes name the query instead of who may post.
fn events_read_error(status: StatusCode, code: &str, reason: &str, hint: &str) -> AxumResponse {
    events_refusal(
        status,
        code,
        "read the pipeline event log",
        reason,
        &[
            "send one cursor (after_seq) and a limit from 1 to 500",
            "check the filter keys and values against GET /api/v1",
        ],
        hint,
    )
}

fn events_refusal(
    status: StatusCode,
    code: &str,
    purpose: &'static str,
    reason: &str,
    common_fixes: &'static [&'static str],
    hint: &str,
) -> AxumResponse {
    typed_error(TypedError {
        status,
        code,
        purpose,
        reason,
        common_fixes,
        docs_url: DOCS,
        repair_hint: hint,
        message: reason,
    })
}

/// A store failure is on the server, not in the request.
fn events_store_error(reason: &str) -> AxumResponse {
    events_refusal(
        StatusCode::INTERNAL_SERVER_ERROR,
        "events_store_failed",
        "record or read the pipeline event log",
        reason,
        &[
            "check that <data_dir>/shift.sqlite is writable and not corrupt",
            "check the server log for the store error this reports",
        ],
        "check <data_dir>/shift.sqlite",
    )
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
    let Some(frame) = frame_parts(event) else {
        return;
    };
    state.ws.publish(PIPELINE_SCOPE, frame);
}

/// The stored events after `after_seq`, oldest first and at most
/// `REPLAY_LIMIT`, as `pipeline` frames stamped with fresh hub sequences: what
/// a WebSocket client that connects with a cursor missed while it was away.
pub(crate) fn replay(state: &WebState, after_seq: i64) -> Result<Vec<WebEvent>, String> {
    let query = EventsQuery {
        after_seq: Some(after_seq),
        limit: Some(REPLAY_LIMIT),
        ..EventsQuery::default()
    };
    Ok(state
        .events
        .query(&query)?
        .iter()
        .filter_map(frame_parts)
        .map(|frame| frame(state.ws.next_seq()))
        .collect())
}

/// Most events one WebSocket replay sends; a client that gets this many
/// continues with `GET /api/v1/events?after_seq=`.
pub(crate) const REPLAY_LIMIT: i64 = store::MAX_LIMIT;

fn frame_parts(event: &Event) -> Option<impl FnOnce(u64) -> WebEvent + Send + 'static> {
    let payload = serde_json::to_value(event).ok()?;
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
    Some(move |seq| WebEvent {
        seq,
        timestamp,
        scope: PIPELINE_SCOPE.to_string(),
        kind,
        entity,
        summary,
        payload,
    })
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
        return events_write_error(
            StatusCode::FORBIDDEN,
            "events_reporter_required",
            "this account may not post pipeline events (JERYU_EVENT_REPORTERS)",
            "post with a global-admin token or add the login to JERYU_EVENT_REPORTERS",
        );
    }
    let events = match events_from_body(&body) {
        Ok(events) => events,
        Err(reason) => {
            return events_write_error(
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
                return events_store_error(&reason);
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
    query: Result<super::strict_query::StrictQuery<EventsQuery>, super::strict_query::QueryRefused>,
) -> AxumResponse {
    let mut query = match query {
        Ok(super::strict_query::StrictQuery(query)) => query,
        // A key this route does not read, or a value it cannot parse: either
        // way the page it would answer is not the page that was asked for.
        Err(refused) => return refused.into_response(),
    };
    match (query.after_seq, query.since) {
        (Some(after), Some(since)) if after != since => {
            return events_read_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "events_invalid_query",
                &format!(
                    "since ({since}) and after_seq ({after}) disagree; they name the same cursor"
                ),
                "send one cursor: after_seq=<last seq you saw>",
            );
        }
        (None, since) => query.after_seq = since,
        _ => {}
    }
    // A family the forge does not know is a typo in the request: answering
    // it with an empty page reads as "nothing happened".
    match super::family::filter(&state, query.family.as_deref()) {
        Ok(family) => query.family = family,
        Err(response) => return *response,
    }
    // `per_page` is the spelling the offset-paged collections take; the log
    // reads it as the same thing, and refuses the two disagreeing.
    match (query.limit, query.per_page) {
        (Some(limit), Some(per_page)) if limit != per_page => {
            return events_read_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "events_invalid_query",
                &format!("limit ({limit}) and per_page ({per_page}) name the same thing; send one"),
                "send one row limit: limit=<1 to 500>",
            );
        }
        (None, per_page) => query.limit = per_page,
        _ => {}
    }
    let limit = query.limit.unwrap_or(store::DEFAULT_LIMIT);
    if !(1..=store::MAX_LIMIT).contains(&limit) {
        return events_read_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "events_invalid_query",
            &format!(
                "limit must be from 1 to {}, got {limit}; it is not clamped",
                store::MAX_LIMIT
            ),
            "send a limit from 1 to 500, or omit it for 100",
        );
    }
    if let Some(kind) = query.kind.as_deref().filter(|kind| !kind.is_empty())
        && !types::valid_kind_filter(kind)
    {
        return events_read_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "events_invalid_query",
            &format!("kind: {kind:?} is neither a kind (todo.claimed) nor a prefix (todo.)"),
            "use a dotted lower-case kind, or a prefix that ends in a dot",
        );
    }
    let page = state
        .events
        .query_page(&query)
        .and_then(|page| Ok((page, state.events.latest_seq()?)));
    match page {
        Ok(((events, has_more), latest_seq)) => Json(EventsResponse {
            schema_version: EVENTS_SCHEMA,
            next_cursor: events.last().map(|event| event.seq),
            events,
            latest_seq,
            limit,
            has_more,
        })
        .into_response(),
        Err(reason) => events_store_error(&reason),
    }
}
