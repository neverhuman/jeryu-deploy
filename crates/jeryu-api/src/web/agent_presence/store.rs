//! Reported agent sessions in `<data_dir>/shift.sqlite` (`agent_presence`,
//! `db/migrations/0012_agent_presence.sql`): one row per `session_id`,
//! replaced on every beat. Every method takes the clock as `now_ms`, so the
//! expiry and retention rules are tested without waiting.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use super::types::{Links, PresenceState, Tokens};
use crate::web::shift::migrate_shift_store;

/// How long an ended or stale session stays listed after it ended or expired.
pub(crate) const LINGER_MS: i64 = 10 * 60 * 1000;
/// Rows not beaten for this long are deleted on write.
pub(crate) const RETENTION_MS: i64 = 14 * 24 * 60 * 60 * 1000;
const PRUNE_EVERY_MS: i64 = 60 * 60 * 1000;

/// One stored session, as the last beat left it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PresenceRow {
    pub session_id: String,
    /// The login that first registered the session; only it (or an admin)
    /// may beat or end it.
    pub reporter: String,
    pub agent: String,
    pub model: Option<String>,
    pub host: Option<String>,
    pub operator: Option<String>,
    pub lane: Option<String>,
    pub branch: Option<String>,
    pub title: Option<String>,
    pub state: PresenceState,
    /// Canonical `owner/name` of every hosted repository the session named.
    pub repos: Vec<String>,
    /// Canonical family keys the session declared.
    pub families: Vec<String>,
    pub started_ms: i64,
    pub last_active_ms: i64,
    pub ttl_seconds: i64,
    pub received_ms: i64,
    pub expires_ms: i64,
    pub ended_ms: Option<i64>,
    pub cost_usd: Option<f64>,
    pub tokens: Option<Tokens>,
    pub links: Option<Links>,
}

/// The state a reader is shown at `now_ms`: an ended session is `ended`, a
/// session past its expiry is `stale`, anything else is what it reported.
pub(crate) fn effective_state(row: &PresenceRow, now_ms: i64) -> &'static str {
    if row.state == PresenceState::Ended || row.ended_ms.is_some() {
        "ended"
    } else if now_ms > row.expires_ms {
        "stale"
    } else {
        row.state.as_str()
    }
}

/// Whether a reader still sees the row at `now_ms`.
pub(crate) fn listed(row: &PresenceRow, now_ms: i64) -> bool {
    match row.ended_ms {
        Some(ended) => now_ms <= ended.saturating_add(LINGER_MS),
        None => now_ms <= row.expires_ms.saturating_add(LINGER_MS),
    }
}

#[derive(Clone)]
pub(crate) struct PresenceStore {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    conn: Connection,
    last_prune_ms: i64,
}

