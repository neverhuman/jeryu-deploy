//! Pipeline visibility: the append-only event log (`/api/v1/events`), its
//! volume over time (`/api/v1/events/histogram`), the attention inbox
//! (`/api/v1/attention`) and one piece of work's trace from todo to
//! deployment (`/api/v1/trace`). Contract: `docs/pipeline-events.md`.
//!
//! Every lifecycle step (todo claimed, gate finished, review posted, queued,
//! merged, staged, deployed) becomes one row in `<data_dir>/shift.sqlite` and
//! one frame on the admin-only `pipeline` WebSocket scope. Producers outside
//! the forge post with `POST /api/v1/events`; the forge emits its own through
//! [`emit`], which never fails the request it rides on.

pub(crate) mod attention;
pub(crate) mod emit;
pub(crate) mod estimate;
pub(crate) mod histogram;
pub(crate) mod main_health;
pub(crate) mod pins;
mod store;
pub(crate) mod trace;
pub(crate) mod trust;
mod types;

#[cfg(test)]
mod attention_tests;
#[cfg(test)]
mod emit_tests;
#[cfg(test)]
mod histogram_tests;
#[cfg(test)]
mod kinds;
#[cfg(test)]
mod kinds_tests;
#[cfg(test)]
mod ledger_tests;
#[cfg(test)]
mod main_health_tests;
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
use store::Inserted;
pub(crate) use store::RETENTION_MS as EVENT_RETENTION_MS;
pub(crate) use store::{EventStore, LedgerRow, SpendRow};
/// The most events one `POST /api/v1/events` takes; other ingestion routes cap to it.
pub(crate) use types::MAX_BATCH as MAX_EVENT_BATCH;
use types::{EVENTS_SCHEMA, EventsResponse, MAX_BATCH, normalize};
pub(crate) use types::{Event, EventsQuery, NewEvent};

/// WebSocket scope every stored event is published on (admin-only).
pub(crate) const PIPELINE_SCOPE: &str = "pipeline";
const REPORTERS_ENV: &str = "JERYU_EVENT_REPORTERS";
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
/// (`JERYU_EVENT_REPORTERS`, comma-separated). No default: a reporter login is
/// one installation's identity, and this source is public. Unset, only global
/// admins may post.
fn reporters() -> &'static [String] {
    EVENT_REPORTERS.get_or_init(|| reporter_list(&std::env::var(REPORTERS_ENV).unwrap_or_default()))
}

static EVENT_REPORTERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Seed the reporter list without reading the environment, so a test states
/// the site it is testing instead of depending on one.
#[cfg(test)]
pub(crate) fn configure_reporters_for_tests(logins: &[&str]) {
    let _ = EVENT_REPORTERS.set(logins.iter().map(|login| (*login).to_owned()).collect());
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

/// Whether `account` may post events: a global admin or a named reporter.
pub(crate) fn may_post(account: &AccountSummary) -> bool {
    may_report(account, reporters())
}

/// Store one event and fan it out on the `pipeline` scope. A repeat of an
/// `event_id` the reporter already stored writes and publishes nothing.
pub(crate) fn record(
    state: &WebState,
    reporter: &str,
    event: NewEvent,
) -> Result<Inserted, String> {
    let mut event = normalize(event)?;
    // The projections read some kinds as the forge's own record of what it
    // did (`trust`). The routes refuse such an event with a 403 of their own;
    // this keeps the invariant for every other path into the store.
    if reporter != trust::FORGE && trust::forge_only(&event.kind) {
        return Err(format!("{}: only the forge stores this kind", event.kind));
    }
    derive_family(state, &mut event);
    let now_ms = Utc::now().timestamp_millis();
    let inserted = state.events.insert(reporter, &event, now_ms)?;
    if !inserted.duplicate {
        publish(state, &inserted.event);
        record_usage(state, &inserted.event, now_ms);
        // An incident blames changes: each is a defect past review.
        crate::web::review_catch::link_incident(state, &inserted.event);
    }
    Ok(inserted)
}

/// Fill `family` from `repo` when the producer named no family.
///
/// Only the queue and shift emitters tag their own events, so every `gate.*`,
/// `review.*`, `deploy.*`, `release.*` and `pin.*` row landed with `family`
/// null and then vanished from the Activity page's family filter: a repo of
/// the family had events, but `?family=<x>&kind=gate.` answered none of them.
/// The repo is enough to know the family, so the server resolves it through
/// the same registry the repository family pills read
/// ([`crate::web::repositories::family_for_slug`]). A producer that does name
/// a family keeps it; a repo the forge hosts in no family stays null.
fn derive_family(state: &WebState, event: &mut NewEvent) {
    if event.family.is_some() {
        return;
    }
    if let Some(repo) = event.repo.as_deref() {
        event.family = crate::web::repositories::family_for_slug(state, repo);
    }
}

/// A finished attempt may carry the attempt's final token totals as
/// `detail.usage` (flat, or todoq's per-model object summed over its models;
/// see [`crate::web::usage::attempt_usage`]); store them as that slot's
/// newest usage sample, so the
/// Activity chart counts the tail of an attempt whose last heartbeat went out
/// before it ended. The slot comes from the event's `actor`.
fn record_usage(state: &WebState, event: &Event, now_ms: i64) {
    if event.kind != types::ATTEMPT_FINISHED_KIND {
        return;
    }
    let Some(usage) = event
        .detail
        .as_ref()
        .and_then(|detail| detail.get("usage"))
        .and_then(crate::web::usage::attempt_usage)
    else {
        return;
    };
    let (operator, host, slot) = crate::web::usage::actor_slot(event.actor.as_deref());
    crate::web::usage::record(
        state,
        crate::web::usage::Sample {
            ts_ms: now_ms,
            operator,
            host,
            slot,
            family: event.family.clone().unwrap_or_default(),
            todo_id: event.todo_id.clone(),
            usage,
        },
    );
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
    // Usage samples and ad-hoc session spend are charged to the operator the
    // actor names, so a reporter may only charge its own.
    if account.role != UserRole::Admin
        && events.iter().any(|event| {
            (event.kind == crate::web::yields::SESSION_KIND
                || event
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.get("usage").is_some()))
                && crate::web::usage::actor_slot(event.actor.as_deref()).0 != account.login
        })
    {
        return events_write_error(
            StatusCode::FORBIDDEN,
            "events_reporter_required",
            "usage samples and session spend must name the authenticated reporter as their operator",
            "report usage for your own operator or use an administrator identity",
        );
    }
    // The whole batch is refused before anything is stored when it carries an
    // event this account may not write (`trust`).
    for (index, event) in events.iter().enumerate() {
        if let Some(refused) = trust::refusal(&account, &event.kind) {
            return events_write_error(
                StatusCode::FORBIDDEN,
                refused.code,
                &format!("events[{index}]: {}", refused.reason),
                refused.hint,
            );
        }
    }
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
