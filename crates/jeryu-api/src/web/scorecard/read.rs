//! The Scorecard's reads: one pass over the ledgers and the retained event
//! log, under the event store's lock, parsed into [`Raw`] rows.

use rusqlite::{Connection, params};

use super::compute::{CostRow, EventRow, FlowRow, Raw};
use crate::web::flow::Station;

/// Event kinds the Scorecard reads.
const KINDS: &[&str] = &[
    "release.staged",
    "deploy.status",
    "deploy.created",
    "gate.started",
    "gate.finished",
    "todo.action",
    "todo.attempt_finished",
    "pr.review",
    "pr.reviewed",
    "review.started",
    "review.finished",
];

/// Flow visits of lots still on the line or that left a station since
/// `flow_since_ms`, every cost row of `family`, and its events since
/// `events_since_ms`.
pub(crate) fn read(
    conn: &Connection,
    family: Option<&str>,
    flow_since_ms: i64,
    events_since_ms: i64,
) -> Result<Raw, String> {
    let err = |e: rusqlite::Error| e.to_string();
    let mut raw = Raw::default();

    let mut stmt = conn
        .prepare(
            "SELECT lot_id, repo, station, entered_ms, started_ms, left_ms, direction
               FROM flow_ledger
              WHERE (?1 IS NULL OR family = ?1)
                AND lot_id IN (SELECT lot_id FROM flow_ledger
                                WHERE (left_ms IS NULL OR left_ms >= ?2)
                                  AND (?1 IS NULL OR family = ?1))
              ORDER BY seq",
        )
        .map_err(err)?;
    let rows = stmt
        .query_map(params![family, flow_since_ms], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(err)?;
    for row in rows {
        let (lot_id, repo, station, entered_ms, started_ms, left_ms, direction) =
            row.map_err(err)?;
        let Some(station) = Station::parse(&station) else {
            continue;
        };
        raw.flow.push(FlowRow {
            lot_id,
            repo,
            station,
            entered_ms,
            started_ms,
            left_ms,
            direction,
        });
    }
    drop(stmt);

    let mut stmt = conn
        .prepare(
            "SELECT ts_ms, kind, repo, pr, todo_id, cost_usd, seconds
               FROM cost_ledger WHERE (?1 IS NULL OR family = ?1) ORDER BY seq",
        )
        .map_err(err)?;
    let rows = stmt
        .query_map(params![family], |row| {
            Ok(CostRow {
                ts_ms: row.get(0)?,
                kind: row.get(1)?,
                repo: row.get(2)?,
                pr: row.get(3)?,
                todo_id: row.get(4)?,
                cost_usd: row.get(5)?,
                seconds: row.get(6)?,
            })
        })
        .map_err(err)?;
    raw.costs = rows.collect::<Result<_, _>>().map_err(err)?;
    drop(stmt);

    let kinds = KINDS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT seq, ts_ms, kind, repo, pr, sha, todo_id, outcome, detail_json
           FROM pipeline_events
          WHERE kind IN ({kinds}) AND ts_ms >= ?2 AND (?1 IS NULL OR family = ?1)
          ORDER BY seq"
    );
    let mut stmt = conn.prepare(&sql).map_err(err)?;
    let rows = stmt
        .query_map(params![family, events_since_ms], |row| {
            let detail: Option<String> = row.get(8)?;
            Ok(EventRow {
                seq: row.get(0)?,
                ts_ms: row.get(1)?,
                kind: row.get(2)?,
                repo: row.get(3)?,
                pr: row.get(4)?,
                sha: row.get(5)?,
                todo_id: row.get(6)?,
                outcome: row.get(7)?,
                detail: detail.and_then(|d| serde_json::from_str(&d).ok()),
            })
        })
        .map_err(err)?;
    raw.events = rows.collect::<Result<_, _>>().map_err(err)?;
    drop(stmt);

    let first = |sql: &str| -> Result<Option<i64>, String> {
        conn.query_row(sql, [], |row| row.get(0)).map_err(err)
    };
    raw.flow_from = first("SELECT MIN(entered_ms) FROM flow_ledger")?;
    raw.events_from = first("SELECT MIN(ts_ms) FROM pipeline_events")?;
    Ok(raw)
}
