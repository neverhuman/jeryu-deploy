//! Incidents: `incident.*` pipeline events folded into `incident_ledger` and
//! `incident_causes` (`db/migrations/0024_incident_ledger.sql`), and the
//! reads, the report form and the alert-webhook route built on them. Contract:
//! `docs/pipeline-events.md`, "Incidents".
//!
//! Any reporter may post the four kinds through `POST /api/v1/events`: an
//! alert webhook, a person, an agent. A person reaches the same fold through
//! the "Report incident" form ([`report_incident`]) and an alert through
//! [`webhook`], both of which only build the events above.
//! Every event names its incident in
//! `detail.incident_id`. The fold is order-free: a restore posted before the
//! opening, or a cause posted after the incident was closed, lands on the
//! same row, so a late attribution only fills in what was missing.
//!
//! A change named by `incident.caused_by` is *failed* for the change outcome
//! readers (the report's `failed_changes`). An incident with no cause row is reported as
//! `cause_unknown`, and still counts for time to restore.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{DateTime, Utc};
use jeryu_core::AccountSummary;
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::WebState;
use super::pipeline::NewEvent;
use super::strict_query::{StrictFields, StrictQuery};
use super::workcells_support::{TypedError, typed_error};

pub(crate) const OPENED: &str = "incident.opened";
pub(crate) const RESTORED: &str = "incident.restored";
pub(crate) const CAUSED_BY: &str = "incident.caused_by";
pub(crate) const CLOSED: &str = "incident.closed";
pub(crate) const INCIDENTS_SCHEMA: &str = "jeryu.incidents/v1";
const DOCS: &str = "docs/pipeline-events.md";
const MAX_CHANGES: usize = 50;

#[cfg(test)]
mod tests;

/// Whether `kind` is one of the four incident kinds.
pub(crate) fn is_incident_kind(kind: &str) -> bool {
    matches!(kind, OPENED | RESTORED | CAUSED_BY | CLOSED)
}

fn text<'a>(detail: &'a Value, key: &str) -> Option<&'a str> {
    detail
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// `detail.<key>` as an RFC 3339 time in milliseconds; `Err` when present and
/// not a time.
fn time_ms(detail: &Value, key: &str) -> Result<Option<i64>, String> {
    let Some(raw) = text(detail, key) else {
        return Ok(None);
    };
    DateTime::parse_from_rfc3339(raw)
        .map(|at| Some(at.timestamp_millis()))
        .map_err(|_| {
            format!("detail.{key}: expected an RFC 3339 time such as 2026-10-01T12:00:00Z")
        })
}

