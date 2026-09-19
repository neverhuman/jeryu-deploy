//! One test per attention kind against the pure rules, then the route.

use std::collections::{BTreeMap, BTreeSet};

use axum::http::{Method as HttpMethod, StatusCode};
use chrono::{DateTime, Duration, TimeZone, Utc};
use jeryu_core::{ForgeCore, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::attention::{
    Draft, Item, LatestDeployment, ProductionFacts, PullFacts, Severity, order, pull_items,
    queue_items, release_items, runner_items, shift_items, todo_items, worker_items,
};
use super::tests::{body_json, request, shift_forge};
use super::types::Event;
use crate::web::control_plane::{GateRunnerHeartbeat, GateRunnerRecord, GateRunnerResult};
use crate::web::merge_queue::{QueueEntry, QueueState};
use crate::web::pulls::PullPosture;
use crate::web::shift::{Heartbeat, ShiftBranch, ShiftPr, ShiftRepo, ShiftTodo, WorkerRow};
use crate::web::{WebState, app};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 19, 13, 0, 0).unwrap()
}

fn todo(id: &str, status: &str) -> ShiftTodo {
    ShiftTodo {
        id: id.to_string(),
        family: "jeryu".to_string(),
        title: format!("Title of {id}"),
        body: String::new(),
        repos: vec!["jeryu-deploy".to_string()],
        mode: "now".to_string(),
        priority: 2,
        blocked_by: Vec::new(),
        status: status.to_string(),
        attempts: 0,
        requested_by: "alton".to_string(),
        filed_at: "2026-09-19T12:00:00Z".to_string(),
        claim_by: String::new(),
        lease_until: String::new(),
        lease_live: false,
        shift: String::new(),
        change_set: String::new(),
        commits: BTreeMap::new(),
        merged: false,
        released: None,
        pr: None,
        prs: Vec::new(),
        cost_usd: None,
        note: String::new(),
        triaged: true,
        worked_by: Vec::new(),
    }
}

/// Every item names exactly one next step a reader can act on cold.
fn assert_actionable(item: &Item) {
    assert!(
        !item.title.is_empty() && !item.reason.is_empty(),
        "{item:?}"
    );
    assert!(item.href.starts_with('/'), "{item:?}");
    assert!(!item.action.label.is_empty(), "{item:?}");
    match &item.action.command {
        Some(command) => assert!(item.next_step.contains(command.as_str()), "{item:?}"),
        None => assert!(item.next_step.contains(item.href.as_str()), "{item:?}"),
    }
}

fn kinds(items: &[Item]) -> Vec<&'static str> {
    items.iter().for_each(assert_actionable);
    items.iter().map(|item| item.kind).collect()
}

#[test]
fn todos_blocked_handed_off_untriaged_stuck_or_behind_a_blocker() {
    let mut blocked = todo("t-blocked", "blocked");
    blocked.note = "The jeryu-core tag split.7 does not exist; a human must cut it.".to_string();
    let handoff = todo("t-handoff", "handoff");
    let mut untriaged = todo("t-untriaged", "open");
    untriaged.triaged = false;
    let mut stuck = todo("t-stuck", "claimed");
    stuck.claim_by = "alton@xbabe0/w1".to_string();
    stuck.lease_until = "2026-09-19T12:30:00Z".to_string();
    let mut renewing = todo("t-renewing", "claimed");
    renewing.lease_until = "2026-09-19T13:05:00Z".to_string();
    renewing.lease_live = true;
    let mut just_lapsed = todo("t-lapsed", "claimed");
    just_lapsed.lease_until = "2026-09-19T12:55:00Z".to_string();
    let mut behind_blocked = todo("t-behind-blocked", "open");
    behind_blocked.blocked_by = vec!["t-blocked".to_string()];
    let mut unmerged = todo("t-done-unmerged", "done");
    unmerged.merged = false;
    let mut behind_unmerged = todo("t-behind-unmerged", "open");
    behind_unmerged.blocked_by = vec!["t-done-unmerged".to_string()];
    let mut merged = todo("t-done-merged", "done");
    merged.merged = true;
    let mut free = todo("t-free", "open");
    free.blocked_by = vec!["t-done-merged".to_string()];

    let items = todo_items(
        "jeryu",
        &[
            blocked,
            handoff,
            untriaged,
            stuck,
            renewing,
            just_lapsed,
            behind_blocked,
            unmerged,
            behind_unmerged,
            merged,
            free,
        ],
        now(),
    );
    assert_eq!(
        kinds(&items),
        [
            "todo_blocked",
            "todo_handoff",
            "todo_untriaged",
            "todo_stuck_claim",
            "todo_waiting_on_blocker",
            "todo_waiting_on_blocker",
        ],
        "a live lease, a lease that lapsed 5 minutes ago and a merged blocker are quiet"
    );
    assert_eq!(items[0].id, "todo-blocked:jeryu:t-blocked");
    assert_eq!(items[0].severity, Severity::Action);
    assert!(
        items[0]
            .reason
            .starts_with("The jeryu-core tag split.7 does not exist; a human must cut it. "),
        "the note leads, verbatim: {}",
        items[0].reason
    );
    assert_eq!(items[0].action.label, "Release the todo");
    assert_eq!(items[0].href, "/work/shift?family=jeryu&todo=t-blocked");
    assert_eq!(items[0].todo_id.as_deref(), Some("t-blocked"));
    assert_eq!(items[3].severity, Severity::Watch);
    assert!(items[3].reason.contains("alton@xbabe0/w1"));
    assert!(items[3].reason.contains("30 minutes ago"));
    assert!(items[4].reason.contains("which is blocked"));
    assert!(items[5].reason.contains("done but not merged"));
}

