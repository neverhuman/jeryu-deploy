//! Durable transitions: daily counts and a bounded, filterable evidence list.
use super::{Station, project::EventView};
use crate::web::{
    WebState,
    strict_query::{StrictFields, StrictQuery},
};
use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const STATES: [&str; 7] = [
    "arrived", "started", "passed", "rework", "held", "released", "failed",
];

#[derive(Default, Deserialize)]
pub(crate) struct Query {
    pub family: Option<String>,
    pub repo: Option<String>,
    pub station: Option<String>,
    pub days: Option<i64>,
    pub day: Option<String>,
    pub state: Option<String>,
    pub lot: Option<String>,
    pub offset: Option<usize>,
}
impl StrictFields for Query {
    const KEYS: &'static [&'static str] = &[
        "family", "repo", "station", "days", "day", "state", "lot", "offset",
    ];
    const REPAIR_HINT: &'static str =
        "choose a station, days=7|28|90, UTC day YYYY-MM-DD and a recorded transition state";
    fn check_values(&self) -> Result<(), String> {
        if self.repo.as_ref().is_some_and(|r| {
            r.len() > 200 || r.split('/').count() != 2 || r.split('/').any(str::is_empty)
        }) {
            return Err("repo must be owner/name".into());
        }
        if !super::WINDOWS.contains(&self.days.unwrap_or(28)) {
            return Err("unsupported window".into());
        }
        if self
            .station
            .as_deref()
            .is_some_and(|s| Station::parse(s).is_none())
        {
            return Err("unknown station".into());
        }
        if self.state.as_deref().is_some_and(|s| !STATES.contains(&s)) {
            return Err("unknown transition state".into());
        }
        if self.day.as_deref().is_some_and(|d| {
            d.len() != 10 || chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_err()
        }) {
            return Err("invalid UTC day".into());
        }
        if self.lot.as_ref().is_some_and(|s| s.len() > 200) || self.offset.unwrap_or(0) > 100_000 {
            return Err("filter exceeds bound".into());
        }
        Ok(())
    }
}
#[derive(Serialize)]
pub(crate) struct Item {
    pub event_seq: i64,
    pub lot_id: String,
    pub station: String,
    pub state: String,
    pub ts: String,
    pub repo: Option<String>,
    pub pr: Option<i64>,
    pub sha: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub runner: Option<String>,
    pub log_url: Option<String>,
    pub href: String,
}
#[derive(Serialize)]
pub(crate) struct History {
    pub daily: BTreeMap<String, BTreeMap<String, u64>>,
    pub items: Vec<Item>,
    pub total: usize,
    pub offset: usize,
    pub detailed_since: String,
    pub related_items: Vec<serde_json::Value>,
    pub deliveries: Vec<serde_json::Value>,
    pub timezone: &'static str,
}

pub(crate) async fn history(
    State(state): State<Arc<WebState>>,
    StrictQuery(mut query): StrictQuery<Query>,
) -> Response {
    query.family = match crate::web::family::filter(&state, query.family.as_deref()) {
        Ok(f) => f,
        Err(r) => return *r,
    };
    let read = tokio::task::spawn_blocking(move || {
        let mut history = state
            .events
            .flow_history(&query, Utc::now().timestamp_millis())?;
        for item in &mut history.items {
            // The recorded slug may be a repository's earlier name; its
            // title is read under the slug it is hosted under now.
            if let (Some(repo), Some(pr)) = (&item.repo, item.pr)
                && let Ok(number) = u64::try_from(pr)
                && let Some(pull) = super::identity::pull(&state, repo, number)
            {
                item.title = Some(pull.title);
            }
        }
        Ok::<_, String>(history)
    })
    .await;
    match read {
        Ok(Ok(data)) => Json(data).into_response(),
        _ => crate::web::api_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "flow_history_failed",
            "Could not read station history",
        ),
    }
}

pub(crate) fn record(
    conn: &Connection,
    event: &EventView<'_>,
    lot: &str,
    station: Station,
    state: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT OR IGNORE INTO flow_activity
         (event_seq,lot_id,station,state,ts_ms,family,repo,pr,sha,summary,runner,log_url)
         SELECT ?1,?2,?3,?4,?5,COALESCE(?6,family),repo,pr,?7,?8,
           (SELECT COALESCE(json_extract(detail_json,'$.runner_id'),json_extract(detail_json,'$.runner')) FROM pipeline_events WHERE seq=?1),
           (SELECT log_url FROM pipeline_events WHERE seq=?1)
         FROM flow_ledger WHERE lot_id=?2 AND station=?3 ORDER BY seq DESC LIMIT 1",
        params![event.seq,lot,station.id(),state,event.ts_ms,event.family,event.sha,event.summary],
    ).map_err(|e|e.to_string())?;
    Ok(())
}

