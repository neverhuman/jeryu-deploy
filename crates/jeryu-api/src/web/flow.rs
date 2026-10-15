//! `GET /api/v1/flow`: where work sits on the delivery line, how long it
//! waits at each station, where it loops back and which station is the
//! bottleneck.
//!
//! Admin-only like the event log it is projected from (see
//! `auth::admin_only_request`). The answer is read from `flow_ledger`, the
//! never-pruned projection [`project`] writes in the same transaction as each
//! event, so a 90-day window outlives the log's 30-day retention. The
//! arithmetic is in [`report`]; the station list in [`stations`] is shared
//! with `GET /api/v1/flow/layout`. Contract: `docs/flow.md`, ADR 0004.

pub(crate) mod delivery;
pub(crate) mod history;
pub(crate) mod layout;
pub(crate) mod project;
pub(crate) mod report;
pub(crate) mod stations;

#[cfg(test)]
mod tests;

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
pub(crate) use report::{Request, Visit};
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
}

impl StrictFields for FlowQuery {
    const KEYS: &'static [&'static str] = &["family", "repo", "days", "window"];
    const REPAIR_HINT: &'static str = "send days as 7, 28 or 90 (or window as 7d, 28d or 90d)";

    fn check_values(&self) -> Result<(), String> {
        if self.repo.as_ref().is_some_and(|r| {
            r.len() > 200 || r.split('/').count() != 2 || r.split('/').any(str::is_empty)
        }) {
            return Err("repo must be owner/name".into());
        }
        days(self).map(|_| ())
    }
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
    let repo = query.repo.clone();
    let read_repo = repo.clone();
    let now = Utc::now().timestamp_millis();
    let since_ms = report::window_start(now, days);
    let read_family = family.clone();
    let worker_state = state.clone();
    let read = tokio::task::spawn_blocking(move || {
        delivery::reconcile(&worker_state)?;
        let reached = worker_state.events.flow_reached(read_family.as_deref())?;
        let visits = worker_state.events.flow_visits_scoped(
            read_family.as_deref(),
            since_ms,
            read_repo.as_deref(),
        )?;
        let repositories = worker_state
            .events
            .flow_repositories(read_family.as_deref())?;
        Ok::<_, String>((visits, reached, repositories))
    })
    .await
    .map_err(|err| err.to_string())
    .and_then(|read| read);
    match read {
        Ok((visits, reached, repositories)) => {
            let request = Request {
                family,
                days,
                now_ms: now,
            };
            let mut body = serde_json::to_value(report::build(&request, &visits, &reached))
                .expect("flow serializes");
            body["repository_wip"] = repositories;
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