#[test]
fn a_shift_branch_with_work_and_no_pull_request() {
    let repo = |name: &str, ahead, pr: Option<&str>| ShiftRepo {
        repo: name.to_string(),
        head: "9f8214948f1a8508fb95b1d3b941c162bf76a73a".to_string(),
        ahead,
        behind: 0,
        pr: pr.map(|state| ShiftPr {
            number: 7,
            state: state.to_string(),
            url: "/repos/jeryu/jeryu/x/pulls/7".to_string(),
        }),
        unmerged_todos: Vec::new(),
    };
    let stranded = |name: &str, state: &str, todos: &[&str]| ShiftRepo {
        unmerged_todos: todos.iter().map(|id| (*id).to_string()).collect(),
        ..repo(name, 1, Some(state))
    };
    let unreviewed = |name: &str, ahead, pr: Option<&str>, todos: &[&str]| ShiftRepo {
        unmerged_todos: todos.iter().map(|id| (*id).to_string()).collect(),
        ..repo(name, ahead, pr)
    };
    let shifts = [ShiftBranch {
        branch: "bulletshift/2026-09-19".to_string(),
        kind: "bulletshift".to_string(),
        date: "2026-09-19".to_string(),
        repos: vec![
            unreviewed("jeryu-deploy", 2, None, &["t1", "t2"]),
            unreviewed("jeryu-web", 1, Some("closed"), &["t2"]),
            repo("jeryu-core", 3, Some("mergeable")),
            repo("jeryu-ci-runner", 0, None),
            // Closed and replaced by a rebased branch that merged: the commits
            // stay "ahead" by sha for ever, but every todo is on the base.
            repo("jeryu-redline", 6, Some("closed")),
            // Ahead with no pull request, and nothing a todo is waiting on.
            repo("jeryu-intelligence", 2, None),
            // Merged by replay: still "ahead" by sha, but every todo is on base.
            stranded("jeryu-tool", "merged", &[]),
            // A todo landed after the pull request merged: finished, going nowhere.
            stranded("jeryu-jira", "merged", &["20260919-053426-6e19e4"]),
            // The same on a branch whose pull request is still open rides that one.
            stranded("jeryu-cache", "mergeable", &["t9"]),
        ],
        todo_ids: vec!["t1".to_string(), "t2".to_string()],
    }];
    let items = shift_items("jeryu", &shifts);
    assert_eq!(
        kinds(&items),
        [
            "shift_without_pr",
            "shift_without_pr",
            "shift_stranded_work"
        ]
    );
    assert_eq!(items[2].repo.as_deref(), Some("jeryu-jira"));
    assert_eq!(items[2].todo_id.as_deref(), Some("20260919-053426-6e19e4"));
    assert!(items[2].reason.contains("already merged"));
    assert_eq!(items[2].action.label, "Open a new review PR for the branch");
    assert_eq!(items[0].repo.as_deref(), Some("jeryu-deploy"));
    assert!(
        items[0]
            .reason
            .starts_with("2 finished todo(s) (t1, t2) sit on bulletshift/2026-09-19"),
        "{}",
        items[0].reason
    );
    assert_eq!(items[1].repo.as_deref(), Some("jeryu-web"));
    assert_eq!(items[0].shift.as_deref(), Some("bulletshift/2026-09-19"));
    assert_eq!(items[0].action.label, "Open the shift's review PR");
}