pub(super) fn changes(detail: &Value) -> Result<Vec<String>, String> {
    let mut keys = Vec::new();
    if let Some(one) = text(detail, "change") {
        keys.push(one.to_string());
    }
    match detail.get("changes") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for item in items {
                match item.as_str().map(str::trim) {
                    Some(key) if !key.is_empty() => keys.push(key.to_string()),
                    _ => return Err("detail.changes: expected change lot keys (strings)".into()),
                }
            }
        }
        Some(_) => return Err("detail.changes: must be an array of change lot keys".into()),
    }
    if keys.len() > MAX_CHANGES {
        return Err(format!("detail.changes: at most {MAX_CHANGES} per event"));
    }
    if keys
        .iter()
        .any(|key| key.chars().count() > 200 || key.chars().any(char::is_control))
    {
        return Err(
            "detail.changes: keys are at most 200 characters, no control characters".into(),
        );
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

/// The incident an event names, or why it does not.
fn incident_id(detail: &Value) -> Result<&str, String> {
    let id =
        text(detail, "incident_id").ok_or("detail.incident_id: required on incident events")?;
    if id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
    {
        return Err("detail.incident_id: at most 64 characters of [A-Za-z0-9._:-]".into());
    }
    Ok(id)
}

/// Refuse an incident event that cannot be folded. Called from event
/// normalisation, so a malformed one is a `422` and nothing is stored.
pub(crate) fn check(kind: &str, detail: Option<&Value>) -> Result<(), String> {
    if !is_incident_kind(kind) {
        return Ok(());
    }
    let empty = json!({});
    let detail = detail.unwrap_or(&empty);
    incident_id(detail)?;
    match kind {
        OPENED => {
            time_ms(detail, "detected_at")?;
        }
        RESTORED => {
            time_ms(detail, "restored_at")?;
        }
        CAUSED_BY => {
            if changes(detail)?.is_empty() {
                return Err("detail.changes: name at least one change lot key".into());
            }
        }
        _ => {
            time_ms(detail, "closed_at")?;
        }
    }
    Ok(())
}

/// Fold one stored incident event into the ledger on `conn`, which the caller
/// holds in a transaction with the event's own insert.
pub(crate) fn apply(
    conn: &Connection,
    reporter: &str,
    event: &NewEvent,
    ts_ms: i64,
    seq: i64,
) -> Result<(), String> {
    if !is_incident_kind(&event.kind) {
        return Ok(());
    }
    let empty = json!({});
    let detail = event.detail.as_ref().unwrap_or(&empty);
    let id = incident_id(detail)?;
    conn.execute(
        "INSERT OR IGNORE INTO incident_ledger (incident_id, reporter, first_seq, last_seq)
         VALUES (?1, ?2, ?3, ?3)",
        params![id, reporter, seq],
    )
    .map_err(|err| err.to_string())?;
    conn.execute(
        "UPDATE incident_ledger SET last_seq = ?2, family = COALESCE(family, ?3)
          WHERE incident_id = ?1",
        params![id, seq, event.family],
    )
    .map_err(|err| err.to_string())?;
    let fail = |err: rusqlite::Error| err.to_string();
    match event.kind.as_str() {
        OPENED => {
            let detected = time_ms(detail, "detected_at")?.unwrap_or(ts_ms);
            conn.execute(
                "UPDATE incident_ledger
                    SET detected_ms = ?2, service = COALESCE(?3, service),
                        severity = COALESCE(?4, severity), summary = ?5
                  WHERE incident_id = ?1",
                params![
                    id,
                    detected,
                    text(detail, "service"),
                    text(detail, "severity"),
                    event.summary
                ],
            )
            .map_err(fail)?;
        }
        RESTORED => {
            let restored = time_ms(detail, "restored_at")?.unwrap_or(ts_ms);
            conn.execute(
                "UPDATE incident_ledger
                    SET restored_ms = ?2, restoring_change = COALESCE(?3, restoring_change),
                        summary = COALESCE(summary, ?4)
                  WHERE incident_id = ?1",
                params![
                    id,
                    restored,
                    text(detail, "restoring_change"),
                    event.summary
                ],
            )
            .map_err(fail)?;
        }
        CAUSED_BY => {
            for key in changes(detail)? {
                conn.execute(
                    "INSERT OR IGNORE INTO incident_causes (incident_id, change_key, reported_ms)
                     VALUES (?1, ?2, ?3)",
                    params![id, key, ts_ms],
                )
                .map_err(fail)?;
            }
        }
        _ => {
            let closed = time_ms(detail, "closed_at")?.unwrap_or(ts_ms);
            conn.execute(
                "UPDATE incident_ledger SET closed_ms = ?2 WHERE incident_id = ?1",
                params![id, closed],
            )
            .map_err(fail)?;
        }
    }
    Ok(())
}

fn rfc3339(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn seconds(from: Option<i64>, to: Option<i64>) -> Option<i64> {
    match (from, to) {
        (Some(from), Some(to)) if to >= from => Some((to - from) / 1000),
        _ => None,
    }
}

/// When a change reached production: the last time its lot left the Deploy
/// station forward, from the flow ledger.
fn deployed_ms(conn: &Connection, change: &str) -> Result<Option<i64>, String> {
    conn.query_row(
        "SELECT MAX(left_ms) FROM flow_ledger
          WHERE lot_id = ?1 AND station = 'deploy' AND direction = 'forward'",
        params![change],
        |row| row.get::<_, Option<i64>>(0),
    )
    .map_err(|err| err.to_string())
}

fn mean(values: &[i64]) -> Option<i64> {
    (!values.is_empty()).then(|| values.iter().sum::<i64>() / values.len() as i64)
}

fn median(values: &mut [i64]) -> Option<i64> {
    values.sort_unstable();
    (!values.is_empty()).then(|| values[values.len() / 2])
}

/// The incident ledger as the report answers it.
/// Days an incidents read covers when the query names none, and at most.
const DEFAULT_DAYS: u32 = 90;
const MAX_DAYS: u32 = 365;

/// The incidents report. `since_ms` bounds it to incidents seen at or after
/// that time (detected, else restored, else closed): the ledgers are never
/// pruned, so the route always passes a window; `None` reads everything.
pub(crate) fn report(
    conn: &Connection,
    family: Option<&str>,
    since_ms: Option<i64>,
) -> Result<Value, String> {
    type Row = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<i64>,
    );
    let rows: Vec<Row> = {
        let mut stmt = conn
            .prepare(
                "SELECT incident_id, family, service, severity, summary, detected_ms,
                        restored_ms, restoring_change, closed_ms
                   FROM incident_ledger
                  WHERE (?1 IS NULL OR family = ?1)
                    AND (?2 IS NULL OR COALESCE(detected_ms, restored_ms, closed_ms) >= ?2)
                  ORDER BY COALESCE(detected_ms, restored_ms, closed_ms) DESC, incident_id",
            )
            .map_err(|err| err.to_string())?;
        let mapped = stmt
            .query_map(params![family, since_ms], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            })
            .map_err(|err| err.to_string())?;
        mapped
            .collect::<Result<_, _>>()
            .map_err(|err| err.to_string())?
    };
    let mut incidents = Vec::with_capacity(rows.len());
    let (mut restore_times, mut detect_times) = (Vec::new(), Vec::new());
    let mut unknown = 0;
    // Changes that caused an incident in this window, read off the causes
    // already loaded rather than a second walk of the whole ledger.
    let mut failed_by_change: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (id, family, service, severity, summary, detected, restored, restoring, closed) in rows {
        let causes: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT change_key FROM incident_causes WHERE incident_id = ?1
                      ORDER BY change_key",
                )
                .map_err(|err| err.to_string())?;
            let mapped = stmt
                .query_map(params![id], |row| row.get(0))
                .map_err(|err| err.to_string())?;
            mapped
                .collect::<Result<_, _>>()
                .map_err(|err| err.to_string())?
        };
        let mut cause_rows = Vec::new();
        for change in &causes {
            failed_by_change
                .entry(change.clone())
                .or_default()
                .push(id.clone());
            let deployed = deployed_ms(conn, change)?;
            let detect = seconds(deployed, detected);
            detect_times.extend(detect);
            cause_rows.push(json!({
                "change": change,
                "outcome": "failed",
                "deployed_at": deployed.map(rfc3339),
                "time_to_detect_seconds": detect,
            }));
        }
        let restore = seconds(detected, restored);
        restore_times.extend(restore);
        let cause_unknown = causes.is_empty();
        unknown += i64::from(cause_unknown);
        let status = if closed.is_some() {
            "closed"
        } else if restored.is_some() {
            "restored"
        } else {
            "open"
        };
        incidents.push(json!({
            "incident_id": id,
            "family": family,
            "service": service,
            "severity": severity,
            "summary": summary,
            "status": status,
            "detected_at": detected.map(rfc3339),
            "restored_at": restored.map(rfc3339),
            "closed_at": closed.map(rfc3339),
            "restoring_change": restoring,
            "time_to_restore_seconds": restore,
            "cause_unknown": cause_unknown,
            "cause": if cause_unknown { "cause unknown" } else { "known" },
            "causes": cause_rows,
        }));
    }
    let failed: Vec<Value> = failed_by_change
        .into_iter()
        .map(|(change, mut incidents)| {
            incidents.sort();
            (change, incidents)
        })
        .map(|(change, incidents)| json!({"change": change, "outcome": "failed", "incidents": incidents}))
        .collect();
    Ok(json!({
        "schema_version": INCIDENTS_SCHEMA,
        "family": family,
        "since": since_ms.map(rfc3339),
        "summary": {
            "incidents": incidents.len(),
            "cause_unknown": unknown,
            "restored": restore_times.len(),
            "mean_time_to_restore_seconds": mean(&restore_times),
            "median_time_to_restore_seconds": median(&mut restore_times),
            "mean_time_to_detect_seconds": mean(&detect_times),
        },
        "failed_changes": failed,
        "incidents": incidents,
    }))
}

