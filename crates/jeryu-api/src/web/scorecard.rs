//! Admin scorecard contract. Each tile is present even before its producer is
//! installed; absence is represented as unknown, never as a numeric zero.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use super::WebState;
use super::strict_query::{StrictFields, StrictQuery};

const METRICS: &[(&str, &str)] = &[
    (
        "lead_time_p50",
        "flow_ledger, pipeline_events and release/deploy events",
    ),
    (
        "lead_time_p85",
        "flow_ledger, pipeline_events and release/deploy events",
    ),
    ("station_breakdown", "flow_ledger station visits"),
    (
        "flow_efficiency",
        "flow_ledger classified active and waiting intervals",
    ),
    (
        "completed_work_items_per_day",
        "flow_ledger merge exits or pr.merged events",
    ),
    ("surviving_net_lines_per_day", "line survival ledger"),
    ("releases_per_day", "release and deploy events"),
    ("changes_per_release", "release membership"),
    (
        "prs_merged_per_day",
        "pipeline_events pr.merged and queue.landed",
    ),
    (
        "raw_net_lines_per_day",
        "durable pull request diff statistics",
    ),
    ("wip_by_station", "flow_ledger open visits"),
    (
        "merged_but_unreleased",
        "flow_ledger and release/deploy membership",
    ),
    (
        "holds_by_age_and_action",
        "flow_ledger holds and pipeline action events",
    ),
    (
        "arrival_vs_completion",
        "pipeline_events todo.filed and merge events",
    ),
    ("change_failure_rate", "change outcome and incident ledgers"),
    (
        "escaped_defects_to_main",
        "outcome ledger and review findings",
    ),
    ("escaped_defects_to_prod", "outcome and incident ledgers"),
    (
        "false_done_rate",
        "outcome ledger and todo completion events",
    ),
    (
        "review_catch_rate",
        "review finding outcomes and outcome ledger",
    ),
    ("rework_rate", "outcome ledger"),
    ("main_red_time_per_repo", "main health transitions"),
    (
        "gate_flake_rate",
        "pipeline_events gate.finished with run and SHA identity",
    ),
    ("time_to_detect", "incident ledger"),
    (
        "time_to_restore",
        "incident ledger and successful deploy events",
    ),
    (
        "cost_per_successful_change",
        "cost_ledger and outcome ledger",
    ),
    (
        "human_minutes_per_successful_change",
        "human time and outcome ledgers",
    ),
    ("review_cost", "pipeline review events and cost_ledger"),
    ("eta_error", "immutable Flow ETA snapshots"),
    ("lead_time_spread", "flow_ledger and shipping events"),
    ("gate_queue_wait", "flow_ledger proven gate queue entry"),
    ("worker_slot_utilisation", "worker slot state history"),
    ("runner_utilisation", "runner heartbeat history"),
    (
        "completed_work_mix",
        "completion events and explicit work classification",
    ),
    (
        "hypotheses_checked_and_met",
        "hypothesis records and release/deploy events",
    ),
];

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

#[derive(Serialize)]
struct Metric {
    value: Option<serde_json::Value>,
    previous: Option<serde_json::Value>,
    daily: Vec<Option<serde_json::Value>>,
    sample_size: usize,
    source: &'static str,
    unknown: Option<Unknown>,
}

#[derive(Serialize)]
struct Unknown {
    reason: &'static str,
}

pub(crate) async fn scorecard(
    State(_state): State<Arc<WebState>>,
    StrictQuery(query): StrictQuery<ScorecardQuery>,
) -> Json<serde_json::Value> {
    let window = query.window.as_deref().unwrap_or("30d");
    let days = window.trim_end_matches('d').parse::<usize>().unwrap_or(30);
    let metrics: serde_json::Map<String, serde_json::Value> = METRICS
        .iter()
        .map(|(name, source)| {
            let metric = Metric {
                value: None,
                previous: None,
                daily: vec![None; days],
                sample_size: 0,
                source,
                unknown: Some(Unknown {
                    reason: "source_not_recorded",
                }),
            };
            (
                (*name).to_owned(),
                serde_json::to_value(metric).expect("scorecard metric serializes"),
            )
        })
        .collect();
    Json(serde_json::json!({
        "schema_version": "jeryu.scorecard/v1",
        "generated_at": Utc::now().to_rfc3339(),
        "window": window,
        "family": query.family,
        "repo": query.repo,
        "metrics": metrics,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_design_metric_is_unknown_without_a_recorded_source() {
        let metrics: serde_json::Map<String, serde_json::Value> = METRICS
            .iter()
            .map(|(name, source)| {
                let metric = Metric {
                    value: None,
                    previous: None,
                    daily: vec![None; 7],
                    sample_size: 0,
                    source,
                    unknown: Some(Unknown {
                        reason: "source_not_recorded",
                    }),
                };
                ((*name).to_string(), serde_json::to_value(metric).unwrap())
            })
            .collect();
        assert_eq!(metrics.len(), 34);
        for metric in metrics.values() {
            assert!(metric["value"].is_null());
            assert_eq!(metric["unknown"]["reason"], "source_not_recorded");
            assert_eq!(metric["sample_size"], 0);
            assert_eq!(metric["daily"].as_array().unwrap().len(), 7);
        }
    }

    #[test]
    fn only_supported_windows_are_accepted() {
        assert!(
            ScorecardQuery {
                window: Some("30d".into()),
                ..Default::default()
            }
            .check_values()
            .is_ok()
        );
        assert!(
            ScorecardQuery {
                window: Some("14d".into()),
                ..Default::default()
            }
            .check_values()
            .is_err()
        );
    }
}
