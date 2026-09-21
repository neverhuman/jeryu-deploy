use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use std::collections::BTreeSet;

use crate::web::WebState;
use crate::web::agent_runs::{AgentRunSourceSnapshot, AgentRunState, AgentRunStatusResponse};
use crate::web::workcells_support::manager;

use super::*;

/// The runner fabric as the forge actually knows it: gate runners that have
/// reported a heartbeat, plus workcell leases and their running agent runs.
/// Nothing is invented; with no reporting runner the fabric is `unknown`.
pub(crate) fn runner_fabric(state: &WebState) -> RunnerFabricResponse {
    runner_fabric_at(state, Utc::now())
}

pub(crate) fn runner_fabric_at(state: &WebState, now: DateTime<Utc>) -> RunnerFabricResponse {
    let workcells = manager(state).workcells();
    let agent_runs = state.agent_runs.list();
    let mut seed = gate_runner_nodes(&state.gate_runners.snapshot(), now);
    for node in seed
        .iter_mut()
        .filter(|node| node.source == REVIEWER_SOURCE)
    {
        with_merge_outcome(state, node);
    }
    let node_details = build_runner_nodes(seed, &workcells, &agent_runs);
    let last_updated = node_details
        .iter()
        .filter_map(|node| node.last_updated.as_ref())
        .max()
        .cloned();
    // Reviewers and background timers are listed but hold no gate slot, so
    // they stay out of the gate capacity figures.
    let holds_slot =
        |node: &&RunnerNodeSummary| ![REVIEWER_SOURCE, AUTOMATION_SOURCE].contains(&&*node.source);
    let online: Vec<&RunnerNodeSummary> = node_details
        .iter()
        .filter(holds_slot)
        .filter(|node| node.state != "offline")
        .collect();
    let online_runners = count(online.len());
    let gate_nodes = node_details.iter().filter(holds_slot).count();
    let offline_runners = count(gate_nodes) - online_runners;
    let busy_runners = count(
        online
            .iter()
            .filter(|node| node.in_flight > 0 || node.active_task_count > 0)
            .count(),
    );
    let active_slots: u32 = online.iter().map(|node| node.capacity).sum();
    let total_slots: u32 = node_details.iter().map(|node| node.capacity).sum();
    let hosts: BTreeSet<&str> = node_details
        .iter()
        .map(|node| node.runner_id.split('/').next().unwrap_or(&node.runner_id))
        .collect();
    let utilization = if active_slots == 0 {
        0.0
    } else {
        f64::from(busy_runners) / f64::from(active_slots)
    };
    RunnerFabricResponse {
        schema_version: "jeryu.runner_fabric/v1".to_string(),
        local: RunnerLocalFabric {
            state: if online_runners == 0 {
                EvidenceState::Unknown
            } else {
                EvidenceState::Fresh
            },
            nodes: count(hosts.len()),
            online_runners,
            offline_runners,
            busy_runners,
            idle_runners: online_runners - busy_runners,
            total_slots,
            active_slots,
            utilization,
            last_updated,
            node_details,
        },
        mirror: MirrorEvidence {
            name: "github_actions_runners".to_string(),
            state: EvidenceState::Missing,
            reason: "optional GitHub mirror runner adapter is not configured".to_string(),
            docs_url: MIRROR_DOCS.to_string(),
        },
    }
}

