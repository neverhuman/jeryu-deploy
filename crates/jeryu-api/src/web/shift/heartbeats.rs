//! Slot heartbeat history in `<data_dir>/shift.sqlite`.
//!
//! Schema comes only from the reviewed files in `db/migrations/`, applied in
//! order and checksum-bound. Rows older than 14 days are pruned (at most once
//! an hour, on write).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::types::{CapacityBucket, Heartbeat, HistoryResponse, Segment, SlotHistory};

const MIGRATIONS: &[(i64, &str, &str)] = &[
    (
        1,
        "0001_shift_heartbeats",
        include_str!("../../../../../db/migrations/0001_shift_heartbeats.sql"),
    ),
    (
        2,
        "0002_pipeline_events",
        include_str!("../../../../../db/migrations/0002_pipeline_events.sql"),
    ),
    (
        3,
        "0003_jankurai_disputes",
        include_str!("../../../../../db/migrations/0003_jankurai_disputes.sql"),
    ),
];

pub(crate) const RETENTION_MS: i64 = 14 * 24 * 60 * 60 * 1000;
pub(crate) const HEALTHY_MS: i64 = 120 * 1000;
const PRUNE_EVERY_MS: i64 = 60 * 60 * 1000;
const HOUR_MS: i64 = 60 * 60 * 1000;

/// A heartbeat row with when it arrived.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StoredHeartbeat {
    pub received_ms: i64,
    pub heartbeat: Heartbeat,
}

#[derive(Clone)]
pub(crate) struct HeartbeatStore {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    conn: Connection,
    last_prune_ms: i64,
}

impl HeartbeatStore {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|err| err.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|err| err.to_string())?;
        migrate(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                conn,
                last_prune_ms: 0,
            })),
        })
    }

    pub(crate) fn insert(
        &self,
        reporter: &str,
        heartbeat: &Heartbeat,
        now_ms: i64,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("shift heartbeat mutex poisoned");
        let schedule = heartbeat.schedule.as_ref().map(|v| v.to_string());
        inner
            .conn
            .execute(
                "INSERT INTO shift_heartbeats (received_ms, reporter, operator, host, slot, family,
                   state, todo_id, stage, lease_until, shift, planned_slots, schedule_json, version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    now_ms,
                    reporter,
                    heartbeat.operator,
                    heartbeat.host,
                    heartbeat.slot,
                    heartbeat.family,
                    heartbeat.state,
                    heartbeat.todo_id,
                    heartbeat.stage,
                    heartbeat.lease_until,
                    heartbeat.shift,
                    heartbeat.planned_slots,
                    schedule,
                    heartbeat.version,
                ],
            )
            .map_err(|err| err.to_string())?;
        if now_ms - inner.last_prune_ms >= PRUNE_EVERY_MS {
            inner
                .conn
                .execute(
                    "DELETE FROM shift_heartbeats WHERE received_ms < ?1",
                    params![now_ms - RETENTION_MS],
                )
                .map_err(|err| err.to_string())?;
            inner.last_prune_ms = now_ms;
        }
        Ok(())
    }

    /// The latest heartbeat of every slot seen since `since_ms`.
    pub(crate) fn latest(&self, since_ms: i64) -> Result<Vec<StoredHeartbeat>, String> {
        self.select(
            "SELECT h.received_ms, h.operator, h.host, h.slot, h.family, h.state, h.todo_id,
                    h.stage, h.lease_until, h.shift, h.planned_slots, h.schedule_json, h.version
               FROM shift_heartbeats h
               JOIN (SELECT MAX(id) AS id FROM shift_heartbeats WHERE received_ms >= ?1
                      GROUP BY operator, host, slot, family) m ON h.id = m.id
              ORDER BY h.family, h.operator, h.host, h.slot",
            params![since_ms],
        )
    }

    /// The newest stored heartbeat of `heartbeat`'s slot, if it ever reported.
    pub(crate) fn latest_for_slot(
        &self,
        heartbeat: &Heartbeat,
    ) -> Result<Option<StoredHeartbeat>, String> {
        Ok(self
            .select(
                "SELECT received_ms, operator, host, slot, family, state, todo_id, stage,
                        lease_until, shift, planned_slots, schedule_json, version
                   FROM shift_heartbeats
                  WHERE operator = ?1 AND host = ?2 AND slot = ?3 AND family = ?4
                  ORDER BY id DESC LIMIT 1",
                params![
                    heartbeat.operator,
                    heartbeat.host,
                    heartbeat.slot,
                    heartbeat.family
                ],
            )?
            .into_iter()
            .next())
    }

    /// Every heartbeat in `[from_ms, to_ms]`, oldest first.
    pub(crate) fn between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<StoredHeartbeat>, String> {
        self.select(
            "SELECT received_ms, operator, host, slot, family, state, todo_id, stage, lease_until,
                    shift, planned_slots, schedule_json, version
               FROM shift_heartbeats WHERE received_ms >= ?1 AND received_ms <= ?2
              ORDER BY received_ms, id",
            params![from_ms, to_ms],
        )
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> i64 {
        let inner = self.inner.lock().expect("shift heartbeat mutex poisoned");
        inner
            .conn
            .query_row("SELECT COUNT(*) FROM shift_heartbeats", [], |row| {
                row.get(0)
            })
            .expect("count heartbeats")
    }

    fn select(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<StoredHeartbeat>, String> {
        let inner = self.inner.lock().expect("shift heartbeat mutex poisoned");
        let mut stmt = inner.conn.prepare(sql).map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params, |row| {
                let schedule: Option<String> = row.get(11)?;
                Ok(StoredHeartbeat {
                    received_ms: row.get(0)?,
                    heartbeat: Heartbeat {
                        operator: row.get(1)?,
                        host: row.get(2)?,
                        slot: row.get(3)?,
                        family: row.get(4)?,
                        state: row.get(5)?,
                        todo_id: row.get(6)?,
                        stage: row.get(7)?,
                        lease_until: row.get(8)?,
                        shift: row.get(9)?,
                        planned_slots: row.get(10)?,
                        schedule: schedule.and_then(|s| serde_json::from_str(&s).ok()),
                        version: row.get(12)?,
                    },
                })
            })
            .map_err(|err| err.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())
    }
}