fn pull(number: u64, minutes_old: i64, posture: PullPosture) -> PullFacts {
    PullFacts {
        repo: "jeryu/jeryu-web".to_string(),
        number,
        title: format!("PR {number}"),
        author: "alton2".to_string(),
        head_sha: "b761244b76371995527bfe7795e98492703553a8".to_string(),
        updated_at: now() - Duration::minutes(minutes_old),
        posture,
    }
}

#[test]
fn pull_requests_waiting_on_a_person() {
    let pulls = [
        pull(
            1,
            5,
            PullPosture {
                changes_requested: 1,
                failing: vec!["jeryu-web/required".to_string()],
                ..PullPosture::default()
            },
        ),
        pull(
            2,
            5,
            PullPosture {
                failing: vec!["jeryu-web/required".to_string()],
                ..PullPosture::default()
            },
        ),
        pull(
            3,
            5,
            PullPosture {
                checks_green: true,
                approvals: 0,
                required_approvals: 1,
                ..PullPosture::default()
            },
        ),
        pull(
            4,
            45,
            PullPosture {
                can_merge: true,
                checks_green: true,
                approvals: 1,
                required_approvals: 1,
                ..PullPosture::default()
            },
        ),
        // Just became mergeable: the merge bot gets its ten minutes.
        pull(
            5,
            2,
            PullPosture {
                can_merge: true,
                checks_green: true,
                ..PullPosture::default()
            },
        ),
        // Checks still running: nobody's turn yet.
        pull(6, 30, PullPosture::default()),
    ];
    let items = pull_items(&pulls, now());
    assert_eq!(
        kinds(&items),
        [
            "pr_changes_requested",
            "pr_checks_failing",
            "pr_awaiting_approval",
            "pr_ready_to_merge",
        ]
    );
    assert_eq!(items[0].href, "/repos/jeryu/jeryu/jeryu-web/pulls/1");
    assert!(items[1].reason.contains("jeryu-web/required failed"));
    assert!(items[2].reason.contains("0 of 1 required approval"));
    assert!(items[3].reason.contains("45 minutes"));
    assert_eq!(items[3].pr, Some(4));
}

fn queue_entry(number: u64, state: QueueState, hours_old: i64) -> QueueEntry {
    QueueEntry {
        repo: "jeryu/jeryu-deploy".to_string(),
        base: "main".to_string(),
        number,
        pr_head_sha: "01dfe680a6de5e02da4e9aa7821534742aa46d7e".to_string(),
        base_sha: String::new(),
        queue_ref: format!("refs/queue/main/{number}"),
        queue_sha: String::new(),
        state,
        enqueued_at: (now() - Duration::hours(hours_old)).to_rfc3339(),
        enqueued_by: "jain-merge-bot".to_string(),
        approvers: Vec::new(),
        attempts: Vec::new(),
        reason: Some("the gate failed on abc and def".to_string()),
        landed_sha: None,
    }
}

#[test]
fn a_failed_merge_queue_entry_whose_pull_request_is_still_open() {
    let open: BTreeSet<(String, u64)> = [
        ("jeryu/jeryu-deploy".to_string(), 40),
        ("jeryu/jeryu-deploy".to_string(), 41),
        ("jeryu/jeryu-deploy".to_string(), 44),
    ]
    .into();
    let entries = [
        queue_entry(40, QueueState::Failed, 2),
        queue_entry(41, QueueState::Dequeued, 1),
        queue_entry(42, QueueState::Failed, 2), // its PR was closed since
        queue_entry(43, QueueState::Landed, 1),
        queue_entry(44, QueueState::Failed, 30), // older than a day
        queue_entry(45, QueueState::Building, 0),
    ];
    let items = queue_items(&entries, &open, now());
    assert_eq!(kinds(&items), ["queue_failed", "queue_failed"]);
    assert_eq!(items[0].pr, Some(40));
    assert!(
        items[0]
            .reason
            .starts_with("The gate failed on abc and def. "),
        "the queue's own reason leads: {}",
        items[0].reason
    );
}

