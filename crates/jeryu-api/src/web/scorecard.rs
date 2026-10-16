//! Admin scorecard. Each tile is present even before its producer is
//! installed; absence is represented as unknown, never as a numeric zero.
//! The arithmetic is `compute`, the ledger reads are `read`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::Utc;
use serde::Deserialize;

use super::WebState;
use super::strict_query::{StrictFields, StrictQuery};
use super::workcells_support::{TypedError, typed_error};

pub(crate) mod compute;
pub(crate) mod digest;
mod hypotheses;
pub(crate) mod read;
pub(crate) mod weekly;

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct ScorecardQuery {
    window: Option<String>,
    family: Option<String>,
    repo: Option<String>,
}

impl StrictFields for ScorecardQuery {
    const KEYS: &'static [&'static str] = &["window", "family", "repo"];
    const REPAIR_HINT: &'static str =
        "use window=7d, 30d or 90d and optional family and repo filters";

    fn check_values(&self) -> Result<(), String> {
        if let Some(window) = self.window.as_deref()
            && !["7d", "30d", "90d"].contains(&window)
        {
            return Err("window must be 7d, 30d or 90d".into());
        }
        Ok(())
    }
}

/// One Scorecard answer: the metric map `GET /api/v1/scorecard` returns and
/// what it was computed for. The route and the weekly digest both read this,
/// so the digest reports the same numbers and does no arithmetic of its own.
pub(crate) struct Report {
    pub(crate) window: String,
    pub(crate) generated_at: String,
    pub(crate) family: Option<String>,
    pub(crate) repo: Option<String>,
    pub(crate) metrics: serde_json::Map<String, serde_json::Value>,
}

impl Report {
    /// The answer as the route sends it.
    fn into_json(self) -> serde_json::Value {
        serde_json::json!({
            "schema_version": SCHEMA,
            "generated_at": self.generated_at,
            "window": self.window,
            "family": self.family,
            "repo": self.repo,
            "metrics": self.metrics,
        })
    }
}

pub(crate) const SCHEMA: &str = "jeryu.scorecard/v1";

/// How many days a window names. The query only accepts the three windows.
fn window_days(window: &str) -> usize {
    window.trim_end_matches('d').parse::<usize>().unwrap_or(30)
}

/// Read the ledgers and compute every tile. Blocking: it reads
/// `<data_dir>/shift.sqlite` and walks every change.
pub(crate) fn report(
    state: &WebState,
    window: &str,
    family: Option<&str>,
    repo: Option<&str>,
    now_ms: i64,
) -> Result<Report, String> {
    let days = window_days(window);
    let changes = state.events.outcome_changes(family, repo)?;
    let raw = state.events.scorecard_raw(family, now_ms, days as i64)?;
    let todos = super::shift::enriched_todos(state, family)?;
    let ctx = compute::Ctx::new(&raw, &changes, repo, now_ms, days);
    let mut metrics: serde_json::Map<String, serde_json::Value> = compute::metrics(&ctx)
        .into_iter()
        .map(|(name, metric)| {
            (
                name.to_owned(),
                serde_json::to_value(metric).expect("scorecard metric serializes"),
            )
        })
        .collect();
    metrics.insert(
        "hypotheses_checked_and_met".into(),
        serde_json::to_value(hypotheses::metric(&todos, &changes, now_ms))
            .expect("metric serializes"),
    );
    Ok(Report {
        window: window.to_string(),
        generated_at: super::shift::rfc3339_ms(now_ms),
        family: family.map(str::to_string),
        repo: repo.map(str::to_string),
        metrics,
    })
}

pub(crate) async fn scorecard(
    State(state): State<Arc<WebState>>,
    StrictQuery(query): StrictQuery<ScorecardQuery>,
) -> AxumResponse {
    let family = match super::family::filter(&state, query.family.as_deref()) {
        Ok(family) => family,
        Err(response) => return *response,
    };
    let repo = query
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(ToString::to_string);
    let window = query.window.as_deref().unwrap_or("30d").to_string();
    let now_ms = Utc::now().timestamp_millis();
    let worker = state.clone();
    let computed = tokio::task::spawn_blocking(move || {
        report(&worker, &window, family.as_deref(), repo.as_deref(), now_ms)
    })
    .await
    .map_err(|err| err.to_string())
    .and_then(|computed| computed);
    match computed {
        Ok(report) => Json(report.into_json()).into_response(),
        Err(reason) => typed_error(TypedError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "scorecard_read_failed",
            purpose: "report delivery outcomes",
            reason: &reason,
            common_fixes: &["retry; check the server log for the ledger read"],
            docs_url: "docs/scorecard.md",
            repair_hint: "check <data_dir>/shift.sqlite and the server log for the projection",
            message: &reason,
        }),
    }
}

#[cfg(test)]
mod tests;
