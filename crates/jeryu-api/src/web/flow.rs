//! `GET /api/v1/flow`: where work sits on the delivery line, how long it
//! waits at each station, where it loops back and which station is the
//! bottleneck.
//!
//! Admin-only like the event log it is projected from (see
//! `auth::admin_only_request`). The answer is read from `flow_ledger`, the
//! never-pruned projection [`project`] writes in the same transaction as each
//! event, so a 90-day window outlives the log's 30-day retention. The
//! arithmetic is in [`report`]; the station list in [`stations`] is shared
//! with `GET /api/v1/flow/layout`. Contract: `docs/flow.md`, ADR 0013.

pub(crate) mod claims;
pub(crate) mod closure;
pub(crate) mod delivery;
pub(crate) mod history;
pub(crate) mod identity;
pub(crate) mod layout;
pub(crate) mod project;
pub(crate) mod report;
pub(crate) mod stations;
pub(crate) mod topology;
pub(crate) mod work;

#[cfg(test)]
mod closure_identity_tests;
#[cfg(test)]
mod hold_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod topology_tests;
#[cfg(test)]
mod work_tests;

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::Utc;
use serde::Deserialize;

use super::WebState;
use super::strict_query::{StrictFields, StrictQuery, one_of};
use super::workcells_support::{TypedError, typed_error};
pub(crate) use layout::layout;
pub(crate) use report::{LotsPage, Request, Visit};
pub(crate) use stations::Station;

const DOCS: &str = "docs/flow.md";
/// The windows the report answers for, in days, and the one it defaults to.
pub(crate) const WINDOWS: [i64; 3] = [7, 28, 90];
pub(crate) const DEFAULT_DAYS: i64 = 28;

/// `GET /api/v1/flow` query.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct FlowQuery {
    /// Either spelling of a family key; the server canonicalises it.
    pub family: Option<String>,
    pub repo: Option<String>,
    /// The window in days: one of [`WINDOWS`].
    pub days: Option<String>,
    /// The same window as the design spells it (`7d`, `28d`, `90d`).
    pub window: Option<String>,
    /// List only this station's lots (a station id such as `work`).
    pub station: Option<String>,
    /// Skip this many of each listed station's lots, oldest first.
    pub lots_offset: Option<String>,
    /// List at most this many lots per station (1 to 100, default 20).
    pub lots_limit: Option<String>,
    /// `oldest` (default) or `newest` first, by arrival at the station.
    pub lots_order: Option<String>,
}

impl StrictFields for FlowQuery {
    const KEYS: &'static [&'static str] = &[
        "family",
        "repo",
        "days",
        "window",
        "station",
        "lots_offset",
        "lots_limit",
        "lots_order",
    ];
    const REPAIR_HINT: &'static str = "send days as 7, 28 or 90 (or window as 7d, 28d or 90d); \
         station is a station id, lots_offset a whole number, lots_limit 1 to 100, \
         lots_order oldest or newest";

    fn check_values(&self) -> Result<(), String> {
        if self.repo.as_ref().is_some_and(|r| {
            r.len() > 200 || r.split('/').count() != 2 || r.split('/').any(str::is_empty)
        }) {
            return Err("repo must be owner/name".into());
        }
        lots_page(self)?;
        days(self).map(|_| ())
    }
}

/// The page of lots one query names. Out-of-range values are refused, not
/// clamped: a clamped page would list lots the caller did not ask for.
pub(crate) fn lots_page(query: &FlowQuery) -> Result<LotsPage, String> {
    let text = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    };
    let station = match text(&query.station) {
        None => None,
        Some(id) => Some(
            Station::parse(&id)
                .ok_or_else(|| {
                    let ids: Vec<&str> = Station::ALL.iter().map(|s| s.id()).collect();
                    format!("station: {id:?} is not one of {}", ids.join(", "))
                })?
                .id(),
        ),
    };
    let number = |key: &str, value: Option<String>, max: u32| -> Result<Option<u32>, String> {
        let Some(value) = value else { return Ok(None) };
        let parsed: u32 = value
            .parse()
            .map_err(|_| format!("{key}: {value:?} is not a whole number"))?;
        if parsed > max {
            return Err(format!("{key}: {parsed} is more than {max}"));
        }
        Ok(Some(parsed))
    };
    let offset = number("lots_offset", text(&query.lots_offset), 1_000_000)?.unwrap_or(0);
    let limit = number(
        "lots_limit",
        text(&query.lots_limit),
        report::MAX_LOTS_PER_STATION as u32,
    )?
    .unwrap_or(report::LOTS_PER_STATION as u32);
    if limit == 0 {
        return Err("lots_limit: 0 lists nothing; send 1 to 100".to_string());
    }
    let order = match text(&query.lots_order).as_deref() {
        None | Some("oldest") => "oldest",
        Some("newest") => "newest",
        Some(other) => return Err(format!("lots_order: {other:?} is not oldest or newest")),
    };
    Ok(LotsPage {
        station,
        offset,
        limit,
        order,
    })
}

