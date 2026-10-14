//! Reported agent sessions in `<data_dir>/shift.sqlite` (`agent_presence`,
//! `db/migrations/0012_agent_presence.sql`): one row per `session_id`,
//! replaced on every beat. Every method takes the clock as `now_ms`, so the
//! expiry and retention rules are tested without waiting.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use super::types::{Links, PresenceState, Tokens};
use crate::web::shift::migrate_shift_store;

/// How long an ended or stale session stays listed after it ended or expired.
pub(crate) const LINGER_MS: i64 = 10 * 60 * 1000;
/// Rows not beaten for this long are deleted on write.
pub(crate) const RETENTION_MS: i64 = 14 * 24 * 60 * 60 * 1000;
/// Listed (live, stale or lingering) sessions per reporter and forge-wide.
pub(crate) const MAX_REPORTER_ROWS: i64 = 64;
pub(crate) const MAX_ROWS: i64 = 1024;
/// Stored rows per reporter and forge-wide, listed or not. Past its own bound a
/// reporter's oldest unlisted rows are deleted to make room. At the forge bound,
/// unlisted history is reclaimed from the largest retained reporter first;
/// listed rows are never evicted. Refuse only when no unlisted row can make room.
pub(crate) const MAX_REPORTER_RETAINED: i64 = 2048;
pub(crate) const MAX_RETAINED: i64 = 16384;
const WRITES_PER_MINUTE: u32 = 120;

/// The four session bounds, the constants above unless a test shrinks them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub reporter_listed: i64,
    pub listed: i64,
    pub reporter_retained: i64,
    pub retained: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            reporter_listed: MAX_REPORTER_ROWS,
            listed: MAX_ROWS,
            reporter_retained: MAX_REPORTER_RETAINED,
            retained: MAX_RETAINED,
        }
    }
}

#[derive(Debug)]
pub(crate) enum StoreError {
    Owned,
    Limit,
    /// The forge stores its bound of sessions; a new one is refused.
    Full,
    WriteBudget,
    Database(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.to_string())
    }
}

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

/// When a stored session that has not ended yet ends at `now_ms`: now while a
/// reader still sees it, else the moment it expired. Ending a session that has
/// stopped being listed therefore never lists it again, so it cannot get past
/// the listed caps.
fn end_time(stored: &PresenceRow, now_ms: i64) -> i64 {
    if listed(stored, now_ms) {
        now_ms
    } else {
        stored.expires_ms.min(now_ms)
    }
}