/// How a vendor's alert body becomes incident events. The generic JSON shape
/// is [`GenericAlert`]; a vendor adapter is another implementation, chosen by
/// the route, so no vendor shape leaks into the fold.
pub(crate) trait AlertAdapter {
    fn events(&self, body: &Value) -> Result<Vec<NewEvent>, String>;
}

/// `{"incident_id", "status": "firing"|"resolved", "service", "severity",
/// "summary", "family", "repo", "detected_at", "restored_at",
/// "restoring_change", "changes": [...]}`.
pub(crate) struct GenericAlert;

impl AlertAdapter for GenericAlert {
    fn events(&self, body: &Value) -> Result<Vec<NewEvent>, String> {
        let id = incident_id(body)?;
        let status = text(body, "status").unwrap_or("firing");
        let summary = text(body, "summary")
            .map(str::to_string)
            .unwrap_or_else(|| format!("alert {id}"));
        let base = |kind: &str, suffix: &str, detail: Value| NewEvent {
            event_id: Some(format!("alert-{id}-{suffix}")),
            source: "alert-webhook".to_string(),
            kind: kind.to_string(),
            family: text(body, "family").map(str::to_string),
            repo: text(body, "repo").map(str::to_string),
            summary: summary.clone(),
            detail: Some(detail),
            ..NewEvent::default()
        };
        let mut events = Vec::new();
        match status {
            "firing" | "triggered" | "open" | "opened" => {
                events.push(base(
                    OPENED,
                    "opened",
                    json!({
                        "incident_id": id,
                        "service": text(body, "service"),
                        "severity": text(body, "severity"),
                        "detected_at": text(body, "detected_at"),
                    }),
                ));
            }
            "resolved" | "restored" | "ok" => {
                events.push(base(
                    RESTORED,
                    "restored",
                    json!({
                        "incident_id": id,
                        "restored_at": text(body, "restored_at"),
                        "restoring_change": text(body, "restoring_change"),
                    }),
                ));
            }
            other => return Err(format!("status: {other:?} is not firing or resolved")),
        }
        let named = changes(body)?;
        if !named.is_empty() {
            events.push(base(
                CAUSED_BY,
                "caused-by",
                json!({"incident_id": id, "changes": named}),
            ));
        }
        Ok(events)
    }
}

