use std::sync::Arc;

use chrono::Utc;
use jeryu_agent_stream::{AgentOutputStream, AgentRunStreamKey, AgentTtyEvent};
use jeryu_core::{
    CheckConclusion, CheckRun, CheckRunStatus, CreateCheckRunRequest, CreatePullRequestRequest,
    CreateRepositoryRequest, ForgeCore, check_conclusion_wire_value,
};
use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::web::WebState;

fn seeded_state() -> Arc<WebState> {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "feature".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("head-no-checks".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "failing".to_string(),
            head: "failing".to_string(),
            base: "main".to_string(),
            head_sha: Some("head-failing".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "ci/fast".to_string(),
            head_sha: "head-failing".to_string(),
            status: Some(CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Failure),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    // A failure on a commit no open PR points at (e.g. a merged PR's head):
    // history, which must not count as open work.
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "ci/fast".to_string(),
            head_sha: "other-head".to_string(),
            status: Some(CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Failure),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    Arc::new(WebState::new(core))
}

#[test]
fn priority_rules_rank_missing_pr_checks_and_failing_ci() {
    let snapshot = snapshot(&seeded_state());
    let ids: Vec<&str> = snapshot
        .priorities
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    assert!(
        ids.iter().any(|id| id.contains("checks-missing")),
        "missing PR head checks must be explicit priority evidence"
    );
    assert!(ids.contains(&"ci-failing-checks"));
    assert_eq!(snapshot.priorities[0].rules_version, RULES_VERSION);
    assert!(snapshot.priorities[0].score >= snapshot.priorities[1].score);
}

#[test]
fn summary_and_graph_count_only_active_prs_and_their_head_checks() {
    let state = seeded_state();
    let snapshot = snapshot(&state);
    assert_eq!(snapshot.summary.open_pr_count, 2);
    assert_eq!(
        snapshot.summary.failing_check_count, 1,
        "stale other-head failure excluded"
    );
    assert_eq!(snapshot.repos[0].failing_checks, 1);
    // The raw lists still carry history for views that want it.
    assert_eq!(snapshot.check_runs.len(), 2);

    let graph = repo_graph_response(&state, None);
    let check_heads: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|node| node.kind == "check_run")
        .filter_map(|node| node.metadata.get("headSha").map(String::as_str))
        .collect();
    assert_eq!(check_heads, vec!["head-failing"]);
}

#[test]
fn active_view_drops_merged_and_closed_prs_and_their_checks() {
    let pr = |number: u64, state: &str, head: &str| ControlPullRequest {
        repo: "alice/jeryu".to_string(),
        number,
        title: String::new(),
        author: "alice".to_string(),
        draft: false,
        state: state.to_string(),
        head_ref: String::new(),
        head_sha: head.to_string(),
        base_ref: "main".to_string(),
        base_sha: String::new(),
        mergeable: false,
        mergeable_state: String::new(),
        changed_files: Vec::new(),
        checks: CheckSummary {
            total: 1,
            queued: 0,
            running: 0,
            failing: 1,
            successful: 0,
            missing: false,
        },
        state_evidence: EvidenceState::Fresh,
        source_links: Vec::new(),
    };
    let check = |head: &str| ControlCheckRun {
        id: head.to_string(),
        repo: "alice/jeryu".to_string(),
        name: "ci".to_string(),
        head_sha: head.to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        started_at: String::new(),
        completed_at: None,
        details_url: None,
        state: EvidenceState::Failed,
    };
    let prs = vec![
        pr(1, "blockedbychecks", "a"),
        pr(2, "merged", "b"),
        pr(3, "closed", "c"),
    ];
    let checks = vec![check("a"), check("b"), check("c")];
    let (active, current) = active_view(&prs, &checks);
    assert_eq!(
        active.iter().map(|pr| pr.number).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(
        current
            .iter()
            .map(|c| c.head_sha.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
}

#[test]
fn artifacts_absence_is_not_success() {
    let response = artifacts(&seeded_state());
    assert_eq!(response.state, EvidenceState::Missing);
    assert!(!response.absence_is_success);
    assert_eq!(response.latest_release.artifact_count, 0);
}

#[test]
fn mirror_degrades_explicitly_when_unavailable() {
    let remote = remote_status();
    assert_eq!(remote.state, EvidenceState::Missing);
    assert_eq!(remote.divergence.state, EvidenceState::Unknown);
    assert!(remote.divergence.reason.contains("unknown"));
}

#[test]
fn repo_graph_contains_ci_runner_and_mirror_clusters() {
    let graph = repo_graph_response(&seeded_state(), None);
    assert!(graph.nodes.iter().any(|node| node.kind == "repo"));
    assert!(
        graph
            .clusters
            .iter()
            .any(|cluster| cluster.kind == "ci_blocker")
    );
    assert!(
        graph
            .clusters
            .iter()
            .any(|cluster| cluster.kind == "runner_capacity")
    );
    assert!(
        graph
            .clusters
            .iter()
            .any(|cluster| cluster.kind == "superseded_mirror")
    );
}

fn report(state: &Arc<WebState>, runner_id: &str, gating: bool, at: chrono::DateTime<Utc>) {
    let beat: GateRunnerHeartbeat = serde_json::from_value(json!({
        "runnerId": runner_id,
        "host": "xbabe2",
        "slot": 0,
        "current": if gating { json!({
            "repo": "veox/jain-web", "pr": 13,
            "sha": "abc30d78ca5eadc15694dd1434d9f8f99c44a0d3",
            "recipe": "just required", "startedAt": at.to_rfc3339()
        }) } else { serde_json::Value::Null },
        "last": {
            "repo": "veox/jain-deploy", "pr": 31, "sha": "3926cbd7",
            "recipe": "just required", "conclusion": "success",
            "seconds": 46, "finishedAt": at.to_rfc3339()
        }
    }))
    .unwrap();
    state.gate_runners.record(beat, "gatebot", at).unwrap();
}

#[test]
fn runner_fabric_is_unknown_until_a_runner_reports() {
    let state = seeded_state();
    let runners = runner_fabric(&state);
    assert_eq!(runners.local.state, EvidenceState::Unknown);
    assert_eq!(runners.local.total_slots, 0);
    assert!(runners.local.node_details.is_empty());
    assert_eq!(runners.mirror.state, EvidenceState::Missing);
}

#[test]
fn runner_fabric_reports_live_gate_runners() {
    let state = seeded_state();
    let now = Utc::now();
    report(&state, "xbabe2/slot0", true, now);
    report(&state, "xbabe2/slot1", false, now);
    report(
        &state,
        "xbabe2/slot2",
        false,
        now - chrono::Duration::seconds(600),
    );

    let runners = runner_fabric_at(&state, now);
    assert_eq!(runners.local.state, EvidenceState::Fresh);
    assert_eq!(runners.local.nodes, 1);
    assert_eq!(runners.local.online_runners, 2);
    assert_eq!(runners.local.offline_runners, 1);
    assert_eq!(runners.local.busy_runners, 1);
    assert_eq!(runners.local.idle_runners, 1);
    assert_eq!(runners.local.active_slots, 2);
    assert_eq!(runners.local.total_slots, 3);

    let slot0 = &runners.local.node_details[0];
    assert_eq!(slot0.runner_id, "xbabe2/slot0");
    assert_eq!(slot0.state, "active");
    assert_eq!(slot0.active_tasks[0].repo.as_deref(), Some("veox/jain-web"));
    assert_eq!(slot0.last_activity.as_ref().unwrap().conclusion, "success");
    let slot2 = &runners.local.node_details[2];
    assert_eq!(slot2.state, "offline");
    assert!(slot2.active_tasks.is_empty());
}

#[test]
fn mcp_facade_returns_limited_graph_jobs_and_blockers() {
    let state = seeded_state();

    let status = mcp_status(&state);
    assert_eq!(status["schemaVersion"], SCHEMA_VERSION);
    assert_eq!(status["localAuthority"]["state"], "fresh");

    let priorities = mcp_priorities(&state, &json!({ "limit": 1 }));
    assert_eq!(priorities["priorities"].as_array().unwrap().len(), 1);

    let clusters = mcp_repo_graph_clusters(
        &state,
        &json!({ "cluster_kind": "runner_capacity", "limit": 1 }),
    );
    assert_eq!(clusters["clusters"].as_array().unwrap().len(), 1);
    assert_eq!(clusters["clusters"][0]["kind"], "runner_capacity");

    let graph = mcp_repo_graph_query(
        &state,
        &json!({
            "repo": "alice/jeryu",
            "query": "feature",
            "limit": 3
        }),
    );
    assert_eq!(graph["schemaVersion"], "jeryu.repo_graph/v1");
    assert!(graph["nodes"].as_array().unwrap().len() <= 3);

    let remote = mcp_remote_status();
    assert_eq!(remote["state"], "missing");
    let artifacts = mcp_artifacts_latest(&state);
    assert_eq!(artifacts["absenceIsSuccess"], false);
    let runners = mcp_runner_fabric_status(&state);
    assert_eq!(runners["local"]["state"], "unknown");
    report(&state, "xbabe2/slot0", false, Utc::now());
    let runners = mcp_runner_fabric_status(&state);
    assert_eq!(runners["local"]["state"], "fresh");

    let jobs = mcp_ci_run_jobs(&state, &json!({ "ci_run_id": "run-1" }));
    assert_eq!(jobs["ci_run_id"], "run-1");
    // Raw job listing keeps every check run (seeded: one per head).
    assert_eq!(jobs["jobs"].as_array().unwrap().len(), 2);

    let bottlenecks = mcp_ci_bottlenecks(&state, &json!({ "repo": "alice/jeryu" }));
    assert_eq!(bottlenecks["repo"], "alice/jeryu");
    assert!(!bottlenecks["bottlenecks"].as_array().unwrap().is_empty());

    let blockers = mcp_explain_blockers(
        &state,
        &json!({ "entity_type": "pull_request", "entity_id": "alice/jeryu#1" }),
    );
    assert_eq!(blockers["mergeable"], false);
    assert_eq!(blockers["entity_type"], "pull_request");

    let plan = mcp_plan_validation(
        &state,
        &json!({ "repo": "alice/jeryu", "ref_name": "feature" }),
    );
    assert_eq!(plan["rules_version"], RULES_VERSION);
    assert!(!plan["lanes"].as_array().unwrap().is_empty());
}

#[test]
fn helper_branches_normalize_tty_time_and_check_states() {
    let run = AgentRunStreamKey {
        repo: Some("alice/jeryu".to_string()),
        workcell_id: "wc-1".to_string(),
        agent_run_id: "run-1".to_string(),
        agent: "codex".to_string(),
        model: "gpt-5".to_string(),
    };
    let events: Vec<_> = (0..7)
        .map(|seq| {
            AgentTtyEvent::text(
                seq,
                1_700_000_000_000 + seq,
                &run,
                AgentOutputStream::Stdout,
                format!("line-{seq}\n"),
            )
        })
        .collect();
    let preview = tty_preview_lines(&events);
    assert_eq!(preview.len(), 5);
    assert_eq!(preview[0], "line-2");
    assert_eq!(task_label("/usr/bin/codex"), "codex");
    assert_eq!(task_label("/"), "/");
    assert!(rfc3339_from_ms(0).starts_with("1970-01-01T00:00:00"));
    assert_eq!(rfc3339_from_ms(u64::MAX), u64::MAX.to_string());

    let checks = vec![
        CheckRun {
            id: Uuid::from_u128(1),
            owner: "alice".to_string(),
            repo: "jeryu".to_string(),
            name: "queued".to_string(),
            head_sha: "head".to_string(),
            status: CheckRunStatus::Queued,
            conclusion: None,
            started_at: Utc::now(),
            completed_at: None,
            details_url: None,
            output: None,
        },
        CheckRun {
            id: Uuid::from_u128(2),
            owner: "alice".to_string(),
            repo: "jeryu".to_string(),
            name: "running".to_string(),
            head_sha: "head".to_string(),
            status: CheckRunStatus::InProgress,
            conclusion: None,
            started_at: Utc::now(),
            completed_at: None,
            details_url: None,
            output: None,
        },
        CheckRun {
            id: Uuid::from_u128(3),
            owner: "alice".to_string(),
            repo: "jeryu".to_string(),
            name: "pass".to_string(),
            head_sha: "head".to_string(),
            status: CheckRunStatus::Completed,
            conclusion: Some(CheckConclusion::Success),
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            details_url: None,
            output: None,
        },
        CheckRun {
            id: Uuid::from_u128(4),
            owner: "alice".to_string(),
            repo: "jeryu".to_string(),
            name: "fail".to_string(),
            head_sha: "head".to_string(),
            status: CheckRunStatus::Completed,
            conclusion: Some(CheckConclusion::TimedOut),
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            details_url: None,
            output: None,
        },
    ];
    let summary = summarize_checks(&checks);
    assert_eq!(summary.queued, 1);
    assert_eq!(summary.running, 1);
    assert_eq!(summary.successful, 1);
    assert_eq!(summary.failing, 1);
    assert_eq!(check_state(&checks[0]), EvidenceState::Queued);
    assert_eq!(check_state(&checks[1]), EvidenceState::Fresh);
    assert_eq!(check_state(&checks[3]), EvidenceState::Failed);
    assert_eq!(check_status(&CheckRunStatus::Queued), "queued");
    assert_eq!(check_status(&CheckRunStatus::InProgress), "in_progress");
    assert_eq!(check_status(&CheckRunStatus::Completed), "completed");
    assert_eq!(
        check_conclusion(&CheckConclusion::ActionRequired),
        "action_required"
    );
    assert_eq!(check_conclusion(&CheckConclusion::Cancelled), "cancelled");
    assert_eq!(check_conclusion(&CheckConclusion::Failure), "failure");
    assert_eq!(check_conclusion(&CheckConclusion::Neutral), "neutral");
    assert_eq!(check_conclusion(&CheckConclusion::Success), "success");
    assert_eq!(check_conclusion(&CheckConclusion::Skipped), "skipped");
    assert_eq!(
        check_conclusion(&CheckConclusion::Superseded),
        check_conclusion_wire_value(&CheckConclusion::Superseded)
    );
    assert_eq!(check_conclusion(&CheckConclusion::TimedOut), "timed_out");
}

#[test]
fn bootstrap_tui_pools_match_the_live_runner_fabric() {
    let state = seeded_state();
    let now = Utc::now();
    report(&state, "xbabe2/slot0", true, now);
    report(&state, "xbabe2/slot1", false, now);
    report(
        &state,
        "xbabe2/slot2",
        false,
        now - chrono::Duration::seconds(600),
    );

    let runners = runner_fabric(&state).local;
    let tui = crate::web::workcells::live_tui(&state);
    let pool = &tui.pool_activity.pools[0];
    assert_eq!(pool.online_runners, runners.online_runners);
    assert_eq!(pool.active_slots, runners.active_slots);
    assert_eq!(pool.configured_max_slots, runners.total_slots);
    assert_eq!(pool.stuck_runners, runners.offline_runners);
    assert_eq!(
        pool.configured_max_slots, 3,
        "reporting slots, not a fixture"
    );
    assert_eq!(tui.system.runners.online, 2);
    assert!(matches!(
        tui.system.scm.status,
        jeryu_readmodel::HealthLevel::Unknown
    ));
}

#[test]
fn active_repo_jobs_ignore_checks_off_open_pr_heads() {
    let state = seeded_state();
    let core = state.github.core();
    let before: u32 = active_repo_jobs(core).iter().map(|jobs| jobs.failed).sum();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "old".to_string(),
            head_sha: "0000000000000000000000000000000000000000".to_string(),
            status: Some(CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Failure),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    let after: u32 = active_repo_jobs(core).iter().map(|jobs| jobs.failed).sum();
    assert_eq!(
        before, after,
        "a failure off any open PR head is not active"
    );
}
