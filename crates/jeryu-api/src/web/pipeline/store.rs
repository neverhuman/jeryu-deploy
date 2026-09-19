//! The pipeline event log in `<data_dir>/shift.sqlite`.
//!
//! Append-only: rows are inserted and, after 30 days, pruned (at most once an
//! hour, on write); nothing updates a row. The schema comes from the reviewed
//! `db/migrations/0002_pipeline_events.sql`, applied by the shift migration
//! runner. The store holds its own connection to the file the heartbeat store
//! also uses; sqlite serialises the two writers and `busy_timeout` covers the
//! overlap.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, Row, params, params_from_iter};

use super::super::shift::{migrate_shift_store, rfc3339_ms};
use super::types::{Event, EventsQuery, NewEvent};

pub(crate) const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const PRUNE_EVERY_MS: i64 = 60 * 60 * 1000;
const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;

const COLUMNS: &str = "seq, ts_ms, source, kind, reporter, actor, family, repo, pr, sha, todo_id,
    shift, outcome, needs_human, summary, reason, cost_usd, seconds, log_tail, log_url, detail_json";

#[derive(Clone)]
pub(crate) struct EventStore {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    conn: Connection,
    last_prune_ms: i64,
}

impl EventStore {
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

    /// Append one already-normalised event and return it as stored.
    pub(crate) fn insert(
        &self,
        reporter: &str,
        event: &NewEvent,
        now_ms: i64,
    ) -> Result<Event, String> {
        let mut inner = self.inner.lock().expect("pipeline event mutex poisoned");
        let detail = event.detail.as_ref().map(|d| d.to_string());
        inner
            .conn
            .execute(
                "INSERT INTO pipeline_events (ts_ms, source, kind, reporter, actor, family, repo,
                   pr, sha, todo_id, shift, outcome, needs_human, summary, reason, cost_usd,
                   seconds, log_tail, log_url, detail_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                   ?17, ?18, ?19, ?20)",
                params![
                    now_ms,
                    event.source,
                    event.kind,
                    reporter,
                    event.actor,
                    event.family,
                    event.repo,
                    event.pr,
                    event.sha,
                    event.todo_id,
                    event.shift,
                    event.outcome,
                    event.needs_human,
                    event.summary,
                    event.reason,
                    event.cost_usd,
                    event.seconds,
                    event.log_tail,
                    event.log_url,
                    detail,
                ],
            )
            .map_err(|err| err.to_string())?;
        let seq = inner.conn.last_insert_rowid();
        if now_ms - inner.last_prune_ms >= PRUNE_EVERY_MS {
            inner
                .conn
                .execute(
                    "DELETE FROM pipeline_events WHERE ts_ms < ?1",
                    params![now_ms - RETENTION_MS],
                )
                .map_err(|err| err.to_string())?;
            inner.last_prune_ms = now_ms;
        }
        Ok(Event {
            seq,
            ts: rfc3339_ms(now_ms),
            source: event.source.clone(),
            kind: event.kind.clone(),
            reporter: reporter.to_string(),
            actor: event.actor.clone(),
            family: event.family.clone(),
            repo: event.repo.clone(),
            pr: event.pr,
            sha: event.sha.clone(),
            todo_id: event.todo_id.clone(),
            shift: event.shift.clone(),
            outcome: event.outcome.clone(),
            needs_human: event.needs_human,
            summary: event.summary.clone(),
            reason: event.reason.clone(),
            cost_usd: event.cost_usd,
            seconds: event.seconds,
            log_tail: event.log_tail.clone(),
            log_url: event.log_url.clone(),
            detail: event.detail.clone(),
        })
    }

    /// Events matching `query`. With `after_seq` the page is a cursor tail,
    /// oldest first; otherwise it is the newest events, newest first.
    pub(crate) fn query(&self, query: &EventsQuery) -> Result<Vec<Event>, String> {
        let mut clauses: Vec<String> = Vec::new();
        let mut values: Vec<SqlValue> = Vec::new();
        let mut push = |clause: &str, value: SqlValue| {
            values.push(value);
            clauses.push(clause.replace('?', &format!("?{}", values.len())));
        };
        if let Some(after) = query.after_seq {
            push("seq > ?", SqlValue::Integer(after));
        }
        if let Some(before) = query.before_seq {
            push("seq < ?", SqlValue::Integer(before));
        }
        fn text(value: &Option<String>) -> Option<&str> {
            value.as_deref().map(str::trim).filter(|v| !v.is_empty())
        }
        for (column, value) in [
            ("family", text(&query.family)),
            ("repo", text(&query.repo)),
            ("todo_id", text(&query.todo_id)),
            ("source", text(&query.source)),
        ] {
            if let Some(value) = value {
                push(&format!("{column} = ?"), SqlValue::Text(value.to_string()));
            }
        }
        if let Some(pr) = query.pr {
            push("pr = ?", SqlValue::Integer(pr));
        }
        if let Some(kind) = text(&query.kind) {
            if kind.ends_with('.') {
                // A prefix such as `todo.`: compare the leading characters, so
                // no LIKE wildcard in the value can widen the match. Both
                // placeholders become the same numbered parameter.
                push(
                    "substr(kind, 1, length(?)) = ?",
                    SqlValue::Text(kind.to_string()),
                );
            } else {
                push("kind = ?", SqlValue::Text(kind.to_string()));
            }
        }
        if let Some(needs_human) = query.needs_human {
            push("needs_human = ?", SqlValue::Integer(i64::from(needs_human)));
        }
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let order = if query.after_seq.is_some() {
            "ASC"
        } else {
            "DESC"
        };
        let filter = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let sql = format!(
            "SELECT {COLUMNS} FROM pipeline_events {filter} ORDER BY seq {order} LIMIT {limit}"
        );
        let inner = self.inner.lock().expect("pipeline event mutex poisoned");
        let mut stmt = inner.conn.prepare(&sql).map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params_from_iter(values.iter()), event_from_row)
            .map_err(|err| err.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())
    }

    /// The highest sequence ever assigned (0 for an empty log).
    pub(crate) fn latest_seq(&self) -> Result<i64, String> {
        let inner = self.inner.lock().expect("pipeline event mutex poisoned");
        inner
            .conn
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM pipeline_events",
                [],
                |row| row.get(0),
            )
            .map_err(|err| err.to_string())
    }

    /// The newest event of exactly `kind`, if any.
    pub(crate) fn newest_of_kind(&self, kind: &str) -> Result<Option<Event>, String> {
        Ok(self
            .query(&EventsQuery {
                kind: Some(kind.to_string()),
                limit: Some(1),
                ..EventsQuery::default()
            })?
            .into_iter()
            .next())
    }
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    let detail: Option<String> = row.get(20)?;
    Ok(Event {
        seq: row.get(0)?,
        ts: rfc3339_ms(row.get(1)?),
        source: row.get(2)?,
        kind: row.get(3)?,
        reporter: row.get(4)?,
        actor: row.get(5)?,
        family: row.get(6)?,
        repo: row.get(7)?,
        pr: row.get(8)?,
        sha: row.get(9)?,
        todo_id: row.get(10)?,
        shift: row.get(11)?,
        outcome: row.get(12)?,
        needs_human: row.get(13)?,
        summary: row.get(14)?,
        reason: row.get(15)?,
        cost_usd: row.get(16)?,
        seconds: row.get(17)?,
        log_tail: row.get(18)?,
        log_url: row.get(19)?,
        detail: detail.and_then(|d| serde_json::from_str(&d).ok()),
    })
}