/// What the authenticated "Report incident" form submits:
/// `{"incident_id"?, "summary", "service", "severity", "family", "repo",
/// "detected_at", "restored_at", "restoring_change", "changes": [...],
/// "closed"}`. A reporter tells the form what they know when they send it, so
/// one submission may open, attribute, restore and close at once; what they do
/// not know yet arrives later as its own event on the same incident.
pub(crate) struct ReportedIncident;

impl AlertAdapter for ReportedIncident {
    fn events(&self, body: &Value) -> Result<Vec<NewEvent>, String> {
        let id = incident_id(body)?;
        let summary = text(body, "summary")
            .ok_or("summary: say in one line what is wrong")?
            .to_string();
        let base = |kind: &str, suffix: &str, detail: Value| NewEvent {
            event_id: Some(format!("report-{id}-{suffix}")),
            source: "incident-form".to_string(),
            kind: kind.to_string(),
            family: text(body, "family").map(str::to_string),
            repo: text(body, "repo").map(str::to_string),
            summary: summary.clone(),
            detail: Some(detail),
            ..NewEvent::default()
        };
        let mut events = vec![base(
            OPENED,
            "opened",
            json!({
                "incident_id": id,
                "service": text(body, "service"),
                "severity": text(body, "severity"),
                "detected_at": text(body, "detected_at"),
            }),
        )];
        let named = changes(body)?;
        if !named.is_empty() {
            events.push(base(
                CAUSED_BY,
                "caused-by",
                json!({"incident_id": id, "changes": named}),
            ));
        }
        if text(body, "restored_at").is_some() || text(body, "restoring_change").is_some() {
            events.push(base(
                RESTORED,
                "restored",
                json!({
                    "incident_id": id,
                    "restored_at": text(body, "restored_at"),
                    "restoring_change": text(body, "restoring_change"),
                }),
            ));
        }
        if body.get("closed").and_then(Value::as_bool) == Some(true) {
            events.push(base(
                CLOSED,
                "closed",
                json!({"incident_id": id, "closed_at": text(body, "closed_at")}),
            ));
        }
        Ok(events)
    }
}

