//! Deliberately deferred attention items, kept in `<data_dir>/shift.sqlite`
//! (`attention_acks`, `db/migrations/0006_attention_acks.sql`).
//!
//! Parking is not only for todos: a mirror everybody knows is failing, or a
//! draft pull request somebody is keeping open on purpose, asks for a person
//! every ten seconds and teaches a reader to ignore the inbox. An
//! acknowledgement is keyed by the attention item's own id, whatever kind it
//! is, and hides the item until a date. Once that date passes the item is
//! listed again, because the acknowledgement said "not now", not "never".
//!
//! - `GET /api/v1/attention/acks`: every acknowledgement, expired ones too.
//! - `POST /api/v1/attention/acks`: `{"item_id", "until": RFC3339 | null, "note"}`;
//!   `until: null` drops the acknowledgement and lists the item again.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{DateTime, Utc};
use jeryu_core::AccountSummary;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use super::super::super::shift::{migrate_shift_store, rfc3339_ms};
use super::super::super::{WebState, api_error};

/// One acknowledgement as it is stored and reported.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Ack {
    pub item_id: String,
    /// When the item comes back, RFC 3339 with milliseconds.
    pub until: String,
    pub note: String,
    pub acked_at: String,
    pub acked_by: String,
}

#[derive(Clone)]
pub(crate) struct AckStore {
    inner: Arc<Mutex<Connection>>,
}

impl AckStore {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|err| err.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|err| err.to_string())?;
        migrate_shift_store(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    /// Every acknowledgement, newest-ending first. Expired ones are kept and
    /// reported: they are the record of what somebody decided to live with.
    pub(crate) fn all(&self) -> Result<Vec<Ack>, String> {
        let conn = self.inner.lock().expect("attention ack mutex poisoned");
        let mut statement = conn
            .prepare(
                "SELECT item_id, until_ms, note, acked_ms, acked_by
                   FROM attention_acks ORDER BY until_ms DESC, item_id",
            )
            .map_err(|err| err.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok(Ack {
                    item_id: row.get(0)?,
                    until: rfc3339_ms(row.get(1)?),
                    note: row.get(2)?,
                    acked_at: rfc3339_ms(row.get(3)?),
                    acked_by: row.get(4)?,
                })
            })
            .map_err(|err| err.to_string())?;
        rows.collect::<Result<Vec<Ack>, _>>()
            .map_err(|err| err.to_string())
    }

    /// The ids whose acknowledgement still holds at `now_ms`.
    pub(crate) fn hidden(&self, now_ms: i64) -> Result<Vec<String>, String> {
        let conn = self.inner.lock().expect("attention ack mutex poisoned");
        let mut statement = conn
            .prepare("SELECT item_id FROM attention_acks WHERE until_ms > ?1")
            .map_err(|err| err.to_string())?;
        let rows = statement
            .query_map(params![now_ms], |row| row.get(0))
            .map_err(|err| err.to_string())?;
        rows.collect::<Result<Vec<String>, _>>()
            .map_err(|err| err.to_string())
    }

    /// Acknowledge `item_id` until `until_ms`, or drop it when `until_ms` is
    /// `None`.
    pub(crate) fn set(
        &self,
        item_id: &str,
        until_ms: Option<i64>,
        note: &str,
        acked_by: &str,
        now_ms: i64,
    ) -> Result<(), String> {
        let conn = self.inner.lock().expect("attention ack mutex poisoned");
        match until_ms {
            Some(until_ms) => conn.execute(
                "INSERT INTO attention_acks (item_id, until_ms, note, acked_ms, acked_by)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(item_id) DO UPDATE SET
                   until_ms = excluded.until_ms,
                   note = excluded.note,
                   acked_ms = excluded.acked_ms,
                   acked_by = excluded.acked_by",
                params![item_id, until_ms, note, now_ms, acked_by],
            ),
            None => conn.execute(
                "DELETE FROM attention_acks WHERE item_id = ?1",
                params![item_id],
            ),
        }
        .map(|_| ())
        .map_err(|err| err.to_string())
    }
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct AckRequest {
    pub item_id: String,
    /// When the item comes back. `null` drops the acknowledgement.
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AcksResponse {
    pub acks: Vec<Ack>,
}

/// `GET /api/v1/attention/acks` (admin-only by path, see `auth::admin_only_request`).
pub(crate) async fn list_acks(State(state): State<Arc<WebState>>) -> AxumResponse {
    match state.attention_acks.all() {
        Ok(acks) => Json(AcksResponse { acks }).into_response(),
        Err(reason) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("attention acknowledgements could not be read: {reason}"),
        ),
    }
}

/// `POST /api/v1/attention/acks`
pub(crate) async fn ack(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    body: Bytes,
) -> AxumResponse {
    let request: AckRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                &err.to_string(),
            );
        }
    };
    let item_id = request.item_id.trim();
    if item_id.is_empty() {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "item_id must be the id of an attention item",
        );
    }
    let until = request
        .until
        .as_deref()
        .map(str::trim)
        .filter(|until| !until.is_empty());
    let until = match until.map(DateTime::parse_from_rfc3339) {
        Some(Ok(until)) => Some(until.with_timezone(&Utc)),
        Some(Err(err)) => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                &format!("until must be an RFC 3339 time: {err}"),
            );
        }
        None => None,
    };
    let now = Utc::now();
    let note = request.note.as_deref().map(str::trim).unwrap_or_default();
    if let Err(reason) = state.attention_acks.set(
        item_id,
        until.map(|until| until.timestamp_millis()),
        note,
        &account.login,
        now.timestamp_millis(),
    ) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("the acknowledgement could not be stored: {reason}"),
        );
    }
    // The inbox answer is cached for a few seconds; an acknowledgement must
    // show in the next read, not after the cache runs out.
    state.attention.invalidate();
    match until {
        Some(until) => (
            StatusCode::CREATED,
            Json(Ack {
                item_id: item_id.to_string(),
                until: rfc3339_ms(until.timestamp_millis()),
                note: note.to_string(),
                acked_at: rfc3339_ms(now.timestamp_millis()),
                acked_by: account.login.clone(),
            }),
        )
            .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}