pub(crate) fn read(conn: &Connection, query: &Query, now: i64) -> Result<History, String> {
    let days = query.days.unwrap_or(28);
    // Include today and exactly days-1 preceding UTC calendar days.
    let since = now.div_euclid(86_400_000) * 86_400_000 - (days - 1) * 86_400_000;
    let mut daily = BTreeMap::new();
    for n in 0..days {
        daily.insert(
            crate::web::shift::rfc3339_ms(since + n * 86_400_000)[..10].to_string(),
            BTreeMap::new(),
        );
    }
    let mut stmt = conn
        .prepare(
            "SELECT substr(strftime('%Y-%m-%d',ts_ms/1000,'unixepoch'),1,10),state,COUNT(*)
        FROM flow_activity WHERE ts_ms>=?1 AND ts_ms<=?2 AND (?3 IS NULL OR family=?3)
        AND (?4 IS NULL OR station=?4) AND (?5 IS NULL OR lot_id=?5) AND (?9 IS NULL OR repo=?9) GROUP BY 1,2",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(
            params![
                since,
                now,
                query.family,
                query.station,
                query.lot,
                query.day,
                query.state,
                query.offset.unwrap_or(0),
                query.repo
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u64>(2)?,
                ))
            },
        )
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (day, state, count) = row.map_err(|e| e.to_string())?;
        daily.entry(day).or_default().insert(state, count);
    }
    let total = conn
        .query_row(
            "SELECT COUNT(*) FROM flow_activity WHERE ts_ms>=?1 AND ts_ms<=?2 AND (?3 IS NULL OR family=?3)
        AND (?4 IS NULL OR station=?4) AND (?5 IS NULL OR lot_id=?5) AND (?9 IS NULL OR repo=?9)
        AND (?6 IS NULL OR strftime('%Y-%m-%d',ts_ms/1000,'unixepoch')=?6) AND (?7 IS NULL OR state=?7)",
            params![
                since,
                now,
                query.family,
                query.station,
                query.lot,
                query.day,
                query.state,
                query.offset.unwrap_or(0),
                query.repo
            ],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    let mut stmt=conn.prepare("SELECT event_seq,lot_id,station,state,ts_ms,repo,pr,sha,summary,runner,log_url FROM flow_activity WHERE ts_ms>=?1 AND ts_ms<=?2 AND (?3 IS NULL OR family=?3)
        AND (?4 IS NULL OR station=?4) AND (?5 IS NULL OR lot_id=?5) AND (?9 IS NULL OR repo=?9)
        AND (?6 IS NULL OR strftime('%Y-%m-%d',ts_ms/1000,'unixepoch')=?6) AND (?7 IS NULL OR state=?7) ORDER BY ts_ms DESC,event_seq DESC,lot_id LIMIT 100 OFFSET ?8").map_err(|e|e.to_string())?;
    let items = stmt
        .query_map(
            params![
                since,
                now,
                query.family,
                query.station,
                query.lot,
                query.day,
                query.state,
                query.offset.unwrap_or(0),
                query.repo
            ],
            |r| {
                let lot_id: String = r.get(1)?;
                Ok(Item {
                    event_seq: r.get(0)?,
                    href: super::report::lot_href(&lot_id),
                    lot_id,
                    station: r.get(2)?,
                    state: r.get(3)?,
                    ts: crate::web::shift::rfc3339_ms(r.get(4)?),
                    repo: r.get(5)?,
                    pr: r.get(6)?,
                    sha: r.get(7)?,
                    title: None,
                    summary: r.get(8)?,
                    runner: r.get(9)?,
                    log_url: r.get(10)?,
                })
            },
        )
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let start = conn
        .query_row(
            "SELECT since_ms FROM flow_evidence_meta WHERE version=1",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    let mut stmt=conn.prepare("SELECT DISTINCT repo,revision,release,kind,ts_ms FROM flow_delivery_receipts WHERE lot_id=?1 AND EXISTS(SELECT 1 FROM flow_ledger WHERE lot_id=?1 AND (?2 IS NULL OR family=?2)) ORDER BY ts_ms DESC LIMIT 100").map_err(|e|e.to_string())?;
    let deliveries=stmt.query_map(params![query.lot,query.family],|r|Ok(serde_json::json!({"repo":r.get::<_,String>(0)?,"revision":r.get::<_,String>(1)?,"release":r.get::<_,Option<String>>(2)?,"kind":r.get::<_,String>(3)?,"ts":crate::web::shift::rfc3339_ms(r.get(4)?)}))).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
    let mut stmt=conn.prepare("SELECT DISTINCT b.lot_id FROM flow_delivery_receipts a JOIN flow_delivery_receipts b ON a.event_seq=b.event_seq WHERE a.lot_id=?1 AND b.lot_id!=?1 AND EXISTS(SELECT 1 FROM flow_ledger WHERE lot_id=a.lot_id AND (?2 IS NULL OR family=?2)) AND EXISTS(SELECT 1 FROM flow_ledger WHERE lot_id=b.lot_id AND (?2 IS NULL OR family=?2)) ORDER BY b.lot_id LIMIT 100").map_err(|e|e.to_string())?;
    let related_items = stmt
        .query_map(params![query.lot, query.family], |r| {
            let id: String = r.get(0)?;
            Ok(serde_json::json!({"lot_id":id,"href":super::report::lot_href(&id)}))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(History {
        related_items,
        deliveries,
        daily,
        items,
        total,
        offset: query.offset.unwrap_or(0),
        detailed_since: crate::web::shift::rfc3339_ms(start),
        timezone: "UTC",
    })
}
