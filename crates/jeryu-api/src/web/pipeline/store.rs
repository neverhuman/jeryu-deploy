//! The pipeline event log in `<data_dir>/shift.sqlite`.
//!
//! Append-only: rows are inserted and, after 30 days, pruned (at most once an
//! hour, on write); nothing updates a row. An event that carries a `cost_usd`
//! is also written to `cost_ledger` (`db/migrations/0010_cost_ledger.sql`),
//! and every event is projected into `flow_ledger`
//! (`db/migrations/0011_flow_ledger.sql`, the fold in `web/flow/project.rs`);
//! neither ledger is ever pruned. The schema comes from the reviewed
//! `db/migrations/0002_pipeline_events.sql`, applied by the shift migration
//! runner. The store holds its own connection to the file the heartbeat store
//! also uses; sqlite serialises the two writers and `busy_timeout` covers the
//! overlap.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};

use super::super::flow::{Station, Visit, project};
use super::super::shift::{migrate_shift_store, rfc3339_ms};
use super::types::{Event, EventsQuery, NewEvent};

pub(crate) const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const PRUNE_EVERY_MS: i64 = 60 * 60 * 1000;
pub(crate) const DEFAULT_LIMIT: i64 = 100;
pub(crate) const MAX_LIMIT: i64 = 500;

const COLUMNS: &str = "seq, ts_ms, event_id, source, kind, reporter, actor, family, repo, pr, sha, todo_id,
    shift, outcome, needs_human, summary, reason, cost_usd, seconds, log_tail, log_url, detail_json";

/// One row of the durable cost ledger (`db/migrations/0010_cost_ledger.sql`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LedgerRow {
    pub repo: Option<String>,
    pub pr: Option<i64>,
    pub cost_usd: f64,
    pub detail: Option<serde_json::Value>,
}

/// What [`EventStore::insert`] did.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Inserted {
    pub event: Event,
    /// True when the reporter had already stored this `event_id`.
    pub duplicate: bool,
}

#[derive(Clone)]
pub(crate) struct EventStore {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    conn: Connection,
    last_prune_ms: i64,
}