/// The incident id for a form submission that did not name one: the minute it
/// was reported, so a person reading the ledger recognises it, and enough
/// random hex that two reports in the same minute stay apart.
fn new_incident_id() -> String {
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let token = Uuid::new_v4().simple().to_string();
    format!("inc-{stamp}-{}", &token[..8])
}

fn refuse(status: StatusCode, code: &str, reason: &str, hint: &str) -> AxumResponse {
    typed_error(TypedError {
        status,
        code,
        purpose: "report or read incidents",
        reason,
        common_fixes: &[
            "send detail.incident_id on every incident event",
            "post as a global admin or a JERYU_EVENT_REPORTERS identity",
        ],
        docs_url: DOCS,
        repair_hint: hint,
        message: reason,
    })
}

/// `POST /api/v1/incidents/webhook`: one generic alert body, or an array of
/// them, turned into incident events.
pub(crate) async fn webhook(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    body: Bytes,
) -> AxumResponse {
    if !super::pipeline::may_post(&account) {
        return refuse(
            StatusCode::FORBIDDEN,
            "events_reporter_required",
            "this account may not post pipeline events (JERYU_EVENT_REPORTERS)",
            "post with a global-admin token or add the login to JERYU_EVENT_REPORTERS",
        );
    }
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
            return refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                "incident_webhook_invalid",
                &err.to_string(),
                "send a JSON object with incident_id and status",
            );
        }
    };
    let bodies = match value {
        Value::Array(items) => items,
        single => vec![single],
    };
    // The same cap as `POST /api/v1/events`: the ledgers this feeds are never
    // pruned, so a batch is bounded, not just the body size.
    if bodies.len() > super::pipeline::MAX_EVENT_BATCH {
        return refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            "incident_webhook_too_many",
            &format!(
                "alerts: at most {} per request",
                super::pipeline::MAX_EVENT_BATCH
            ),
            "split the alerts across requests",
        );
    }
    let mut events = Vec::new();
    for (index, item) in bodies.iter().enumerate() {
        match GenericAlert.events(item) {
            Ok(mut more) => events.append(&mut more),
            Err(reason) => {
                return refuse(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "incident_webhook_invalid",
                    &format!("alerts[{index}]: {reason}"),
                    "send incident_id and status firing or resolved",
                );
            }
        }
    }
    // Each insert takes the event-store lock: off the async runtime.
    let worker = state.clone();
    let login = account.login.clone();
    let recorded = tokio::task::spawn_blocking(move || {
        let mut seqs = Vec::new();
        for event in events {
            seqs.push(super::pipeline::record(&worker, &login, event)?.event.seq);
        }
        Ok::<_, String>(seqs)
    })
    .await
    .map_err(|err| err.to_string())
    .and_then(|recorded| recorded);
    match recorded {
        Ok(seqs) => (
            StatusCode::CREATED,
            Json(json!({"schema_version": INCIDENTS_SCHEMA, "ok": true, "seqs": seqs})),
        )
            .into_response(),
        Err(reason) => refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            "incident_webhook_invalid",
            &reason,
            "fix the field named in the message and retry",
        ),
    }
}