fn runner(
    id: &str,
    labels: &[&str],
    seconds_ago: i64,
    last: Option<(&str, u64)>,
) -> GateRunnerRecord {
    GateRunnerRecord {
        heartbeat: GateRunnerHeartbeat {
            runner_id: id.to_string(),
            host: "xbabe2".to_string(),
            slot: 0,
            labels: labels.iter().map(|l| (*l).to_string()).collect(),
            current: None,
            last: last.map(|(conclusion, pr)| GateRunnerResult {
                repo: "jeryu/jeryu-web".to_string(),
                pr,
                sha: "b761244b76371995527bfe7795e98492703553a8".to_string(),
                recipe: "review".to_string(),
                conclusion: conclusion.to_string(),
                seconds: 12,
                finished_at: now() - Duration::minutes(3),
            }),
        },
        reporter: "gatebot".to_string(),
        received_at: now() - Duration::seconds(seconds_ago),
    }
}

#[test]
fn a_reviewer_without_a_verdict_and_a_gate_with_no_runner() {
    let open: BTreeSet<(String, u64)> = [("jeryu/jeryu-web".to_string(), 35)].into();
    let stuck = runner(
        "xbabe0/pr-redteam",
        &["redteam"],
        30,
        Some(("too_large", 35)),
    );
    let approved = runner(
        "xbabe0/pr-redteam-2",
        &["redteam"],
        30,
        Some(("approve", 35)),
    );
    let closed_pr = runner("xbabe0/pr-redteam-3", &["redteam"], 30, Some(("hold", 99)));
    let gate = runner("xbabe2/slot0", &["pr-gate"], 30, None);
    let stale_gate = runner("xbabe2/slot1", &["pr-gate"], 600, None);

    let healthy = runner_items(
        &[stuck.clone(), approved, closed_pr, gate, stale_gate.clone()],
        &open,
        false,
        now(),
    );
    assert_eq!(kinds(&healthy), ["reviewer_stuck"]);
    assert!(
        healthy[0]
            .reason
            .contains("larger than the reviewer accepts")
    );
    assert_eq!(healthy[0].pr, Some(35));

    // Only a reviewer and a gate slot silent for ten minutes are left.
    let down = runner_items(&[stuck, stale_gate.clone()], &open, false, now());
    assert_eq!(kinds(&down), ["reviewer_stuck", "gate_runner_down"]);
    assert_eq!(down[1].severity, Severity::Critical);
    assert!(down[1].action.command.is_some());
    // No open PR and nothing queued: a silent gate is nobody's problem yet.
    let only_stale = std::slice::from_ref(&stale_gate);
    assert!(runner_items(only_stale, &BTreeSet::new(), false, now()).is_empty());
    assert_eq!(
        kinds(&runner_items(only_stale, &BTreeSet::new(), true, now())),
        ["gate_runner_down"],
        "the merge queue waiting on a gate counts"
    );
}

fn worker(family: &str, slot: &str, healthy: bool) -> WorkerRow {
    WorkerRow {
        heartbeat: Heartbeat {
            operator: "alton@xbabe0".to_string(),
            host: "xbabe0".to_string(),
            slot: slot.to_string(),
            family: family.to_string(),
            state: "idle".to_string(),
            todo_id: None,
            stage: None,
            lease_until: None,
            shift: None,
            planned_slots: None,
            schedule: None,
            version: None,
        },
        last_seen: "2026-09-19T12:45:19Z".to_string(),
        healthy,
    }
}

#[test]
fn a_family_with_queued_work_and_no_healthy_worker() {
    let families = [
        ("jeryu".to_string(), 3),
        ("jain".to_string(), 2),
        ("veox-ai".to_string(), 0),
    ];
    let workers = [
        // A healthy supervisor is not a worker slot.
        worker("jeryu", "supervisor", true),
        worker("jeryu", "w1", false),
        worker("jain", "w1", true),
        worker("jain", "w1.", false),
    ];
    let items = worker_items(&families, &workers);
    assert_eq!(kinds(&items), ["workers_down"]);
    assert_eq!(items[0].family.as_deref(), Some("jeryu"));
    assert_eq!(items[0].severity, Severity::Critical);
    assert_eq!(
        items[0].action.command.as_deref(),
        Some("systemctl --user status todoq-supervisor@jeryu")
    );
}

