//! `GET /api/v1/outcomes`: what became of each change — shipped, reverted,
//! rolled back, reworked or blamed for an incident — one row per change.
//!
//! Admin-only like flow. Rows come from `outcome_ledger`, the never-pruned
//! projection [`project`] writes in the same transaction as each event, so
//! the Scorecard's change failure, false-done and rework rates keep their
//! 14-day observation horizon past the event log's retention. The derived
//! `outcome` is computed here from the stored facts and the clock, because a
//! rework link or an incident may arrive after the 14-day window and must
//! still turn a successful change into a reworked one.
//! Contract: `docs/scorecard.md`, ADR 0010.

pub(crate) mod project;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};

use super::flow::{self, FlowQuery};
use super::shift::rfc3339_ms;
use super::strict_query::{StrictFields, StrictQuery, filter_one_of};
use super::workcells_support::{TypedError, typed_error};
use project::{Change, Leg};

const DOCS: &str = "docs/scorecard.md";
pub(crate) const OUTCOMES_SCHEMA: &str = "jeryu.outcomes/v1";
/// A change is pending until it has been in production this long.
pub(crate) const OBSERVATION_MS: i64 = 14 * 24 * 60 * 60 * 1000;
const OUTCOMES: [&str; 4] = ["pending", "successful", "failed", "reworked"];

/// `GET /api/v1/outcomes` query.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct OutcomesQuery {
    pub family: Option<String>,
    /// A repository (`owner/name`); a multi-repository change matches any of
    /// its repositories.
    pub repo: Option<String>,
    pub days: Option<String>,
    pub window: Option<String>,
    /// Only changes whose derived outcome is this one.
    pub outcome: Option<String>,
}

impl StrictFields for OutcomesQuery {
    const KEYS: &'static [&'static str] = &["family", "repo", "days", "window", "outcome"];
    const REPAIR_HINT: &'static str = "send days as 7, 28 or 90 (or window as 7d, 28d or 90d), outcome as pending, successful, failed or reworked";

    fn check_values(&self) -> Result<(), String> {
        window_days(self)?;
        filter_one_of("outcome", self.outcome.as_ref(), &OUTCOMES)
    }
}

fn window_days(query: &OutcomesQuery) -> Result<i64, String> {
    flow::days(&FlowQuery {
        repo: None,
        family: None,
        days: query.days.clone(),
        window: query.window.clone(),
        ..FlowQuery::default()
    })
}

/// When the change reached production: the latest of its merged repositories'
/// first ship, and only once every merged repository has shipped.
pub(crate) fn shipped_ms(legs: &[Leg]) -> Option<i64> {
    let merged: Vec<&Leg> = legs.iter().filter(|l| l.merged_ms.is_some()).collect();
    if merged.is_empty() {
        return None;
    }
    let mut latest = 0;
    for leg in merged {
        let at = match (leg.released_ms, leg.deployed_ms) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => return None,
        };
        latest = latest.max(at);
    }
    Some(latest)
}

/// pending, successful, failed or reworked.
///
/// Failed (reverted, rolled back or an incident traced to it) outranks
/// reworked, and either holds whatever its age; otherwise a change is pending
/// until it has been shipped for 14 days.
pub(crate) fn outcome_of(change: &Change, now_ms: i64) -> &'static str {
    if change.reverted_ms.is_some()
        || change.rolled_back_ms.is_some()
        || !change.incidents.is_empty()
    {
        return "failed";
    }
    if !change.reworked_by.is_empty() {
        return "reworked";
    }
    match shipped_ms(&change.legs) {
        Some(at) if now_ms - at >= OBSERVATION_MS => "successful",
        _ => "pending",
    }
}

/// The time a change belongs to a window by: its merge, else the attempt that
/// reported it done, else its filing, else the first event that named it.
fn anchor_ms(change: &Change) -> i64 {
    change
        .legs
        .iter()
        .filter_map(|l| l.merged_ms)
        .min()
        .or(change.done_ms)
        .or(change.filed_ms)
        .unwrap_or(change.first_seen_ms)
}

fn ts(ms: Option<i64>) -> Value {
    ms.map_or(Value::Null, |ms| Value::String(rfc3339_ms(ms)))
}