/// Apply every shift.sqlite migration not yet recorded. Idempotent, so each
/// store sharing the file (heartbeats, pipeline events) may call it on open.
pub(crate) fn migrate(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS shift_schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            sha256 TEXT NOT NULL,
            applied_ms INTEGER NOT NULL
        )",
    )
    .map_err(|err| err.to_string())?;
    for (version, name, sql) in MIGRATIONS {
        let checksum = hex::encode(Sha256::digest(sql.as_bytes()));
        let applied: Option<String> = conn
            .query_row(
                "SELECT sha256 FROM shift_schema_migrations WHERE version = ?1",
                params![version],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| err.to_string())?;
        match applied {
            Some(existing) if existing == checksum => {}
            Some(existing) => {
                return Err(format!(
                    "migration {name} changed after it was applied ({existing} != {checksum})"
                ));
            }
            None => {
                let tx = conn
                    .unchecked_transaction()
                    .map_err(|err| err.to_string())?;
                tx.execute_batch(sql).map_err(|err| err.to_string())?;
                tx.execute(
                    "INSERT INTO shift_schema_migrations (version, name, sha256, applied_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![version, name, checksum, Utc::now().timestamp_millis()],
                )
                .map_err(|err| err.to_string())?;
                tx.commit().map_err(|err| err.to_string())?;
            }
        }
    }
    Ok(())
}

pub(crate) fn rfc3339_ms(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch"))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

type SlotKey = (String, String, String, String);

fn slot_key(hb: &Heartbeat) -> SlotKey {
    (
        hb.operator.clone(),
        hb.host.clone(),
        hb.slot.clone(),
        hb.family.clone(),
    )
}

/// Turn raw heartbeats into per-slot swim-lane segments and hourly capacity.
///
/// Consecutive heartbeats with the same state, todo and stage merge into one
/// segment. A segment runs until the slot's next heartbeat, but never more
/// than [`HEALTHY_MS`] past its last one: a silent slot shows a gap.
/// Capacity buckets are whole UTC hours: `planned` sums, per
/// operator/host/family, the largest `planned_slots` reported in the hour;
/// `busy` counts slots that reported `working` in the hour.
pub(crate) fn build_history(rows: &[StoredHeartbeat], from_ms: i64, to_ms: i64) -> HistoryResponse {
    let mut by_slot: BTreeMap<SlotKey, Vec<&StoredHeartbeat>> = BTreeMap::new();
    for row in rows {
        by_slot
            .entry(slot_key(&row.heartbeat))
            .or_default()
            .push(row);
    }
    let mut slots = Vec::new();
    for ((operator, host, slot, family), beats) in by_slot {
        let mut segments: Vec<(i64, i64, &Heartbeat)> = Vec::new();
        for (index, beat) in beats.iter().enumerate() {
            let next = beats.get(index + 1).map(|b| b.received_ms);
            let end = next
                .unwrap_or(i64::MAX)
                .min(beat.received_ms.saturating_add(HEALTHY_MS))
                .min(to_ms);
            let start = beat.received_ms.max(from_ms);
            if end <= start {
                continue;
            }
            let hb = &beat.heartbeat;
            if let Some(last) = segments.last_mut()
                && last.1 == start
                && last.2.state == hb.state
                && last.2.todo_id == hb.todo_id
                && last.2.stage == hb.stage
            {
                last.1 = end;
                continue;
            }
            segments.push((start, end, hb));
        }
        if segments.is_empty() {
            continue;
        }
        slots.push(SlotHistory {
            operator,
            host,
            slot,
            family,
            segments: segments
                .into_iter()
                .map(|(start, end, hb)| Segment {
                    from: rfc3339_ms(start),
                    to: rfc3339_ms(end),
                    state: hb.state.clone(),
                    todo_id: hb.todo_id.clone(),
                    stage: hb.stage.clone(),
                })
                .collect(),
        });
    }

    let first_bucket = from_ms.div_euclid(HOUR_MS) * HOUR_MS;
    let mut capacity = Vec::new();
    let mut bucket = first_bucket;
    while bucket <= to_ms {
        let end = bucket + HOUR_MS;
        let mut planned: BTreeMap<(String, String, String), i64> = BTreeMap::new();
        let mut busy: BTreeSet<SlotKey> = BTreeSet::new();
        for row in rows
            .iter()
            .filter(|r| r.received_ms >= bucket && r.received_ms < end)
        {
            let hb = &row.heartbeat;
            if let Some(n) = hb.planned_slots {
                let key = (hb.operator.clone(), hb.host.clone(), hb.family.clone());
                let entry = planned.entry(key).or_insert(0);
                *entry = (*entry).max(n);
            }
            if hb.state == "working" {
                busy.insert(slot_key(hb));
            }
        }
        capacity.push(CapacityBucket {
            at: rfc3339_ms(bucket),
            planned: planned.values().sum(),
            busy: busy.len() as i64,
            queue_depth: None,
        });
        bucket = end;
    }

    HistoryResponse {
        from: rfc3339_ms(from_ms),
        to: rfc3339_ms(to_ms),
        slots,
        capacity,
    }
}
