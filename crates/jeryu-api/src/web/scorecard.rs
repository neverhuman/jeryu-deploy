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
pub(crate) mod read;

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
    let days = window.trim_end_matches('d').parse::<usize>().unwrap_or(30);
    let now_ms = Utc::now().timestamp_millis();
    let (read_family, read_repo, worker) = (family.clone(), repo.clone(), state.clone());
    let read = tokio::task::spawn_blocking(move || {
        let changes = worker
            .events
            .outcome_changes(read_family.as_deref(), read_repo.as_deref())?;
        let raw = worker
            .events
            .scorecard_raw(read_family.as_deref(), now_ms, days as i64)?;
        Ok::<_, String>((changes, raw))
    })
    .await
    .map_err(|err| err.to_string())
    .and_then(|read| read);
    let (changes, raw) = match read {
        Ok(read) => read,
        Err(reason) => {
            return typed_error(TypedError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "scorecard_read_failed",
                purpose: "report delivery outcomes",
                reason: &reason,
                common_fixes: &["retry; check the server log for the ledger read"],
                docs_url: "docs/scorecard.md",
                repair_hint: "check <data_dir>/shift.sqlite and the server log for the projection",
                message: &reason,
            });
        }
    };
    let ctx = compute::Ctx::new(&raw, &changes, repo.as_deref(), now_ms, days);
    let metrics: serde_json::Map<String, serde_json::Value> = compute::metrics(&ctx)
        .into_iter()
        .map(|(name, metric)| {
            (
                name.to_owned(),
                serde_json::to_value(metric).expect("scorecard metric serializes"),
            )
        })
        .collect();
    Json(serde_json::json!({
        "schema_version": "jeryu.scorecard/v1",
        "generated_at": super::shift::rfc3339_ms(now_ms),
        "window": window,
        "family": family,
        "repo": repo,
        "metrics": metrics,
    }))
    .into_response()
}

#[cfg(test)]
mod tests;