/// The row limit a query reads with: its own `limit`, or [`DEFAULT_LIMIT`].
/// The route refuses an out-of-range value before the store is reached, so the
/// clamp here only guards a caller inside the server.
fn page_limit(query: &EventsQuery) -> i64 {
    query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

impl EventStore {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|err| err.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|err| err.to_string())?;
        migrate_shift_store(&conn)?;
        // A failed replay must not keep the event log from starting: its
        // transaction rolls back, no replay is recorded, and the next start
        // tries again. Until then Flow answers from what the insert path has
        // projected since.
        if let Err(err) = replay_flow_ledger(&conn) {
            eprintln!(
                "jeryu-api pipeline: flow ledger replay failed, retried at next start: {err}"
            );
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                conn,
                last_prune_ms: 0,
            })),
        })
    }

    /// Append one already-normalised event and return it as stored. When the
    /// reporter already stored an event with the same `event_id`, nothing is
    /// written and that event comes back with `duplicate = true`.
    pub(crate) fn insert(
        &self,
        reporter: &str,
        event: &NewEvent,
        now_ms: i64,
    ) -> Result<Inserted, String> {
        let mut inner = self.inner.lock().expect("pipeline event mutex poisoned");
        if let Some(event_id) = &event.event_id {
            let existing = inner
                .conn
                .query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM pipeline_events
                          WHERE reporter = ?1 AND event_id = ?2"
                    ),
                    params![reporter, event_id],
                    event_from_row,
                )
                .optional()
                .map_err(|err| err.to_string())?;
            if let Some(event) = existing {
                return Ok(Inserted {
                    event,
                    duplicate: true,
                });
            }
        }
        let detail = event.detail.as_ref().map(|d| d.to_string());
        // The event and its ledger row commit together: a cost is never in
        // the log without the ledger, nor in the ledger without the log.
        let tx = inner
            .conn
            .unchecked_transaction()
            .map_err(|err| err.to_string())?;
        tx.execute(
            "INSERT INTO pipeline_events (ts_ms, event_id, source, kind, reporter, actor,
                   family, repo, pr, sha, todo_id, shift, outcome, needs_human, summary, reason,
                   cost_usd, seconds, log_tail, log_url, detail_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                   ?17, ?18, ?19, ?20, ?21)",
            params![
                now_ms,
                event.event_id,
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
        let seq = tx.last_insert_rowid();
        if event.cost_usd.is_some() {
            // OR IGNORE: an event pruned from the log and posted again under
            // the same event_id is already in the ledger, and counts once.
            tx.execute(
                "INSERT OR IGNORE INTO cost_ledger (reporter, event_id, ts_ms, source, kind,
                   family, repo, pr, sha, todo_id, shift, outcome, cost_usd, seconds, detail_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    reporter,
                    event.event_id,
                    now_ms,
                    event.source,
                    event.kind,
                    event.family,
                    event.repo,
                    event.pr,
                    event.sha,
                    event.todo_id,
                    event.shift,
                    event.outcome,
                    event.cost_usd,
                    event.seconds,
                    detail,
                ],
            )
            .map_err(|err| err.to_string())?;
        }
        // The flow projection commits with the event too: a station visit
        // is never in the ledger without the event that moved it, nor an
        // event in the log without the visits it opened.
        project::apply(
            &tx,
            &project::EventView {
                seq,
                reporter,
                event_id: event.event_id.as_deref(),
                ts_ms: now_ms,
                kind: &event.kind,
                outcome: event.outcome.as_deref(),
                family: event.family.as_deref(),
                repo: event.repo.as_deref(),
                pr: event.pr,
                todo_id: event.todo_id.as_deref(),
                shift: event.shift.as_deref(),
            },
        )?;
        tx.commit().map_err(|err| err.to_string())?;
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
        let event = Event {
            seq,
            ts: rfc3339_ms(now_ms),
            event_id: event.event_id.clone(),
            source: event.source.clone(),
            kind: event.kind.clone(),
            reporter: reporter.to_string(),
            actor: event.actor.clone(),
            family: event.family.clone(),
            family_label: crate::web::family::optional_label(event.family.as_ref()),
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
        };
        Ok(Inserted {
            event,
            duplicate: false,
        })
    }

    /// Events matching `query`. With `after_seq` the page is a cursor tail,
    /// oldest first; otherwise it is the newest events, newest first.
    pub(crate) fn query(&self, query: &EventsQuery) -> Result<Vec<Event>, String> {
        self.read(query, page_limit(query))
    }

    /// One page of [`Self::query`] plus whether the log holds another row
    /// behind it. The page is read one row longer than asked for; the extra
    /// row, if it came back, is the whole answer, and it is dropped before the
    /// page is returned.
    pub(crate) fn query_page(&self, query: &EventsQuery) -> Result<(Vec<Event>, bool), String> {
        let limit = page_limit(query);
        let mut events = self.read(query, limit + 1)?;
        let has_more = i64::try_from(events.len()).unwrap_or(i64::MAX) > limit;
        events.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok((events, has_more))
    }

    fn read(&self, query: &EventsQuery, limit: i64) -> Result<Vec<Event>, String> {
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
        // A `-split` alias filters the same family as the canonical key.
        let family = query
            .family
            .as_deref()
            .map(crate::web::family::canonical)
            .filter(|family| !family.is_empty());
        for (column, value) in [
            ("family", family.as_deref()),
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

    /// Every ledger row of exactly `kind`, oldest first. The ledger is never
    /// pruned, so this reads spend older than the event log keeps.
    pub(crate) fn ledger(&self, kind: &str) -> Result<Vec<LedgerRow>, String> {
        let inner = self.inner.lock().expect("pipeline event mutex poisoned");
        let mut stmt = inner
            .conn
            .prepare_cached(
                "SELECT repo, pr, cost_usd, detail_json
                   FROM cost_ledger WHERE kind = ?1 ORDER BY seq",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params![kind], |row| {
                let detail: Option<String> = row.get(3)?;
                Ok(LedgerRow {
                    repo: row.get(0)?,
                    pr: row.get(1)?,
                    cost_usd: row.get(2)?,
                    detail: detail.and_then(|d| serde_json::from_str(&d).ok()),
                })
            })
            .map_err(|err| err.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())
    }

    /// The station visits `GET /api/v1/flow` needs for `family` (every family
    /// when it is `None`) over a window that starts at `since_ms`, oldest
    /// first.
    ///
    /// The bound: every visit of every lot that is still on the line (a visit
    /// with no `left_ms`) or that left a station at or after `since_ms`, and
    /// nothing else. That is exactly what the report reads, because every
    /// window metric is keyed on a visit that started or left inside the
    /// window (a visit cannot start in the window and leave before it), and
    /// the per-lot figures (cycle time, X-factor, first pass, ETA) read whole
    /// lots, which the `lot_id IN` keeps whole. A lot that finished before the
    /// window is never read, so a request costs the window's lots plus the
    /// work in flight, not the ledger's lifetime; `flow_ledger_left` and
    /// `flow_ledger_lot` serve the two halves. The event-writer mutex is held
    /// only while the rows are read: parsing and the arithmetic run after it
    /// is released.
    pub(crate) fn flow_visits(
        &self,
        family: Option<&str>,
        since_ms: i64,
    ) -> Result<Vec<Visit>, String> {
        type Row = (
            String,
            String,
            i64,
            Option<i64>,
            Option<i64>,
            String,
            Option<String>,
        );
        let rows: Vec<Row> = {
            let inner = self.inner.lock().expect("pipeline event mutex poisoned");
            let mut stmt = inner
                .conn
                .prepare_cached(
                    "SELECT lot_id, station, entered_ms, started_ms, left_ms, direction, rework_to
                       FROM flow_ledger
                      WHERE (?1 IS NULL OR family = ?1)
                        AND lot_id IN (
                            SELECT lot_id FROM flow_ledger
                             WHERE (left_ms IS NULL OR left_ms >= ?2)
                               AND (?1 IS NULL OR family = ?1))
                      ORDER BY seq",
                )
                .map_err(|err| err.to_string())?;
            let rows = stmt
                .query_map(params![family, since_ms], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                })
                .map_err(|err| err.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|err| err.to_string())?
        };
        let mut visits = Vec::with_capacity(rows.len());
        for (lot_id, station, entered_ms, started_ms, left_ms, direction, rework_to) in rows {
            // A row naming a station this binary does not know is left out
            // rather than guessed at.
            let Some(station) = Station::parse(&station) else {
                continue;
            };
            visits.push(Visit {
                lot_id,
                station,
                entered_ms,
                started_ms,
                left_ms,
                direction,
                rework_to: rework_to.as_deref().and_then(Station::parse),
            });
        }
        Ok(visits)
    }

    /// Every station a lot of `family` (any family when `None`) has ever
    /// visited, whether or not the visit is in the window [`Self::flow_visits`]
    /// reads. One indexed probe per station (`flow_ledger_station_family`),
    /// each stopping at its first row.
    pub(crate) fn flow_reached(&self, family: Option<&str>) -> Result<BTreeSet<Station>, String> {
        let inner = self.inner.lock().expect("pipeline event mutex poisoned");
        let sql = if family.is_some() {
            "SELECT 1 FROM flow_ledger WHERE station = ?1 AND family = ?2 LIMIT 1"
        } else {
            "SELECT 1 FROM flow_ledger WHERE station = ?1 AND ?2 IS NULL LIMIT 1"
        };
        let mut stmt = inner
            .conn
            .prepare_cached(sql)
            .map_err(|err| err.to_string())?;
        let mut reached = BTreeSet::new();
        for station in Station::ALL {
            let hit = stmt
                .query_row(params![station.id(), family], |row| row.get::<_, i64>(0))
                .optional()
                .map_err(|err| err.to_string())?;
            if hit.is_some() {
                reached.insert(station);
            }
        }
        Ok(reached)
    }

    /// Every family the log has an event for, canonical keys, sorted.
    pub(crate) fn families(&self) -> Result<Vec<String>, String> {
        let inner = self.inner.lock().expect("pipeline event mutex poisoned");
        let mut stmt = inner
            .conn
            .prepare("SELECT DISTINCT family FROM pipeline_events WHERE family IS NOT NULL ORDER BY family")
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
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

    /// The newest event of exactly `kind` for each repository that has one,
    /// newest repository first. An event that names no repository is its own
    /// scope, so a forge-wide one is kept too.
    pub(crate) fn newest_of_kind_per_repo(&self, kind: &str) -> Result<Vec<Event>, String> {
        let mut seen: Vec<Option<String>> = Vec::new();
        let mut newest = Vec::new();
        for event in self.query(&EventsQuery {
            kind: Some(kind.to_string()),
            limit: Some(MAX_LIMIT),
            ..EventsQuery::default()
        })? {
            // The query answers newest first, so the first event of a scope is
            // that scope's newest.
            if !seen.contains(&event.repo) {
                seen.push(event.repo.clone());
                newest.push(event);
            }
        }
        Ok(newest)
    }

    /// How long the newest finished passes of `kind` on `repo` took, newest
    /// first: at most `limit` values, counting only the events whose outcome is
    /// one of `outcomes` and whose `detail.recipe` is `recipe`. A pass that
    /// failed early says little about how long a whole one takes, so callers
    /// name the outcomes that mean "ran to the end". Every filter runs in
    /// SQLite, so other recipes on the same repository never crowd the window.
    pub(crate) fn finished_seconds(
        &self,
        kind: &str,
        repo: &str,
        recipe: &str,
        outcomes: &[&str],
        limit: usize,
    ) -> Result<Vec<u64>, String> {
        let outcomes = serde_json::to_string(outcomes).map_err(|err| err.to_string())?;
        let limit = i64::try_from(limit).unwrap_or(MAX_LIMIT);
        let inner = self.inner.lock().expect("pipeline event mutex poisoned");
        let mut stmt = inner
            .conn
            .prepare_cached(
                "SELECT seconds FROM pipeline_events
                  WHERE kind = ?1 AND repo = ?2 AND seconds IS NOT NULL
                    AND json_extract(detail_json, '$.recipe') = ?3
                    AND outcome IN (SELECT value FROM json_each(?4))
                  ORDER BY seq DESC LIMIT ?5",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params![kind, repo, recipe, outcomes, limit], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|err| err.to_string())?;
        let mut seconds = Vec::new();
        for row in rows {
            if let Ok(secs) = u64::try_from(row.map_err(|err| err.to_string())?) {
                seconds.push(secs);
            }
        }
        Ok(seconds)
    }
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    let detail: Option<String> = row.get(21)?;
    let family: Option<String> = row.get(7)?;
    Ok(Event {
        seq: row.get(0)?,
        ts: rfc3339_ms(row.get(1)?),
        event_id: row.get(2)?,
        source: row.get(3)?,
        kind: row.get(4)?,
        reporter: row.get(5)?,
        actor: row.get(6)?,
        family_label: crate::web::family::optional_label(family.as_ref()),
        family,
        repo: row.get(8)?,
        pr: row.get(9)?,
        sha: row.get(10)?,
        todo_id: row.get(11)?,
        shift: row.get(12)?,
        outcome: row.get(13)?,
        needs_human: row.get(14)?,
        summary: row.get(15)?,
        reason: row.get(16)?,
        cost_usd: row.get(17)?,
        seconds: row.get(18)?,
        log_tail: row.get(19)?,
        log_url: row.get(20)?,
        detail: detail.and_then(|d| serde_json::from_str(&d).ok()),
    })
}

/// Build `flow_ledger` from every event the log still holds, once.
///
/// The projection is a Rust fold over the event sequence, so migration 0011
/// can only create the table; this is its backfill. The `flow_ledger_replay`
/// row it writes is what makes it happen exactly once: a restart finds the
/// row and leaves the visits it already built alone. A projection bug is
/// fixed by truncating `flow_ledger`, `flow_ledger_events` and
/// `flow_ledger_replay`, which makes the next start replay again.
fn replay_flow_ledger(conn: &Connection) -> Result<(), String> {
    let replayed: i64 = conn
        .query_row("SELECT COUNT(*) FROM flow_ledger_replay", [], |row| {
            row.get(0)
        })
        .map_err(|err| err.to_string())?;
    if replayed > 0 {
        return Ok(());
    }
    let tx = conn
        .unchecked_transaction()
        .map_err(|err| err.to_string())?;
    let (events, visits) = project::replay(&tx)?;
    tx.execute(
        "INSERT INTO flow_ledger_replay (replayed_ms, events, visits) VALUES (?1, ?2, ?3)",
        params![chrono::Utc::now().timestamp_millis(), events, visits],
    )
    .map_err(|err| err.to_string())?;
    tx.commit().map_err(|err| err.to_string())
}