/// `POST /api/v1/incidents`: one incident from the authenticated "Report
/// incident" form. The body is [`ReportedIncident`]; `incident_id` is optional
/// and generated when the reporter does not name one, so the form has one
/// required field, `summary`. Replies with the incident id the reporter can
/// then read back or add to.
pub(crate) async fn report_incident(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    body: Bytes,
) -> AxumResponse {
    if !super::pipeline::may_post(&account) {
        return refuse(
            StatusCode::FORBIDDEN,
            "events_reporter_required",
            "this account may not post pipeline events (JERYU_EVENT_REPORTERS)",
            "report with a global-admin token or add the login to JERYU_EVENT_REPORTERS",
        );
    }
    let mut value: Value = match serde_json::from_slice(&body) {
        Ok(Value::Object(fields)) => Value::Object(fields),
        Ok(_) => {
            return refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                "incident_report_invalid",
                "the body is one JSON object describing one incident",
                "send {\"summary\": \"what is wrong\", \"service\": \"checkout\"}",
            );
        }
        Err(err) => {
            return refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                "incident_report_invalid",
                &err.to_string(),
                "send {\"summary\": \"what is wrong\", \"service\": \"checkout\"}",
            );
        }
    };
    if text(&value, "incident_id").is_none() {
        value["incident_id"] = Value::String(new_incident_id());
    }
    let events = match ReportedIncident.events(&value) {
        Ok(events) => events,
        Err(reason) => {
            return refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                "incident_report_invalid",
                &reason,
                "fix the field named in the message and send the form again",
            );
        }
    };
    let id = text(&value, "incident_id").unwrap_or_default().to_string();
    // Each insert takes the event-store lock: off the async runtime.
    let worker = state.clone();
    let login = account.login.clone();
    let recorded = tokio::task::spawn_blocking(move || {
        let mut seqs = Vec::new();
        for event in events {
            seqs.push(super::pipeline::record(&worker, &login, event)?.event.seq);
        }
        Ok::<_, String>(seqs)
    })
    .await
    .map_err(|err| err.to_string())
    .and_then(|recorded| recorded);
    match recorded {
        Ok(seqs) => (
            StatusCode::CREATED,
            Json(json!({
                "schema_version": INCIDENTS_SCHEMA,
                "ok": true,
                "incident_id": id,
                "seqs": seqs,
            })),
        )
            .into_response(),
        Err(reason) => refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            "incident_report_invalid",
            &reason,
            "fix the field named in the message and send the form again",
        ),
    }
}

/// `GET /api/v1/incidents` query.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct IncidentsQuery {
    /// Either spelling of a family key; the server canonicalises it.
    pub family: Option<String>,
    /// How many days back to read (default 90, at most 365).
    pub days: Option<u32>,
}

impl StrictFields for IncidentsQuery {
    const KEYS: &'static [&'static str] = &["family", "days"];
    const REPAIR_HINT: &'static str = "filter by family and days (1 to 365)";
}

/// `GET /api/v1/incidents` (global admins, like the flow report).
pub(crate) async fn list(
    State(state): State<Arc<WebState>>,
    StrictQuery(query): StrictQuery<IncidentsQuery>,
) -> AxumResponse {
    let family = match super::family::filter(&state, query.family.as_deref()) {
        Ok(family) => family,
        Err(response) => return *response,
    };
    let days = query.days.unwrap_or(DEFAULT_DAYS);
    if days == 0 || days > MAX_DAYS {
        return refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            super::strict_query::CODE,
            &format!("days must be 1 to {MAX_DAYS}"),
            "send days between 1 and 365",
        );
    }
    let since_ms = chrono::Utc::now().timestamp_millis() - i64::from(days) * 86_400_000;
    let worker = state.clone();
    let read = tokio::task::spawn_blocking(move || {
        worker
            .events
            .with_conn(|conn| report(conn, family.as_deref(), Some(since_ms)))
    })
    .await
    .map_err(|err| err.to_string())
    .and_then(|read| read);
    match read {
        Ok(body) => Json(body).into_response(),
        Err(reason) => refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "incidents_read_failed",
            &reason,
            "check <data_dir>/shift.sqlite and the server log",
        ),
    }
}