/// The window one query names. `days=7` and `window=7d` are the same window;
/// sending both with different values is a refusal, as on the event cursor.
pub(crate) fn days(query: &FlowQuery) -> Result<i64, String> {
    let text = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    };
    let names: Vec<String> = WINDOWS.iter().map(|days| days.to_string()).collect();
    let allowed: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut chosen: Option<i64> = None;
    for (key, value) in [("days", text(&query.days)), ("window", text(&query.window))] {
        let Some(value) = value else { continue };
        // `days` is a bare number and `window` ends in `d`, as documented.
        let stripped = if key == "window" {
            value
                .strip_suffix('d')
                .ok_or_else(|| format!("window: {value:?} is not 7d, 28d or 90d"))?
        } else {
            value.as_str()
        };
        one_of(key, stripped, &allowed)?;
        let value: i64 = stripped
            .parse()
            .map_err(|_| format!("{key}: {value:?} is not a number of days"))?;
        if chosen.is_some_and(|chosen| chosen != value) {
            return Err("days and window are the same window; send one of them".to_string());
        }
        chosen = Some(value);
    }
    Ok(chosen.unwrap_or(DEFAULT_DAYS))
}

fn refuse(status: StatusCode, code: &str, reason: &str, hint: &str) -> AxumResponse {
    typed_error(TypedError {
        status,
        code,
        purpose: "report where work sits on the delivery line",
        reason,
        common_fixes: &[
            "send days as 7, 28 or 90, or window as 7d, 28d or 90d",
            "drop the filters to read the whole line over 28 days",
        ],
        docs_url: DOCS,
        repair_hint: hint,
        message: reason,
    })
}

/// `GET /api/v1/flow`
pub(crate) async fn flow(
    State(state): State<Arc<WebState>>,
    StrictQuery(query): StrictQuery<FlowQuery>,
) -> AxumResponse {
    let family = match super::family::filter(&state, query.family.as_deref()) {
        Ok(family) => family,
        Err(response) => return *response,
    };
    let days = match days(&query) {
        Ok(days) => days,
        Err(reason) => {
            return refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                super::strict_query::CODE,
                &reason,
                FlowQuery::REPAIR_HINT,
            );
        }
    };
    let page = match lots_page(&query) {
        Ok(page) => page,
        Err(reason) => {
            return refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                super::strict_query::CODE,
                &reason,
                FlowQuery::REPAIR_HINT,
            );
        }
    };
    let repo = query.repo.clone();
    let read_repo = repo.clone();
    let now = Utc::now().timestamp_millis();
    let since_ms = report::window_start(now, days);
    let read_family = family.clone();
    let reconcile_state = state.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(error) = delivery::reconcile(&reconcile_state) {
            eprintln!("jeryu-api Flow delivery reconciliation deferred: {error}");
        }
    });
    let worker_state = state.clone();
    let read = tokio::task::spawn_blocking(move || {
        let reached = worker_state.events.flow_reached(read_family.as_deref())?;
        let mut visits = worker_state.events.flow_visits_scoped(
            read_family.as_deref(),
            since_ms,
            read_repo.as_deref(),
        )?;
        // A pull request a later one replaced is stuck where it stopped,
        // whatever the ledger says; the rule is needs-you's `pr_superseded`.
        report::mark_superseded(
            &mut visits,
            &super::pipeline::attention::superseded_open_pulls(&worker_state, Some(since_ms)),
        );
        let open = worker_state
            .events
            .flow_open_lifecycle(read_family.as_deref())?;
        let lifecycle = closure::snapshot(&worker_state, &open, read_repo.as_deref());
        // Work follows the todo's recorded state (`work`): a held todo is
        // held, an unclaimed one waits, and a done or landed one has left.
        lifecycle.settle(&mut visits);
        // Topology is configuration, so it covers the family scope whatever the
        // repository filter is: a drilldown must not hide the other repositories
        // a product delivers. Only current visits count as work in flight.
        let active: Vec<closure::OpenVisit> = open
            .into_iter()
            .filter(|visit| lifecycle.is_current_open(visit))
            .collect();
        let shape = topology::plan(
            &topology::inputs(&worker_state, read_family.as_deref()),
            &active,
        );
        // Every slot's newest beat inside the retention: what says whether a
        // claim at Work has a live worker behind it.
        let beats = worker_state
            .shift
            .heartbeats
            .latest(now - super::shift::HEARTBEAT_RETENTION_MS)?;
        let evidence = claims::Evidence::from_heartbeats(&beats, now);
        Ok::<_, String>((visits, reached, lifecycle, shape, evidence))
    })
    .await
    .map_err(|err| err.to_string())
    .and_then(|read| read);
    match read {
        Ok((visits, reached, lifecycle, shape, evidence)) => {
            let request = Request {
                family,
                days,
                now_ms: now,
            };
            let is_current = |v: &Visit| lifecycle.is_current(v);
            let mut report = report::build_paged(&request, &visits, &reached, &is_current, page);
            claims::mark(&mut report, &visits, &is_current, &evidence);
            let mut body = serde_json::to_value(report).expect("flow serializes");
            body["repository_wip"] = lifecycle.repositories;
            body["closure_record_count"] = serde_json::json!(lifecycle.total_records);
            body["merged_upstream_record_count"] =
                serde_json::json!(lifecycle.merged_upstream_records);
            body["closures_truncated"] =
                serde_json::json!(lifecycle.total_records > lifecycle.items.len());
            body["closures"] = serde_json::json!(lifecycle.items);
            body["lifecycle_problems"] = serde_json::json!(lifecycle.problems);
            body["work_reconciled"] = serde_json::json!(lifecycle.work);
            body["topology"] = serde_json::json!(shape);
            body["repo"] = serde_json::json!(repo);
            Json(body).into_response()
        }
        Err(reason) => refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "flow_read_failed",
            &reason,
            "check <data_dir>/shift.sqlite and the server log for the projection",
        ),
    }
}