#[derive(Clone)]
pub(crate) struct PresenceStore {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    conn: Connection,
    last_prune_ms: i64,
    budgets: BTreeMap<String, (i64, u32)>,
    limits: Limits,
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
                budgets: BTreeMap::new(),
                limits: Limits::default(),
            })),
        })
    }

    /// The same store with smaller bounds, so tests reach them quickly.
    #[cfg(test)]
    pub(crate) fn with_limits(self, limits: Limits) -> Self {
        self.inner
            .lock()
            .expect("agent presence mutex poisoned")
            .limits = limits;
        self
    }

    /// The stored session, if any.
    #[cfg(test)]
    pub(crate) fn get(&self, session_id: &str) -> Result<Option<PresenceRow>, String> {
        let inner = self.inner.lock().expect("agent presence mutex poisoned");
        get_row(&inner.conn, session_id).map_err(|err| err.to_string())
    }

    fn budget(inner: &mut Inner, reporter: &str, now_ms: i64) -> Result<(), StoreError> {
        inner
            .budgets
            .retain(|_, (start, _)| now_ms.saturating_sub(*start) < 60_000);
        if !inner.budgets.contains_key(reporter) && inner.budgets.len() >= MAX_ROWS as usize {
            return Err(StoreError::WriteBudget);
        }
        let (_, writes) = inner
            .budgets
            .entry(reporter.to_string())
            .or_insert((now_ms, 0));
        if *writes >= WRITES_PER_MINUTE {
            return Err(StoreError::WriteBudget);
        }
        *writes += 1;
        Ok(())
    }

    /// Replace the session's row with `row`. `ended_ms` is decided here: kept
    /// from the stored row while the session stays ended, set to `now_ms` when
    /// it first reports `ended`, and cleared when it comes back.
    #[cfg(test)]
    pub(crate) fn upsert(&self, row: &PresenceRow, now_ms: i64) -> Result<PresenceRow, StoreError> {
        self.upsert_as(row, &row.reporter, false, now_ms)
    }

    pub(crate) fn upsert_as(
        &self,
        row: &PresenceRow,
        actor: &str,
        admin: bool,
        now_ms: i64,
    ) -> Result<PresenceRow, StoreError> {
        let mut inner = self.inner.lock().expect("agent presence mutex poisoned");
        Self::budget(&mut inner, actor, now_ms)?;
        let limits = inner.limits;
        let prune = now_ms.saturating_sub(inner.last_prune_ms) >= PRUNE_EVERY_MS;
        let tx = inner
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if prune {
            tx.execute(
                "DELETE FROM agent_presence WHERE received_ms < ?1",
                params![now_ms - RETENTION_MS],
            )?;
        }
        let previous = get_row(&tx, &row.session_id)?;
        if previous
            .as_ref()
            .is_some_and(|existing| existing.reporter != actor && !admin)
        {
            return Err(StoreError::Owned);
        }
        let mut row = row.clone();
        row.reporter = previous
            .as_ref()
            .map_or_else(|| actor.to_string(), |existing| existing.reporter.clone());
        // Repeating an ended heartbeat cannot renew retention indefinitely.
        row.received_ms = previous
            .as_ref()
            .filter(|stored| stored.ended_ms.is_some() && row.state == PresenceState::Ended)
            .map_or(now_ms, |stored| stored.received_ms);
        row.expires_ms = now_ms.saturating_add(row.ttl_seconds.saturating_mul(1000));
        row.ended_ms = match row.state {
            PresenceState::Ended => Some(match previous.as_ref() {
                Some(stored) => stored.ended_ms.unwrap_or_else(|| end_time(stored, now_ms)),
                None => now_ms,
            }),
            _ => None,
        };
        // Only sessions a reader sees count against the listed caps, so a reporter
        // that starts many short sessions a day is not refused for ones that ended
        // long ago. Every write that makes a row listed is checked: a new session,
        // and a beat that brings an unlisted row back. A row already listed is
        // already counted.
        let was_listed = previous
            .as_ref()
            .is_some_and(|stored| listed(stored, now_ms));
        if !was_listed && listed(&row, now_ms) {
            let cutoff = now_ms.saturating_sub(LINGER_MS);
            let total: i64 = tx.query_row(
                "SELECT COUNT(*) FROM agent_presence
                 WHERE COALESCE(ended_ms, expires_ms) >= ?1",
                params![cutoff],
                |row| row.get(0),
            )?;
            let owned: i64 = tx.query_row(
                "SELECT COUNT(*) FROM agent_presence
                 WHERE reporter = ?1 AND COALESCE(ended_ms, expires_ms) >= ?2",
                params![row.reporter, cutoff],
                |row| row.get(0),
            )?;
            if total >= limits.listed || owned >= limits.reporter_listed {
                return Err(StoreError::Limit);
            }
        }
        if previous.is_none() {
            // Reclaim this reporter's unlisted history first. Global pressure
            // reclaims only unlisted history, favoring the largest retained
            // reporter so a few prolific reporters cannot lock out everyone.
            evict_own_unlisted(
                &tx,
                &row.reporter,
                limits.reporter_retained,
                now_ms.saturating_sub(LINGER_MS),
            )?;
            let owned: i64 = tx.query_row(
                "SELECT COUNT(*) FROM agent_presence WHERE reporter = ?1",
                params![row.reporter],
                |row| row.get(0),
            )?;
            if owned >= limits.reporter_retained {
                return Err(StoreError::Full);
            }
            make_global_room(&tx, limits.retained, now_ms.saturating_sub(LINGER_MS))?;
        }
        tx.execute(
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
        )?;
        tx.commit()?;
        if prune {
            inner.last_prune_ms = now_ms;
        }
        Ok(row)
    }

    /// Mark the session ended at `now_ms` (kept if it already ended). `None`
    /// when no such session is stored.
    #[cfg(test)]
    pub(crate) fn end(
        &self,
        session_id: &str,
        now_ms: i64,
    ) -> Result<Option<PresenceRow>, StoreError> {
        self.end_as(session_id, "test-admin", true, now_ms)
    }

    pub(crate) fn end_as(
        &self,
        session_id: &str,
        actor: &str,
        admin: bool,
        now_ms: i64,
    ) -> Result<Option<PresenceRow>, StoreError> {
        let mut inner = self.inner.lock().expect("agent presence mutex poisoned");
        Self::budget(&mut inner, actor, now_ms)?;
        let tx = inner
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some(mut row) = get_row(&tx, session_id)? else {
            return Ok(None);
        };
        if row.reporter != actor && !admin {
            return Ok(None);
        }
        if row.ended_ms.is_none() {
            let ended = end_time(&row, now_ms);
            row.state = PresenceState::Ended;
            row.ended_ms = Some(ended);
            tx.execute(
                "UPDATE agent_presence SET state = 'ended', ended_ms = ?2 WHERE session_id = ?1",
                params![session_id, ended],
            )?;
        }
        tx.commit()?;
        Ok(Some(row))
    }

    /// Every row a reader still sees at `now_ms` (see [`listed`]).
    pub(crate) fn visible(&self, now_ms: i64) -> Result<Vec<PresenceRow>, String> {
        let inner = self.inner.lock().expect("agent presence mutex poisoned");
        let mut stmt = inner
            .conn
            .prepare(
                "SELECT session_id, reporter, agent, model, host, operator, lane, branch,
                        title, state, repos_json, families_json, started_ms, last_active_ms,
                        ttl_seconds, received_ms, expires_ms, ended_ms, cost_usd, tokens_json,
                        links_json
                   FROM agent_presence
                  WHERE (ended_ms IS NOT NULL AND ended_ms >= ?1)
                     OR (ended_ms IS NULL AND expires_ms >= ?1)
                  ORDER BY session_id",
            )
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

/// Make room for one more row of `reporter` under its retention bound by
/// deleting its own oldest rows no reader sees any more (listed until `cutoff`).
/// Their session ids are released. Other reporters' rows are never touched.
fn evict_own_unlisted(
    conn: &Connection,
    reporter: &str,
    bound: i64,
    cutoff: i64,
) -> rusqlite::Result<()> {
    let owned: i64 = conn.query_row(
        "SELECT COUNT(*) FROM agent_presence WHERE reporter = ?1",
        params![reporter],
        |row| row.get(0),
    )?;
    let excess = owned.saturating_sub(bound - 1);
    if excess > 0 {
        conn.execute(
            "DELETE FROM agent_presence WHERE session_id IN (
                 SELECT session_id FROM agent_presence
                 WHERE reporter = ?1 AND COALESCE(ended_ms, expires_ms) < ?2
                 ORDER BY received_ms ASC LIMIT ?3)",
            params![reporter, cutoff, excess],
        )?;
    }
    Ok(())
}

