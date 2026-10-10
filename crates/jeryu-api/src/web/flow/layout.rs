//! `GET /api/v1/flow/layout`: which tools serve each station of the delivery
//! line, read from the latest runner heartbeats, and whether a station has
//! fewer than the capacity the site expects (`JERYU_FLOW_EXPECTED_<STATION>`).
//! The station list is the one `GET /api/v1/flow` reports ([`Station::ALL`]).

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::Response as AxumResponse;
use axum::{Json, extract::State, response::IntoResponse};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::Station;
use crate::web::control_plane::{self, GateRunnerHeartbeat, GateRunnerRecord};
use crate::web::shift;
use crate::web::strict_query::{StrictFields, StrictQuery};
use crate::web::{WebState, api_error};

#[derive(Debug, Default, Deserialize)]
pub(crate) struct LayoutQuery {}

impl StrictFields for LayoutQuery {
    const KEYS: &'static [&'static str] = &[];
    const REPAIR_HINT: &'static str = "drop query parameters; this layout route reads no filters";
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ToolOnline {
    Online,
    Offline,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ToolLoad {
    Busy,
    Idle,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolCode {
    repo: Option<String>,
    commit: Option<String>,
    version: Option<String>,
    installed_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct StationTool {
    runner_id: String,
    host: String,
    online: ToolOnline,
    load: ToolLoad,
    last_seen: String,
    code: Option<ToolCode>,
    failing: bool,
    failure_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct StationCapacity {
    reporting: usize,
    busy: usize,
    expected: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct LayoutStation {
    id: &'static str,
    label: &'static str,
    order: usize,
    tools: Vec<StationTool>,
    capacity: StationCapacity,
    flagged: bool,
    warnings: Vec<&'static str>,
    unknown: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct LayoutReport {
    schema_version: &'static str,
    generated_at: String,
    stations: Vec<LayoutStation>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ExpectedCapacity {
    values: std::collections::BTreeMap<&'static str, Option<usize>>,
    invalid: std::collections::BTreeSet<&'static str>,
}

impl ExpectedCapacity {
    fn from_env() -> Self {
        let mut expected = Self::default();
        for station in Station::ALL {
            let id = station.id();
            match std::env::var(expected_env_key(id)) {
                Ok(raw) => match raw.trim().parse::<usize>() {
                    Ok(value) => {
                        expected.values.insert(id, Some(value));
                    }
                    Err(_) => {
                        expected.values.insert(id, None);
                        expected.invalid.insert(id);
                    }
                },
                Err(_) => {
                    expected.values.insert(id, None);
                }
            }
        }
        expected
    }

    #[cfg(test)]
    fn with(station: &'static str, value: Option<usize>) -> Self {
        let mut expected = Self::default();
        for station in Station::ALL {
            expected.values.insert(station.id(), None);
        }
        expected.values.insert(station, value);
        expected
    }

    fn get(&self, station: &'static str) -> Option<usize> {
        self.values.get(station).copied().flatten()
    }

    fn invalid(&self, station: &'static str) -> bool {
        self.invalid.contains(station)
    }
}

fn expected_env_key(station: &str) -> String {
    format!("JERYU_FLOW_EXPECTED_{}", station.to_ascii_uppercase())
}

/// GET /api/v1/flow/layout. Station tools come from the latest runner
/// heartbeats; expected capacity is optional site configuration.
pub(crate) async fn layout(
    State(state): State<Arc<WebState>>,
    StrictQuery(_query): StrictQuery<LayoutQuery>,
) -> AxumResponse {
    let now = Utc::now();
    let shift_records = match state
        .shift
        .heartbeats
        .latest(now.timestamp_millis() - chrono::Duration::days(14).num_milliseconds())
    {
        Ok(records) => records,
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "flow_layout_read_failed",
                &format!("could not read flow layout heartbeats: {error}"),
            );
        }
    };
    Json(layout_report(
        now,
        &state.gate_runners.snapshot(),
        &shift_records,
        &ExpectedCapacity::from_env(),
    ))
    .into_response()
}

fn layout_report(
    now: chrono::DateTime<Utc>,
    gate_records: &[GateRunnerRecord],
    shift_records: &[shift::StoredHeartbeat],
    expected: &ExpectedCapacity,
) -> LayoutReport {
    let mut stations = Vec::new();
    for (index, station) in Station::ALL.iter().copied().enumerate() {
        let tools = station_tools(station.id(), now, gate_records, shift_records);
        let reporting = tools
            .iter()
            .filter(|tool| tool.online == ToolOnline::Online)
            .count();
        let busy = tools
            .iter()
            .filter(|tool| tool.load == ToolLoad::Busy)
            .count();
        let mut warnings = Vec::new();
        let mut unknown = Vec::new();
        if tools.is_empty() || reporting == 0 {
            warnings.push("no_reporting_tool");
            unknown.push("no_heartbeat");
        }
        if let Some(expected_count) = expected.get(station.id()) {
            if reporting < expected_count {
                warnings.push("fewer_than_expected");
            }
        } else {
            unknown.push("expected_capacity");
        }
        if expected.invalid(station.id()) {
            warnings.push("expected_capacity_invalid");
        }
        if tools.iter().any(|tool| tool.failing) {
            warnings.push("tool_failing");
        }
        warnings.sort_unstable();
        warnings.dedup();
        unknown.sort_unstable();
        unknown.dedup();
        stations.push(LayoutStation {
            id: station.id(),
            label: station.label(),
            order: index + 1,
            capacity: StationCapacity {
                reporting,
                busy,
                expected: expected.get(station.id()),
            },
            flagged: !warnings.is_empty(),
            warnings,
            unknown,
            tools,
        });
    }
    LayoutReport {
        schema_version: "jeryu.flow.layout/v1",
        generated_at: now.to_rfc3339(),
        stations,
    }
}

fn station_tools(
    station: &str,
    now: chrono::DateTime<Utc>,
    gate_records: &[GateRunnerRecord],
    shift_records: &[shift::StoredHeartbeat],
) -> Vec<StationTool> {
    match station {
        "queue" | "work" => shift_records
            .iter()
            .map(|record| shift_tool(record, now))
            .collect(),
        "review" => gate_records
            .iter()
            .filter(|record| control_plane::is_reviewer(&record.heartbeat))
            .map(|record| runner_tool(record, now))
            .collect(),
        "gate" => gate_records
            .iter()
            .filter(|record| control_plane::holds_gate_slot(&record.heartbeat))
            .map(|record| runner_tool(record, now))
            .collect(),
        "pin" | "stage" => gate_records
            .iter()
            .filter(|record| control_plane::is_automation(&record.heartbeat))
            .filter(|record| automation_serves(station, &record.heartbeat))
            .map(|record| runner_tool(record, now))
            .collect(),
        "deploy" => gate_records
            .iter()
            .filter(|record| control_plane::is_deploy(&record.heartbeat))
            .map(|record| runner_tool(record, now))
            .collect(),
        _ => Vec::new(),
    }
}

fn shift_tool(record: &shift::StoredHeartbeat, now: chrono::DateTime<Utc>) -> StationTool {
    let heartbeat = &record.heartbeat;
    let online = now.timestamp_millis().saturating_sub(record.received_ms) <= shift::HEALTHY_MS;
    let active = heartbeat.active.unwrap_or(0) > 0;
    let busy_state = matches!(heartbeat.state.as_str(), "working" | "busy" | "claimed");
    let failing = matches!(heartbeat.state.as_str(), "failed" | "error");
    StationTool {
        runner_id: format!(
            "{}@{}/{}",
            heartbeat.operator, heartbeat.host, heartbeat.slot
        ),
        host: heartbeat.host.clone(),
        online: if online {
            ToolOnline::Online
        } else {
            ToolOnline::Offline
        },
        load: if active || busy_state || heartbeat.todo_id.is_some() {
            ToolLoad::Busy
        } else {
            ToolLoad::Idle
        },
        last_seen: shift::rfc3339_ms(record.received_ms),
        code: heartbeat.version.as_ref().map(|version| ToolCode {
            repo: None,
            commit: None,
            version: Some(version.clone()),
            installed_at: None,
        }),
        failing,
        failure_reason: failing.then(|| {
            heartbeat
                .detail
                .clone()
                .unwrap_or_else(|| heartbeat.state.clone())
        }),
    }
}

fn runner_tool(record: &GateRunnerRecord, now: chrono::DateTime<Utc>) -> StationTool {
    let heartbeat = &record.heartbeat;
    let (failing, failure_reason) = failing_result(heartbeat);
    StationTool {
        runner_id: heartbeat.runner_id.clone(),
        host: heartbeat.host.clone(),
        online: if control_plane::is_online(record, now) {
            ToolOnline::Online
        } else {
            ToolOnline::Offline
        },
        load: if heartbeat.current.is_some() {
            ToolLoad::Busy
        } else {
            ToolLoad::Idle
        },
        last_seen: record.received_at.to_rfc3339(),
        code: heartbeat.code.as_ref().map(|code| ToolCode {
            repo: Some(code.repo.clone()),
            commit: Some(code.commit.clone()),
            version: code.version.clone(),
            installed_at: code.installed_at.map(|at| at.to_rfc3339()),
        }),
        failing,
        failure_reason,
    }
}

fn automation_serves(station: &str, heartbeat: &GateRunnerHeartbeat) -> bool {
    let says =
        |text: &str| text.contains(station) || text.contains(&station.replace("pin", "auto-pin"));
    heartbeat
        .current
        .as_ref()
        .is_some_and(|task| says(&task.recipe))
        || heartbeat
            .last
            .as_ref()
            .is_some_and(|result| says(&result.recipe))
}

fn failing_result(heartbeat: &GateRunnerHeartbeat) -> (bool, Option<String>) {
    let Some(last) = heartbeat.last.as_ref() else {
        return (false, None);
    };
    let failing = match control_plane::runner_kind(heartbeat) {
        "gate" => matches!(last.conclusion.as_str(), "failure" | "error"),
        "reviewer" => matches!(
            last.conclusion.as_str(),
            "failed" | "interrupted" | "publication_rejected" | "too_large"
        ),
        "automation" | "deployer" => last.conclusion == "failed",
        "jankurai-audit" => matches!(
            last.conclusion.as_str(),
            "tool-failed" | "refused" | "failed"
        ),
        _ => false,
    };
    (
        failing,
        failing.then(|| {
            last.reason
                .clone()
                .unwrap_or_else(|| format!("last conclusion was {}", last.conclusion))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::control_plane::{GateRunnerResult, GateRunnerStore, RunnerCode};
    use crate::web::shift::Heartbeat;
    use chrono::{Duration, TimeZone};

    fn gate_record(runner_id: &str, labels: Vec<String>, conclusion: &str) -> GateRunnerRecord {
        let store = GateRunnerStore::with_reporters(["ci-bot"]);
        let now = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();
        store
            .record(
                GateRunnerHeartbeat {
                    runner_id: runner_id.to_string(),
                    host: "build-1".to_string(),
                    slot: 0,
                    labels,
                    interval_seconds: None,
                    current: None,
                    last: Some(GateRunnerResult {
                        repo: "jeryu/jeryu-deploy".to_string(),
                        pr: Some(7),
                        sha: "1234567".to_string(),
                        recipe: "just required".to_string(),
                        conclusion: conclusion.to_string(),
                        target: None,
                        reason: Some("compiler failed".to_string()),
                        seconds: 42,
                        finished_at: now - Duration::seconds(30),
                    }),
                    code: Some(RunnerCode {
                        repo: "jeryu/jeryu-deploy".to_string(),
                        commit: "abcdef1".to_string(),
                        version: Some("1.2.3".to_string()),
                        installed_at: Some(now - Duration::hours(2)),
                    }),
                    tools: Vec::new(),
                },
                "ci-bot",
                now,
            )
            .unwrap();
        store.snapshot().remove(0)
    }

    #[test]
    fn layout_flags_missing_capacity_and_failed_runner_beats() {
        let now = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();
        let expected = ExpectedCapacity::with("gate", Some(2));
        let report = layout_report(
            now,
            &[gate_record("build-1/slot0", Vec::new(), "failure")],
            &[],
            &expected,
        );
        let gate = report
            .stations
            .iter()
            .find(|station| station.id == "gate")
            .unwrap();
        assert_eq!(gate.capacity.reporting, 1);
        assert_eq!(gate.capacity.expected, Some(2));
        assert!(gate.flagged);
        assert!(gate.warnings.contains(&"fewer_than_expected"));
        assert!(gate.warnings.contains(&"tool_failing"));
        assert_eq!(
            gate.tools[0].code.as_ref().unwrap().commit.as_deref(),
            Some("abcdef1")
        );

        let merge = report
            .stations
            .iter()
            .find(|station| station.id == "merge")
            .unwrap();
        assert!(merge.warnings.contains(&"no_reporting_tool"));
        assert!(merge.unknown.contains(&"expected_capacity"));
    }

    #[test]
    fn layout_maps_worker_slots_to_queue_and_work() {
        let now = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();
        let worker = shift::StoredHeartbeat {
            received_ms: now.timestamp_millis(),
            heartbeat: Heartbeat {
                operator: "worker-a".to_string(),
                host: "host-a".to_string(),
                slot: "w1".to_string(),
                family: "jeryu".to_string(),
                state: "working".to_string(),
                todo_id: Some("T1".to_string()),
                stage: Some("agent".to_string()),
                lease_until: None,
                shift: None,
                planned_slots: None,
                schedule: None,
                version: Some("codex-5".to_string()),
                kind: "todoq".to_string(),
                detail: None,
                capacity: None,
                active: None,
            },
        };
        let report = layout_report(now, &[], &[worker], &ExpectedCapacity::default());
        let queue = report
            .stations
            .iter()
            .find(|station| station.id == "queue")
            .unwrap();
        assert_eq!(queue.tools[0].runner_id, "worker-a@host-a/w1");
        assert_eq!(queue.tools[0].load, ToolLoad::Busy);
        assert_eq!(queue.capacity.reporting, 1);
    }
}
