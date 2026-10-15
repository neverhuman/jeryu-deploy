//! `GET /api/v1/yield`: what the agent factory spent against what reached
//! main. Cost per merged change, yield, first-pass rate and the arms of each
//! routing experiment, over the todos worked in a window of days.
//!
//! Admin-only like the event log it reads (see `auth::admin_only_request`).
//! Todos and their merged truth come from the same derivation
//! `GET /api/v1/shift/todos` uses (`shift::enriched_todos`); review spend
//! comes from the never-pruned cost ledger (`pipeline::EventStore::ledger`),
//! and so does `adhoc`, the spend of agent sessions run by hand
//! (`session.costed`, see `yields/adhoc.rs`).
//! Contract: `docs/pipeline-events.md`, "Yield".

mod adhoc;
#[cfg(test)]
mod adhoc_tests;
mod report;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{Duration, NaiveDate, NaiveTime, Utc};
use serde::Deserialize;

use super::WebState;
use super::strict_query::{StrictFields, StrictQuery, filter_one_of};
use super::workcells_support::{TypedError, typed_error};
pub(crate) use adhoc::SESSION_KIND;
pub(crate) use report::{By, Request, Window, build};

const DOCS: &str = "docs/pipeline-events.md";
/// The ledger kind a pull request review's spend is recorded under.
pub(crate) const REVIEW_KIND: &str = "pr.reviewed";
/// Days before `until` the window starts when `since` is left out.
const DEFAULT_DAYS: i64 = 28;

/// `GET /api/v1/yield` query.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct YieldQuery {
    /// Either spelling of a family key; the server canonicalises it.
    pub family: Option<String>,
    /// First day of the window, `YYYY-MM-DD` (UTC).
    pub since: Option<String>,
    /// Last day of the window, `YYYY-MM-DD` (UTC), included.
    pub until: Option<String>,
    /// How spend is grouped: one of [`By::NAMES`].
    pub by: Option<String>,
}

impl StrictFields for YieldQuery {
    const KEYS: &'static [&'static str] = &["family", "since", "until", "by"];
    const REPAIR_HINT: &'static str =
        "send since and until as YYYY-MM-DD and by as one of the values the message names";

    fn check_values(&self) -> Result<(), String> {
        filter_one_of("by", self.by.as_ref(), By::NAMES)?;
        for (name, value) in [("since", &self.since), ("until", &self.until)] {
            day(name, value.as_deref())?;
        }
        Ok(())
    }
}

/// An optional `YYYY-MM-DD`; blank is absent.
fn day(name: &str, value: Option<&str>) -> Result<Option<NaiveDate>, String> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(value) => NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| format!("{name}: {value:?} is not a day spelled YYYY-MM-DD")),
    }
}

/// The window a query names, with `today` filling what it leaves out.
pub(crate) fn window(query: &YieldQuery, today: NaiveDate) -> Result<Window, String> {
    let until = day("until", query.until.as_deref())?.unwrap_or(today);
    let since = day("since", query.since.as_deref())?
        .unwrap_or_else(|| until - Duration::days(DEFAULT_DAYS));
    if since > until {
        return Err(format!("since ({since}) is after until ({until})"));
    }
    Ok(Window { since, until })
}

/// The window as ledger times: midnight UTC of `since` up to, not including,
/// midnight UTC of the day after `until`.
pub(crate) fn window_ms(window: Window) -> (i64, i64) {
    let start = |day: NaiveDate| day.and_time(NaiveTime::MIN).and_utc().timestamp_millis();
    (start(window.since), start(window.until + Duration::days(1)))
}

/// Ad-hoc session spend in `window`, each row's family filled from its
/// repository when the ledger recorded none (the repository may have joined
/// a family after the spend was posted).
fn session_spend(
    state: &WebState,
    window: Window,
) -> Result<Vec<crate::web::pipeline::SpendRow>, String> {
    let (since_ms, until_ms) = window_ms(window);
    let mut rows = state
        .events
        .ledger_between(SESSION_KIND, since_ms, until_ms)?;
    let mut families: BTreeMap<String, Option<String>> = BTreeMap::new();
    for row in &mut rows {
        if row.family.is_some() {
            continue;
        }
        if let Some(repo) = row.repo.clone() {
            row.family = families
                .entry(repo)
                .or_insert_with_key(|repo| super::repositories::family_for_slug(state, repo))
                .clone();
        }
    }
    Ok(rows)
}

fn refuse(status: StatusCode, code: &str, reason: &str, hint: &str) -> AxumResponse {
    typed_error(TypedError {
        status,
        code,
        purpose: "report spend against merged work",
        reason,
        common_fixes: &[
            "send since and until as YYYY-MM-DD with since not after until",
            "send by as one of model, agent, experiment, family, repo, requester, week",
        ],
        docs_url: DOCS,
        repair_hint: hint,
        message: reason,
    })
}

/// `GET /api/v1/yield`
pub(crate) async fn yield_report(
    State(state): State<Arc<WebState>>,
    StrictQuery(query): StrictQuery<YieldQuery>,
) -> AxumResponse {
    let family = match super::family::filter(&state, query.family.as_deref()) {
        Ok(family) => family,
        Err(response) => return *response,
    };
    let now = Utc::now();
    let window = match window(&query, now.date_naive()) {
        Ok(window) => window,
        Err(reason) => {
            return refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                super::strict_query::CODE,
                &reason,
                "send a since that is not after until",
            );
        }
    };
    let by = query
        .by
        .as_deref()
        .map(str::trim)
        .filter(|by| !by.is_empty())
        .and_then(By::parse)
        .unwrap_or(By::Model);
    let worker_state = state.clone();
    let read_family = family.clone();
    let read = tokio::task::spawn_blocking(move || {
        let todos = super::shift::enriched_todos(&worker_state, read_family.as_deref())?;
        let reviews = worker_state.events.ledger(REVIEW_KIND)?;
        let sessions = session_spend(&worker_state, window)?;
        let events = super::review_catch::read_events(&worker_state, read_family.as_deref())?;
        Ok::<_, String>((todos, reviews, sessions, events))
    })
    .await
    .map_err(|err| err.to_string())
    .and_then(|read| read);
    match read {
        Ok((todos, reviews, sessions, events)) => {
            let request = Request {
                window,
                family,
                by,
                now,
            };
            let mut report = build(&request, &todos, &reviews, &sessions);
            let (since_ms, until_ms) = window_ms(window);
            let at = |ms| chrono::DateTime::from_timestamp_millis(ms);
            report.review_catch =
                super::review_catch::report(&events, at(since_ms).zip(at(until_ms)));
            Json(report).into_response()
        }
        Err(reason) => refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "yield_read_failed",
            &reason,
            "check the todo queue repos and <data_dir>/shift.sqlite",
        ),
    }
}