fn row(change: &Change, now_ms: i64) -> Value {
    let min = |f: fn(&Leg) -> Option<i64>| change.legs.iter().filter_map(f).min();
    let shipped = shipped_ms(&change.legs);
    let outcome = outcome_of(change, now_ms);
    let reported_done = change.done_ms.is_some();
    let arm = match (&change.agent, &change.model) {
        (None, None) => None,
        (agent, model) => Some(format!(
            "{}:{}",
            agent.as_deref().unwrap_or("claude"),
            model.as_deref().unwrap_or("unknown")
        )),
    };
    // The release a change shipped in is the one of the repository that
    // shipped it last, the one that completed its delivery.
    let release = change
        .legs
        .iter()
        .filter(|l| l.release.is_some())
        .max_by_key(|l| l.deployed_ms.or(l.released_ms))
        .and_then(|l| l.release.clone());
    let repos: Vec<Value> = change
        .legs
        .iter()
        .map(|l| {
            json!({
                "repo": (!l.repo.is_empty()).then_some(&l.repo),
                "pr": l.pr,
                "sha": l.sha,
                "merged_at": ts(l.merged_ms),
                "released_at": ts(l.released_ms),
                "deployed_at": ts(l.deployed_ms),
                "release": l.release,
            })
        })
        .collect();
    let first_repo = change
        .legs
        .iter()
        .find(|l| !l.repo.is_empty())
        .map(|l| l.repo.clone());
    json!({
        "lot_id": change.lot_id,
        "href": format!("/work/{}", change.lot_id),
        "todo_id": change.todo_id,
        "family": change.family,
        "repo": first_repo,
        "repos": repos,
        "work_kind": change.work_kind,
        "filed_at": ts(change.filed_ms),
        "reported_done_at": ts(change.done_ms),
        "merged_at": ts(min(|l| l.merged_ms)),
        "released_at": ts(min(|l| l.released_ms)),
        "deployed_at": ts(min(|l| l.deployed_ms)),
        "shipped_at": ts(shipped),
        "release": release,
        "reverted_at": ts(change.reverted_ms),
        "reverted_by": change.reverted_by,
        "rolled_back_at": ts(change.rolled_back_ms),
        "reworks": change.reworks,
        "reworked_by": change.reworked_by,
        "incident_ids": change.incidents,
        "agent": change.agent,
        "model": change.model,
        "experiment": change.experiment,
        "arm": arm,
        "outcome": outcome,
        "observation_ends_at": ts(shipped.map(|at| at + OBSERVATION_MS)),
        // A todo reported done that was later reworked or reverted. Counted
        // once per change, whenever the link arrived.
        "false_done": reported_done
            && (!change.reworked_by.is_empty() || change.reverted_ms.is_some()),
    })
}

/// The answer for `changes` at `now_ms`: rows of the window, newest first,
/// and the counts by outcome and work kind.
pub(crate) fn build(
    changes: &[Change],
    days: i64,
    now_ms: i64,
    outcome: Option<&str>,
    family: Option<&str>,
    repo: Option<&str>,
) -> Value {
    let since = flow::report::window_start(now_ms, days);
    let mut inside: Vec<(&Change, i64)> = changes
        .iter()
        .map(|c| (c, anchor_ms(c)))
        .filter(|(_, at)| *at >= since && *at < now_ms)
        .filter(|(c, _)| outcome.is_none_or(|o| outcome_of(c, now_ms) == o))
        .collect();
    inside.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.lot_id.cmp(&b.0.lot_id)));
    let mut by_outcome = serde_json::Map::new();
    for name in OUTCOMES {
        let n = inside
            .iter()
            .filter(|(c, _)| outcome_of(c, now_ms) == name)
            .count();
        by_outcome.insert(name.to_string(), json!(n));
    }
    let mut by_kind = serde_json::Map::new();
    for name in ["feature", "fix", "rework", "ops", "unknown"] {
        let n = inside
            .iter()
            .filter(|(c, _)| c.work_kind.as_deref().unwrap_or("unknown") == name)
            .count();
        by_kind.insert(name.to_string(), json!(n));
    }
    let false_done = inside
        .iter()
        .filter(|(c, _)| row(c, now_ms)["false_done"] == json!(true))
        .count();
    json!({
        "schema_version": OUTCOMES_SCHEMA,
        "generated_at": rfc3339_ms(now_ms),
        "family": family,
        "repo": repo,
        "days": days,
        "window": {"since": rfc3339_ms(since), "until": rfc3339_ms(now_ms)},
        "observation_days": 14,
        "total": inside.len(),
        "by_outcome": by_outcome,
        "by_work_kind": by_kind,
        "false_done": false_done,
        "changes": inside.iter().map(|(c, _)| row(c, now_ms)).collect::<Vec<_>>(),
    })
}

fn refuse(status: StatusCode, code: &str, reason: &str, hint: &str) -> AxumResponse {
    typed_error(TypedError {
        status,
        code,
        purpose: "list the outcome of every change",
        reason,
        common_fixes: &[
            "send days as 7, 28 or 90, or window as 7d, 28d or 90d",
            "drop the filters to read every change of the last 28 days",
        ],
        docs_url: DOCS,
        repair_hint: hint,
        message: reason,
    })
}

/// `GET /api/v1/outcomes`
pub(crate) async fn outcomes(
    State(state): State<Arc<super::WebState>>,
    StrictQuery(query): StrictQuery<OutcomesQuery>,
) -> AxumResponse {
    let family = match super::family::filter(&state, query.family.as_deref()) {
        Ok(family) => family,
        Err(response) => return *response,
    };
    let days = match window_days(&query) {
        Ok(days) => days,
        Err(reason) => {
            return refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                super::strict_query::CODE,
                &reason,
                OutcomesQuery::REPAIR_HINT,
            );
        }
    };
    let repo = query
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(ToString::to_string);
    let now = Utc::now().timestamp_millis();
    let (read_family, read_repo, worker) = (family.clone(), repo.clone(), state.clone());
    let read = tokio::task::spawn_blocking(move || {
        worker
            .events
            .outcome_changes(read_family.as_deref(), read_repo.as_deref())
    })
    .await
    .map_err(|err| err.to_string())
    .and_then(|read| read);
    match read {
        Ok(changes) => Json(build(
            &changes,
            days,
            now,
            query.outcome.as_deref().filter(|o| !o.is_empty()),
            family.as_deref(),
            repo.as_deref(),
        ))
        .into_response(),
        Err(reason) => refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "outcomes_read_failed",
            &reason,
            "check <data_dir>/shift.sqlite and the server log for the projection",
        ),
    }
}