/// Bound stored history without letting a full collection of tombstones deny
/// new reporters. All changes share the insertion transaction. Listed rows and
/// their ownership survive; reclaimed unlisted session ids are released.
fn make_global_room(conn: &Connection, bound: i64, cutoff: i64) -> Result<(), StoreError> {
    let stored: i64 =
        conn.query_row("SELECT COUNT(*) FROM agent_presence", [], |row| row.get(0))?;
    let excess = stored.saturating_sub(bound - 1);
    if excess > 0 {
        conn.execute(
            "DELETE FROM agent_presence WHERE session_id IN (
                SELECT p.session_id FROM agent_presence p
                JOIN (SELECT reporter, COUNT(*) AS retained FROM agent_presence GROUP BY reporter) r
                  ON p.reporter = r.reporter
                WHERE COALESCE(p.ended_ms, p.expires_ms) < ?1
                ORDER BY r.retained DESC, p.received_ms ASC, p.session_id ASC LIMIT ?2)",
            params![cutoff, excess],
        )?;
        let remaining: i64 =
            conn.query_row("SELECT COUNT(*) FROM agent_presence", [], |row| row.get(0))?;
        if remaining >= bound {
            return Err(StoreError::Full);
        }
    }
    Ok(())
}

fn get_row(conn: &Connection, session_id: &str) -> rusqlite::Result<Option<PresenceRow>> {
    conn.query_row(
        "SELECT session_id, reporter, agent, model, host, operator, lane, branch,
        title, state, repos_json, families_json, started_ms, last_active_ms, ttl_seconds,
        received_ms, expires_ms, ended_ms, cost_usd, tokens_json, links_json
        FROM agent_presence WHERE session_id = ?1",
        params![session_id],
        read_row,
    )
    .optional()
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("presence column serializes")
}

fn from_json<T: serde::de::DeserializeOwned + Default>(raw: Option<String>) -> Option<T> {
    raw.map(|raw| serde_json::from_str(&raw).unwrap_or_default())
}

/// One row of either select, which both list the columns in table order.
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