/// The live fabric's capacity in read-model terms, so the TUI Pools/Health
/// panes show exactly what `GET /api/v1/control-plane/runners` shows.
pub(crate) fn fleet_capacity(state: &WebState) -> crate::read_model::FleetCapacity {
    let local = runner_fabric(state).local;
    crate::read_model::FleetCapacity {
        online_runners: local.online_runners,
        busy_runners: local.busy_runners,
        idle_runners: local.idle_runners,
        stuck_runners: local.offline_runners,
        active_slots: local.active_slots,
        total_slots: local.total_slots,
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `source` of a node reported by the pr-redteam reviewer.
pub(crate) const REVIEWER_SOURCE: &str = "pr-redteam";

/// `source` of a node reported by a background timer (auto-pin, auto-stage).
pub(crate) const AUTOMATION_SOURCE: &str = "automation";

/// `repo#pr`, or `repo@sha` for work that has no pull request.
pub(crate) fn work_label(repo: &str, pr: Option<u64>, sha: &str) -> String {
    match pr {
        Some(pr) => format!("{repo}#{pr}"),
        None => format!("{repo}@{}", sha.get(..7).unwrap_or(sha)),
    }
}

/// One node per reporting gate runner slot, plus one per reviewer and per
/// background timer. A runner silent for longer than its offline threshold
/// ([`offline_after_secs`]) is shown offline, with no running task.
pub(crate) fn gate_runner_nodes(
    records: &[GateRunnerRecord],
    now: DateTime<Utc>,
) -> Vec<RunnerNodeSummary> {
    records
        .iter()
        .map(|record| {
            let beat = &record.heartbeat;
            let online = is_online(record, now);
            let reviewer = is_reviewer(beat);
            let automation = is_automation(beat);
            let received = record.received_at.to_rfc3339();
            let mut labels = vec![beat.host.clone(), format!("slot {}", beat.slot)];
            for label in &beat.labels {
                if !labels.contains(label) {
                    labels.push(label.clone());
                }
            }
            let active_tasks: Vec<RunnerTaskSummary> = beat
                .current
                .iter()
                .filter(|_| online)
                .map(|task| RunnerTaskSummary {
                    task_id: format!("{}@{}", beat.runner_id, task.sha),
                    job_id: work_label(&task.repo, task.pr, &task.sha),
                    agent_run_id: None,
                    workcell_id: None,
                    repo: Some(task.repo.clone()),
                    label: work_label(&task.repo, task.pr, &task.sha),
                    program: task.recipe.clone(),
                    state: "running".to_string(),
                    started_at: Some(task.started_at.to_rfc3339()),
                    updated_at: Some(received.clone()),
                    tty_preview: RunnerTtyPreview {
                        state: EvidenceState::Missing,
                        lines: Vec::new(),
                    },
                })
                .collect();
            RunnerNodeSummary {
                runner_id: beat.runner_id.clone(),
                source: if automation {
                    AUTOMATION_SOURCE
                } else if reviewer {
                    REVIEWER_SOURCE
                } else {
                    "pr-gate-runner"
                }
                .to_string(),
                state: if online { "active" } else { "offline" }.to_string(),
                capacity: u32::from(!reviewer && !automation),
                in_flight: count(active_tasks.len()),
                labels,
                classes: vec![
                    if automation {
                        "automation"
                    } else if reviewer {
                        "reviewer"
                    } else {
                        "pr-gate"
                    }
                    .to_string(),
                ],
                active_task_count: count(active_tasks.len()),
                last_updated: Some(received),
                active_tasks,
                last_activity: beat.last.as_ref().map(|last| RunnerLastActivity {
                    repo: last.repo.clone(),
                    pr: last.pr,
                    sha: last.sha.clone(),
                    recipe: last.recipe.clone(),
                    conclusion: last.conclusion.clone(),
                    seconds: last.seconds,
                    finished_at: last.finished_at.to_rfc3339(),
                    merge_attempt: None,
                }),
                offline_after_seconds: Some(offline_after_secs(beat)),
                merge_grant_gaps: Vec::new(),
            }
        })
        .collect()
}

/// A reviewer's approval is only half the story: attach the forge's answer to
/// the last merge attempt on the PR it last reviewed, and flag every repository
/// it is looking at where the merge identity has no grant, so `/runners` says
/// "approved - merge blocked" instead of a plain "approved".
fn with_merge_outcome(state: &WebState, node: &mut RunnerNodeSummary) {
    let mut repos: Vec<String> = node
        .active_tasks
        .iter()
        .filter_map(|task| task.repo.clone())
        .collect();
    if let Some(last) = node.last_activity.as_mut() {
        repos.push(last.repo.clone());
        last.merge_attempt = last
            .pr
            .and_then(|pr| state.merge_attempts.last(&last.repo, pr));
    }
    repos.sort();
    repos.dedup();
    node.merge_grant_gaps = repos
        .iter()
        .filter_map(|repo| crate::web::merge_attempts::grant_gap(state, repo))
        .collect();
}

fn build_runner_nodes(
    seed: Vec<RunnerNodeSummary>,
    workcells: &[jeryu_runnerd::WorkcellLease],
    agent_runs: &[AgentRunStatusResponse],
) -> Vec<RunnerNodeSummary> {
    let mut nodes: BTreeMap<String, RunnerNodeSummary> = seed
        .into_iter()
        .map(|node| (node.runner_id.clone(), node))
        .collect();

    for lease in workcells {
        if lease.runner_id.is_empty() {
            continue;
        }
        nodes
            .entry(lease.runner_id.clone())
            .or_insert_with(|| RunnerNodeSummary {
                runner_id: lease.runner_id.clone(),
                source: "workcell".to_string(),
                state: "active".to_string(),
                capacity: 0,
                in_flight: 0,
                labels: Vec::new(),
                classes: Vec::new(),
                active_task_count: 0,
                last_updated: None,
                active_tasks: Vec::new(),
                last_activity: None,
                offline_after_seconds: None,
                merge_grant_gaps: Vec::new(),
            });
    }

    let workcell_by_id: BTreeMap<_, _> = workcells
        .iter()
        .cloned()
        .map(|lease| (lease.workcell_id.clone(), lease))
        .collect();

    for run in agent_runs
        .iter()
        .filter(|run| matches!(run.state, AgentRunState::Running))
    {
        let AgentRunSourceSnapshot::Workcell { workcell_id, .. } = &run.source else {
            continue;
        };
        let Some(lease) = workcell_by_id.get(workcell_id.as_str()) else {
            continue;
        };
        let task = runner_task_summary(run, lease);
        let node = nodes
            .entry(lease.runner_id.clone())
            .or_insert_with(|| RunnerNodeSummary {
                runner_id: lease.runner_id.clone(),
                source: "workcell".to_string(),
                state: "active".to_string(),
                capacity: 0,
                in_flight: 0,
                labels: Vec::new(),
                classes: Vec::new(),
                active_task_count: 0,
                last_updated: None,
                active_tasks: Vec::new(),
                last_activity: None,
                offline_after_seconds: None,
                merge_grant_gaps: Vec::new(),
            });
        node.active_tasks.push(task);
    }

    let mut out: Vec<_> = nodes
        .into_values()
        .map(|mut node| {
            node.active_tasks.sort_by(|a, b| a.task_id.cmp(&b.task_id));
            node.active_task_count = node.active_tasks.len() as u32;
            let task_last_updated = node
                .active_tasks
                .iter()
                .filter_map(|task| task.updated_at.clone())
                .max();
            node.last_updated = node.last_updated.take().or(task_last_updated);
            node
        })
        .collect();
    out.sort_by(|a, b| a.runner_id.cmp(&b.runner_id));
    out
}

fn runner_task_summary(
    run: &AgentRunStatusResponse,
    lease: &jeryu_runnerd::WorkcellLease,
) -> RunnerTaskSummary {
    let tty_lines = tty_preview_lines(&run.tty_events);
    let repo = run
        .tty_events
        .iter()
        .rev()
        .find_map(|event| event.repo.clone())
        .or_else(|| {
            lease
                .repo_roots
                .first()
                .map(|path| path.to_string_lossy().to_string())
        });
    let started_at = run
        .tty_events
        .first()
        .map(|event| rfc3339_from_ms(event.occurred_at_ms));
    let updated_at = run
        .tty_events
        .last()
        .map(|event| rfc3339_from_ms(event.occurred_at_ms));
    RunnerTaskSummary {
        task_id: run.agent_run_id.clone(),
        job_id: lease.workcell_id.clone(),
        agent_run_id: Some(run.agent_run_id.clone()),
        workcell_id: Some(lease.workcell_id.clone()),
        repo,
        label: task_label(&run.program),
        program: run.program.clone(),
        state: format!("{:?}", run.state).to_ascii_lowercase(),
        started_at,
        updated_at: updated_at.clone(),
        tty_preview: RunnerTtyPreview {
            state: if tty_lines.is_empty() {
                EvidenceState::Missing
            } else {
                EvidenceState::Fresh
            },
            lines: tty_lines,
        },
    }
}

pub(crate) fn tty_preview_lines(events: &[jeryu_agent_stream::AgentTtyEvent]) -> Vec<String> {
    let mut lines = Vec::new();
    for event in events {
        if let Some(text) = &event.text {
            for line in text.lines() {
                let line = line.trim_end();
                if !line.is_empty() {
                    lines.push(line.to_string());
                }
            }
        }
    }
    const MAX_PREVIEW_LINES: usize = 5;
    if lines.len() > MAX_PREVIEW_LINES {
        lines = lines[lines.len() - MAX_PREVIEW_LINES..].to_vec();
    }
    lines
}

pub(crate) fn task_label(program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(program)
        .to_string()
}

pub(crate) fn rfc3339_from_ms(ms: u64) -> String {
    DateTime::<Utc>::from_timestamp_millis(i64::try_from(ms).unwrap_or(i64::MAX))
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| ms.to_string())
}