fn release_event(seq: i64, kind: &str, sha: &str, ts: &str, needs_human: bool) -> Event {
    Event {
        seq,
        ts: ts.to_string(),
        event_id: None,
        source: "auto-stage".to_string(),
        kind: kind.to_string(),
        reporter: "alton".to_string(),
        actor: None,
        family: None,
        repo: Some("jeryu/jeryu-deploy".to_string()),
        pr: None,
        sha: Some(sha.to_string()),
        todo_id: None,
        shift: None,
        outcome: None,
        needs_human,
        summary: "x".to_string(),
        reason: None,
        cost_usd: None,
        seconds: None,
        log_tail: None,
        log_url: None,
        detail: Some(json!({
            "release": "prod-20260919T130210Z-01dfe68-unsigned",
            "deploy_command": "scripts/release/deploy-release.sh prod-20260919T130210Z-01dfe68-unsigned",
        })),
    }
}

fn production(sha: &str, at: &str, latest_state: &str) -> ProductionFacts {
    let at = DateTime::parse_from_rfc3339(at)
        .unwrap()
        .with_timezone(&Utc);
    ProductionFacts {
        repo: "jeryu/jeryu-deploy".to_string(),
        current: Some((sha.to_string(), at)),
        latest: Some(LatestDeployment {
            state: Some(latest_state.to_string()),
            release: Some("prod-20260919T122707Z-283416e-unsigned".to_string()),
            sha: sha.to_string(),
            description: Some("switch.sh exited 1".to_string()),
            created_at: at,
        }),
    }
}

#[test]
fn a_staged_release_a_staging_that_gave_up_and_a_failed_deploy() {
    let live = "283416e2824d526ce03996cfa0cccca4ac3611d8";
    let next = "01dfe680a6de5e02da4e9aa7821534742aa46d7e";
    let staged = release_event(10, "release.staged", next, "2026-09-19T13:03:26Z", false);

    // Production runs an older commit, deployed before the staging.
    let waiting = release_items(
        Some(&staged),
        None,
        &[production(live, "2026-09-19T12:29:36Z", "success")],
    );
    assert_eq!(kinds(&waiting), ["release_staged"]);
    assert_eq!(
        waiting[0].action.command.as_deref(),
        Some("scripts/release/deploy-release.sh prod-20260919T130210Z-01dfe68-unsigned")
    );
    assert!(waiting[0].reason.contains("283416e282"));
    assert!(waiting[0].next_step.contains("deploy-release.sh"));

    // Deployed: the same sha is live, or something newer than the staging is.
    assert!(
        release_items(
            Some(&staged),
            None,
            &[production(next, "2026-09-19T13:10:00Z", "success")]
        )
        .is_empty()
    );
    assert!(
        release_items(
            Some(&staged),
            None,
            &[production(live, "2026-09-19T13:30:00Z", "success")]
        )
        .is_empty()
    );

    // A staging failure counts only when it gave up and nothing staged since.
    let gave_up = release_event(
        11,
        "release.stage_failed",
        next,
        "2026-09-19T13:20:00Z",
        true,
    );
    let retrying = release_event(
        11,
        "release.stage_failed",
        next,
        "2026-09-19T13:20:00Z",
        false,
    );
    let older = release_event(
        9,
        "release.stage_failed",
        next,
        "2026-09-19T12:50:00Z",
        true,
    );
    let prod_ok = [production(next, "2026-09-19T13:10:00Z", "success")];
    assert_eq!(
        kinds(&release_items(Some(&staged), Some(&gave_up), &prod_ok)),
        ["release_stage_failed"]
    );
    assert!(release_items(Some(&staged), Some(&retrying), &prod_ok).is_empty());
    assert!(release_items(Some(&staged), Some(&older), &prod_ok).is_empty());

    let failed = release_items(
        None,
        None,
        &[production(live, "2026-09-19T12:29:36Z", "failure")],
    );
    assert_eq!(kinds(&failed), ["deploy_failed"]);
    assert_eq!(failed[0].severity, Severity::Critical);
    assert!(failed[0].reason.contains("switch.sh exited 1"));
}