impl PresenceStore {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|err| err.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|err| err.to_string())?;
        migrate_shift_store(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                conn,
                last_prune_ms: 0,
            })),
        })
    }

    /// The stored session, if any.
    pub(crate) fn get(&self, session_id: &str) -> Result<Option<PresenceRow>, String> {
        let inner = self.inner.lock().expect("agent presence mutex poisoned");
        inner
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM agent_presence WHERE session_id = ?1"),
                params![session_id],
                read_row,
            )
            .optional()
            .map_err(|err| err.to_string())
    }

    /// Replace the session's row with `row`. `ended_ms` is decided here: kept
    /// from the stored row while the session stays ended, set to `now_ms` when
    /// it first reports `ended`, and cleared when it comes back.
    pub(crate) fn upsert(&self, row: &PresenceRow, now_ms: i64) -> Result<PresenceRow, String> {
        let previous_end = self
            .get(&row.session_id)?
            .and_then(|stored| stored.ended_ms);
        let mut row = row.clone();
        row.received_ms = now_ms;
        row.expires_ms = now_ms.saturating_add(row.ttl_seconds.saturating_mul(1000));
        row.ended_ms = match row.state {
            PresenceState::Ended => Some(previous_end.unwrap_or(now_ms)),
            _ => None,
        };
        let mut inner = self.inner.lock().expect("agent presence mutex poisoned");
        inner
            .conn
            .execute(
                "INSERT INTO agent_presence (session_id, reporter, agent, model, host, operator,
                   lane, branch, title, state, repos_json, families_json, started_ms,
                   last_active_ms, ttl_seconds, received_ms, expires_ms, ended_ms, cost_usd,
                   tokens_json, links_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                   ?16, ?17, ?18, ?19, ?20, ?21)
                 ON CONFLICT(session_id) DO UPDATE SET
                   reporter = excluded.reporter, agent = excluded.agent,
                   model = excluded.model, host = excluded.host,
                   operator = excluded.operator, lane = excluded.lane,
                   branch = excluded.branch, title = excluded.title,
                   state = excluded.state, repos_json = excluded.repos_json,
                   families_json = excluded.families_json, started_ms = excluded.started_ms,
                   last_active_ms = excluded.last_active_ms,
                   ttl_seconds = excluded.ttl_seconds, received_ms = excluded.received_ms,
                   expires_ms = excluded.expires_ms, ended_ms = excluded.ended_ms,
                   cost_usd = excluded.cost_usd, tokens_json = excluded.tokens_json,
                   links_json = excluded.links_json",
                params![
                    row.session_id,
                    row.reporter,
                    row.agent,
                    row.model,
                    row.host,
                    row.operator,
                    row.lane,
                    row.branch,
                    row.title,
                    row.state.as_str(),
                    to_json(&row.repos),
                    to_json(&row.families),
                    row.started_ms,
                    row.last_active_ms,
                    row.ttl_seconds,
                    row.received_ms,
                    row.expires_ms,
                    row.ended_ms,
                    row.cost_usd,
                    row.tokens.as_ref().map(to_json),
                    row.links.as_ref().map(to_json),
                ],
            )
            .map_err(|err| err.to_string())?;
        if now_ms - inner.last_prune_ms >= PRUNE_EVERY_MS {
            inner
                .conn
                .execute(
                    "DELETE FROM agent_presence WHERE received_ms < ?1",
                    params![now_ms - RETENTION_MS],
                )
                .map_err(|err| err.to_string())?;
            inner.last_prune_ms = now_ms;
        }
        Ok(row)
    }

    /// Mark the session ended at `now_ms` (kept if it already ended). `None`
    /// when no such session is stored.
    pub(crate) fn end(&self, session_id: &str, now_ms: i64) -> Result<Option<PresenceRow>, String> {
        let Some(mut row) = self.get(session_id)? else {
            return Ok(None);
        };
        if row.ended_ms.is_some() {
            return Ok(Some(row));
        }
        row.state = PresenceState::Ended;
        row.ended_ms = Some(now_ms);
        let inner = self.inner.lock().expect("agent presence mutex poisoned");
        inner
            .conn
            .execute(
                "UPDATE agent_presence SET state = 'ended', ended_ms = ?2 WHERE session_id = ?1",
                params![session_id, now_ms],
            )
            .map_err(|err| err.to_string())?;
        Ok(Some(row))
    }

    /// Every row a reader still sees at `now_ms` (see [`listed`]).
    pub(crate) fn visible(&self, now_ms: i64) -> Result<Vec<PresenceRow>, String> {
        let inner = self.inner.lock().expect("agent presence mutex poisoned");
        let mut stmt = inner
            .conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM agent_presence
                  WHERE (ended_ms IS NOT NULL AND ended_ms >= ?1)
                     OR (ended_ms IS NULL AND expires_ms >= ?1)
                  ORDER BY session_id"
            ))
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params![now_ms - LINGER_MS], read_row)
            .map_err(|err| err.to_string())?;
        let rows = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?;
        Ok(rows.into_iter().filter(|row| listed(row, now_ms)).collect())
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> i64 {
        let inner = self.inner.lock().expect("agent presence mutex poisoned");
        inner
            .conn
            .query_row("SELECT COUNT(*) FROM agent_presence", [], |row| row.get(0))
            .expect("count agent presence rows")
    }
}

const COLUMNS: &str = "session_id, reporter, agent, model, host, operator, lane, branch, title,
    state, repos_json, families_json, started_ms, last_active_ms, ttl_seconds, received_ms,
    expires_ms, ended_ms, cost_usd, tokens_json, links_json";

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("presence column serializes")
}

fn from_json<T: serde::de::DeserializeOwned + Default>(raw: Option<String>) -> Option<T> {
    raw.map(|raw| serde_json::from_str(&raw).unwrap_or_default())
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PresenceRow> {
    let state: String = row.get(9)?;
    Ok(PresenceRow {
        session_id: row.get(0)?,
        reporter: row.get(1)?,
        agent: row.get(2)?,
        model: row.get(3)?,
        host: row.get(4)?,
        operator: row.get(5)?,
        lane: row.get(6)?,
        branch: row.get(7)?,
        title: row.get(8)?,
        state: PresenceState::parse(&state).unwrap_or(PresenceState::Ended),
        repos: from_json(row.get(10)?).unwrap_or_default(),
        families: from_json(row.get(11)?).unwrap_or_default(),
        started_ms: row.get(12)?,
        last_active_ms: row.get(13)?,
        ttl_seconds: row.get(14)?,
        received_ms: row.get(15)?,
        expires_ms: row.get(16)?,
        ended_ms: row.get(17)?,
        cost_usd: row.get(18)?,
        tokens: from_json(row.get(19)?),
        links: from_json(row.get(20)?),
    })
}