#[tokio::test]
async fn attention_route_is_admin_only_and_reads_current_state() {
    let dir = tempfile::tempdir().unwrap();
    let (router, admin) = shift_forge(dir.path());
    let call = |method, uri: &str, token: &str, body: Option<Value>| {
        router.clone().oneshot(request(method, uri, token, body))
    };

    // Block a todo and report a staged release, as the pipeline would.
    let filed = body_json(
        call(
            HttpMethod::POST,
            "/api/v1/shift/todos",
            &admin,
            Some(
                json!({"family": "jeryu", "text": "Cut the tag", "mode": "now",
                        "title": "Cut the tag", "repos": ["jeryu-deploy"]}),
            ),
        )
        .await
        .unwrap(),
    )
    .await;
    let id = filed["id"].as_str().unwrap().to_string();
    call(
        HttpMethod::POST,
        &format!("/api/v1/shift/todos/jeryu/{id}/action"),
        &admin,
        Some(json!({"action": "block", "note": "A human must cut tag split.7 first."})),
    )
    .await
    .unwrap();
    let staged = call(
        HttpMethod::POST,
        "/api/v1/events",
        &admin,
        Some(json!({
            "source": "auto-stage", "kind": "release.staged", "repo": "jeryu/jeryu-deploy",
            "sha": "01dfe680a6de5e02da4e9aa7821534742aa46d7e",
            "summary": "staged prod-1",
            "detail": {"release": "prod-1", "deploy_command": "scripts/release/deploy-release.sh prod-1"},
        })),
    )
    .await
    .unwrap();
    assert_eq!(staged.status(), StatusCode::CREATED);

    let response = call(HttpMethod::GET, "/api/v1/attention", &admin, None)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["schema_version"], "jeryu.attention/v1");
    let items = body["items"].as_array().unwrap();
    let kinds: Vec<&str> = items.iter().map(|i| i["kind"].as_str().unwrap()).collect();
    // Critical first (open todos and no worker has ever reported), then the
    // undated item. The todo and the staged release carry whole-second
    // timestamps taken a moment apart, so which of them leads depends on
    // whether a second ticked in between: `order` has its own test, and here
    // only their presence is asserted.
    assert_eq!(kinds[..2], ["workers_down", "shift_without_pr"], "{body}");
    let mut rest = kinds[2..].to_vec();
    rest.sort_unstable();
    assert_eq!(rest, ["release_staged", "todo_blocked"], "{body}");
    assert_eq!(
        body["counts"],
        json!({"critical": 1, "action": 3, "watch": 0})
    );
    let of_kind = |kind: &str| items.iter().find(|i| i["kind"] == kind).unwrap();
    let blocked = of_kind("todo_blocked");
    assert_eq!(blocked["todo_id"], id.as_str());
    assert!(
        blocked["reason"]
            .as_str()
            .unwrap()
            .contains("A human must cut tag split.7 first.")
    );
    assert_eq!(
        of_kind("release_staged")["action"]["command"],
        "scripts/release/deploy-release.sh prod-1"
    );
    assert_eq!(items[1]["shift"], "nightshift/2026-09-18");

    // An ordinary account reads neither the inbox nor its counts.
    let core = ForgeCore::new();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    let user = core
        .create_personal_access_token("bob", "t", None)
        .unwrap()
        .secret;
    let other = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let denied = other
        .clone()
        .oneshot(request(HttpMethod::GET, "/api/v1/attention", &user, None))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(denied).await["code"], "permission_denied");
    let anonymous = other
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/v1/attention")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
}

/// `since` arrives in several RFC 3339 spellings. Ordering the text put
/// `…:38.100Z` before `…:37Z`-style neighbours by punctuation; the order is by
/// instant, undated first, id as the tie-break.
#[test]
fn items_order_by_instant_not_by_timestamp_spelling() {
    let item = |id: &str, since: Option<&str>| {
        let mut item = Draft {
            id: id.to_string(),
            kind: "todo_blocked",
            severity: Severity::Action,
            title: id.to_string(),
            reason: String::new(),
            href: "/work/shift".to_string(),
            label: "Open",
            command: None,
        }
        .build();
        item.since = since.map(str::to_string);
        item
    };
    let mut items = vec![
        item("fraction-later", Some("2026-09-19T14:45:38.100Z")),
        item("offset-same-second", Some("2026-09-19T14:45:37.900+00:00")),
        item("whole-second", Some("2026-09-19T14:45:37Z")),
        item("b-undated", None),
        item("a-undated", None),
    ];
    order(&mut items);
    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "a-undated",
            "b-undated",
            "whole-second",
            "offset-same-second",
            "fraction-later"
        ]
    );
}
