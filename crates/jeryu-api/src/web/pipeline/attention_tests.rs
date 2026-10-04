//! One test per attention kind against the pure rules, then the route.

use std::collections::{BTreeMap, BTreeSet};

use axum::http::{Method as HttpMethod, StatusCode};
use chrono::{DateTime, Duration, TimeZone, Utc};
use jeryu_core::{ForgeCore, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::attention::{
    ApiCall, Draft, DraftFacts, Hosts, Item, LatestDeployment, MirrorDrift, MirrorFailure,
    ProductionFacts, PullFacts, Severity, divergence_items, draft_items, mirror_items, order,
    pin_items, pull_items, queue_items, release_items, runner_items, shift_items, todo_items,
    web_routes, worker_items,
};
use super::pins::{BumpPr, Consumer, Pin, Unreleased};
use super::tests::{body_json, request, shift_forge};
use super::types::Event;
use crate::web::control_plane::{GateRunnerHeartbeat, GateRunnerRecord, GateRunnerResult};
use crate::web::merge_queue::{QueueEntry, QueueState};
use crate::web::pulls::PullPosture;
use crate::web::shift::{
    BlockKind, Heartbeat, ShiftBranch, ShiftPr, ShiftRepo, ShiftTodo, TodoStatus, WorkerRow,
};
use crate::web::{WebState, app};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 19, 13, 0, 0).unwrap()
}

fn todo(id: &str, status: &str) -> ShiftTodo {
    ShiftTodo {
        id: id.to_string(),
        family: "jeryu".to_string(),
        family_label: "jeryu".to_string(),
        title: format!("Title of {id}"),
        body: String::new(),
        repos: vec!["jeryu-deploy".to_string()],
        mode: "now".to_string(),
        priority: 2,
        blocked_by: Vec::new(),
        status: TodoStatus::parse(status).expect("known status"),
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
        park_until: String::new(),
        block_kind: BlockKind::derive(
            TodoStatus::parse(status).expect("known status"),
            &format!("Title of {id}"),
            "",
            "",
        ),
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
    // The href is the whole next step for an item with no command: it has to
    // be a route of the web app, not a path the app only redirects from.
    assert_web_route(item);
    assert!(!item.action.label.is_empty(), "{item:?}");
    match &item.action.command {
        Some(command) => assert!(item.next_step.contains(command.as_str()), "{item:?}"),
        None => assert!(item.next_step.contains(item.href.as_str()), "{item:?}"),
    }
    assert_says_where(item);
    assert_api_is_a_call(item);
}

/// An `action.api` is a call this API serves: a method and an absolute
/// `/api/v1` path, ready to send as it stands.
pub(super) fn assert_api_is_a_call(item: &Item) {
    let Some(api) = &item.action.api else {
        return;
    };
    assert_eq!(api.method, "POST", "{item:?}");
    assert!(api.path.starts_with("/api/v1/"), "{item:?}");
    assert!(!api.path.contains(' '), "{item:?}");
}

/// What one item's step is, as a call: `None` when no route performs it.
fn api(item: &Item) -> Option<(&str, &str, Value)> {
    item.action.api.as_ref().map(|api: &ApiCall| {
        (
            api.method,
            api.path.as_str(),
            api.body.clone().unwrap_or(Value::Null),
        )
    })
}

/// The href opens a page of the web app directly. `assert_actionable` runs
/// this over every rule's output in this file; the pins tests call it for
/// theirs.
pub(super) fn assert_web_route(item: &Item) -> &'static str {
    match web_routes::route_of(&item.href) {
        Ok(route) => route,
        Err(why) => panic!("{} emitted an href that opens nothing: {why}", item.kind),
    }
}

/// A command always says where it is run, and nothing else carries a place.
/// `kinds` runs this over every rule's output in this file; the pins tests
/// call it for theirs.
pub(super) fn assert_says_where(item: &Item) {
    assert_eq!(
        item.action.command.is_some(),
        item.action.run_in.is_some(),
        "{item:?}"
    );
    if let Some(run_in) = &item.action.run_in {
        assert!(!run_in.trim().is_empty(), "{item:?}");
        // A reader of `next_step` alone hears where before what.
        let command = item.action.command.as_deref().unwrap_or_default();
        let place = item.next_step.find(run_in.as_str());
        assert!(
            place.is_some() && place < item.next_step.find(command),
            "{item:?}"
        );
    }
}

/// The three roles on three distinct, non-default names, so a test proves the
/// rule used the right role and not a literal.
fn hosts() -> Hosts {
    Hosts::from_lookup(|name| {
        Some(
            match name {
                "JERYU_RELEASE_HOST" => "release-box",
                "JERYU_GATE_HOST" => "gate-box",
                "JERYU_FORGE_HOST" => "forge-box",
                other => panic!("unexpected variable {other}"),
            }
            .to_string(),
        )
    })
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
        true,
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
    assert_eq!(items[0].href, "/work/t-blocked?family=jeryu");
    assert_eq!(items[0].todo_id.as_deref(), Some("t-blocked"));
    assert_eq!(items[2].severity, Severity::Watch);
    assert_eq!(items[3].severity, Severity::Watch);
    assert!(items[3].reason.contains("alton@xbabe0/w1"));
    assert!(items[3].reason.contains("30 minutes ago"));
    assert!(items[4].reason.contains("which is blocked"));
    assert!(items[5].reason.contains("done but not merged"));
}

/// A one-line todo from the web is the worker's to triage, not a person's:
/// it only reaches the operator once a healthy worker has left it untriaged
/// for far longer than a pass takes.
#[test]
fn an_untriaged_todo_waits_for_the_workers_next_pass() {
    let untriaged = |id: &str, filed: &str| ShiftTodo {
        triaged: false,
        filed_at: filed.to_string(),
        ..todo(id, "open")
    };
    // Filed 5 minutes ago: the next pass has not even come round yet.
    let fresh = [untriaged("t-fresh", "2026-09-19T12:55:00Z")];
    assert!(
        todo_items("acme", &fresh, true, now()).is_empty(),
        "a worker triages it on its next pass"
    );
    // 31 minutes, and the worker that should have triaged it is healthy.
    let stale = [untriaged("t-stale", "2026-09-19T12:29:00Z")];
    let items = todo_items("acme", &stale, true, now());
    assert_eq!(kinds(&items), ["todo_untriaged"]);
    assert_eq!(items[0].severity, Severity::Watch);
    assert_eq!(items[0].id, "todo-untriaged:acme:t-stale");
    assert!(
        items[0]
            .reason
            .contains("acme workers triage on their next pass; not triaged after 31 minutes"),
        "{}",
        items[0].reason
    );
    assert_eq!(items[0].since.as_deref(), Some("2026-09-19T12:29:00Z"));
    // With no healthy worker the family already has a `workers_down` item;
    // naming the same outage once per untriaged todo says nothing new.
    assert!(todo_items("acme", &stale, false, now()).is_empty());
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
        review_pr: None,
        reviewed_todos: Vec::new(),
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
        family: "jeryu".to_string(),
        family_label: "jeryu".to_string(),
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

/// Opening a shift's review pull request is one call, so the item carries it
/// whole: branch and all. Moving todos into a replacement pull request
/// somebody else opened is not, so that item carries none.
#[test]
fn a_shift_branch_carries_the_call_that_opens_its_review_pull_request() {
    let repo = |name: &str, review_pr: Option<ShiftPr>| ShiftRepo {
        repo: name.to_string(),
        head: "9f8214948f1a8508fb95b1d3b941c162bf76a73a".to_string(),
        ahead: 2,
        behind: 0,
        pr: review_pr.as_ref().map(|_| ShiftPr {
            number: 71,
            state: "closed".to_string(),
            url: "/repos/jeryu/widget-shop/pulls/71".to_string(),
        }),
        unmerged_todos: vec!["t1".to_string()],
        review_pr,
        reviewed_todos: Vec::new(),
    };
    let items = shift_items(
        "acme",
        &[ShiftBranch {
            family: "acme".to_string(),
            family_label: "acme".to_string(),
            branch: "nightshift/2026-09-19".to_string(),
            kind: "nightshift".to_string(),
            date: "2026-09-19".to_string(),
            repos: vec![
                repo("widget-shop", None),
                repo(
                    "widget-api",
                    Some(ShiftPr {
                        number: 78,
                        state: "mergeable".to_string(),
                        url: "/repos/jeryu/widget-api/pulls/78".to_string(),
                    }),
                ),
            ],
            todo_ids: vec!["t1".to_string()],
        }],
    );
    assert_eq!(kinds(&items), ["shift_without_pr", "shift_without_pr"]);
    assert_eq!(
        api(&items[0]),
        Some((
            "POST",
            "/api/v1/shift/shifts/acme/pr",
            json!({"branch": "nightshift/2026-09-19"})
        ))
    );
    assert_eq!(api(&items[1]), None);
    for item in &items {
        assert_actionable(item);
    }
}

/// A shift PR closed on a queue conflict and replaced by an open PR from
/// another branch, cherry-picked onto the base: the todos the replacement
/// carries are under review, not waiting for a pull request of their own.
#[test]
fn a_shift_whose_todos_ride_a_replacement_pull_request() {
    let replaced = |name: &str, todos: &[&str], reviewed: &[&str]| ShiftRepo {
        repo: name.to_string(),
        head: "9f8214948f1a8508fb95b1d3b941c162bf76a73a".to_string(),
        ahead: 19,
        behind: 0,
        pr: Some(ShiftPr {
            number: 71,
            state: "closed".to_string(),
            url: "/repos/jeryu/jeryu-web/x/pulls/71".to_string(),
        }),
        unmerged_todos: todos.iter().map(|id| (*id).to_string()).collect(),
        review_pr: Some(ShiftPr {
            number: 78,
            state: "mergeable".to_string(),
            url: "/repos/jeryu/jeryu-web/x/pulls/78".to_string(),
        }),
        reviewed_todos: reviewed.iter().map(|id| (*id).to_string()).collect(),
    };
    let shift = |repos: Vec<ShiftRepo>| ShiftBranch {
        family: "jeryu".to_string(),
        family_label: "jeryu".to_string(),
        branch: "nightshift/2026-09-28".to_string(),
        kind: "nightshift".to_string(),
        date: "2026-09-28".to_string(),
        repos,
        todo_ids: vec!["t1".to_string(), "t2".to_string(), "t3".to_string()],
    };

    let all = shift(vec![replaced(
        "jeryu-web",
        &["t1", "t2", "t3"],
        &["t1", "t2", "t3"],
    )]);
    assert!(
        shift_items("jeryu", &[all]).is_empty(),
        "every todo is in the open replacement PR"
    );

    let items = shift_items(
        "jeryu",
        &[shift(vec![replaced(
            "jeryu-web",
            &["t1", "t2", "t3"],
            &["t1", "t3"],
        )])],
    );
    assert_eq!(kinds(&items), ["shift_without_pr"]);
    assert!(
        items[0]
            .reason
            .starts_with("1 finished todo(s) (t2) sit on"),
        "only the todo no open PR carries: {}",
        items[0].reason
    );
    assert!(items[0].reason.contains("open #78 that replaces it"));
    assert_eq!(items[0].href, "/repos/jeryu/jeryu-web/x/pulls/78");
    assert_eq!(
        items[0].action.label,
        "Add the missing todo(s) to the open review PR"
    );
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
        // Red, but only a check the base branch does not require (live:
        // jankurai/proof on a seed PR whose passport passes). Worth a look,
        // waiting on nobody.
        pull(
            7,
            5,
            PullPosture {
                failing_optional: vec!["jankurai/proof".to_string()],
                ..PullPosture::default()
            },
        ),
        // The same once it can merge: merging is the step, the red check a footnote.
        pull(
            8,
            45,
            PullPosture {
                can_merge: true,
                checks_green: true,
                failing_optional: vec!["jankurai/proof".to_string()],
                ..PullPosture::default()
            },
        ),
        // A required failure beside an optional one still blocks.
        pull(
            9,
            5,
            PullPosture {
                failing: vec!["jeryu-web/required".to_string()],
                failing_optional: vec!["jankurai/proof".to_string()],
                ..PullPosture::default()
            },
        ),
    ];
    let items = pull_items(&pulls, &[], now());
    assert_eq!(
        kinds(&items),
        [
            "pr_changes_requested",
            "pr_checks_failing",
            "pr_awaiting_approval",
            "pr_ready_to_merge",
            "pr_checks_failing",
            "pr_ready_to_merge",
            "pr_checks_failing",
        ]
    );
    assert_eq!(items[1].severity, Severity::Action);
    assert_eq!(items[4].severity, Severity::Watch, "{:?}", items[4]);
    assert!(
        items[4]
            .reason
            .starts_with("jankurai/proof failed on \"PR 7\""),
        "{}",
        items[4].reason
    );
    assert!(items[4].reason.contains("does not block the merge"));
    assert_eq!(items[5].severity, Severity::Action);
    assert!(
        items[5]
            .reason
            .contains("jankurai/proof failed, which the base branch does not")
    );
    assert_eq!(items[6].severity, Severity::Action);
    assert!(items[6].reason.starts_with("jeryu-web/required failed on"));
    assert_eq!(items[0].href, "/repos/jeryu/jeryu/jeryu-web/pulls/1");
    assert!(items[1].reason.contains("jeryu-web/required failed"));
    assert!(items[2].reason.contains("0 of 1 required approval"));
    assert!(items[3].reason.contains("45 minutes"));
    assert_eq!(items[3].pr, Some(4));
    // A pull request that has passed its gate lands by joining the queue;
    // every other posture waits on a review or a push, which no route does.
    assert_eq!(
        api(&items[3]),
        Some((
            "POST",
            "/api/v1/repos/jeryu/jeryu-web/pulls/4/queue",
            Value::Null
        ))
    );
    assert_eq!(api(&items[0]), None);
    assert_eq!(api(&items[2]), None);
}

fn draft(number: u64, days_old: i64, base_ref: &str) -> DraftFacts {
    DraftFacts {
        repo: "acme/widget-shop".to_string(),
        number,
        title: format!("draft {number}"),
        author: "dana".to_string(),
        base_ref: base_ref.to_string(),
        head_sha: "3f1c9e0b6a2d4857c1b0e9f7a4d2c6b8e0f1a3d5".to_string(),
        updated_at: now() - Duration::days(days_old),
    }
}

#[test]
fn a_draft_idle_past_the_threshold_is_waiting_on_a_person() {
    let drafts = [
        // Fresh, and one day short: a draft is allowed to be a draft.
        draft(1, 0, "main"),
        draft(2, 2, "main"),
        // Past the threshold, including into a base that is not the default
        // branch: the base has nothing to do with being stranded.
        draft(3, 3, "main"),
        draft(4, 11, "rc/auto"),
    ];
    let items = draft_items(&drafts, 3, now());
    assert_eq!(kinds(&items), ["pr_draft_waiting", "pr_draft_waiting"]);
    assert_eq!(items[0].pr, Some(3));
    assert_eq!(items[1].pr, Some(4));
    assert_eq!(items[0].severity, Severity::Action);
    assert_eq!(items[0].href, "/repos/jeryu/acme/widget-shop/pulls/3");
    assert!(
        items[1].reason.contains("into rc/auto"),
        "{}",
        items[1].reason
    );
    assert!(
        items[1].reason.contains("no push for 11 day(s)"),
        "{}",
        items[1].reason
    );
    assert!(
        items[1]
            .next_step
            .starts_with("Mark the draft ready for review")
    );
    assert_eq!(
        api(&items[1]),
        Some((
            "POST",
            "/api/v1/repos/acme/widget-shop/pulls/4/ready",
            Value::Null
        ))
    );

    // The threshold is what decides it, so raising it empties the list.
    assert!(draft_items(&drafts, 30, now()).is_empty());
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
        refusal_code: None,
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
    assert_eq!(
        api(&items[0]),
        Some((
            "POST",
            "/api/v1/repos/jeryu/jeryu-deploy/pulls/40/queue",
            Value::Null
        ))
    );
}

/// A refusal is not a dropped entry: it never built a queue commit, so
/// queueing it again is refused again and the step has to be something else.
#[test]
fn a_refused_enqueue_names_the_step_that_makes_the_pull_request_replayable() {
    let open: BTreeSet<(String, u64)> = [("acme/widgets".to_string(), 7)].into();
    let refused = |code: &str| {
        let mut entry = queue_entry(7, QueueState::Dequeued, 0);
        entry.repo = "acme/widgets".to_string();
        entry.reason = Some(format!("replay refused: {code}"));
        entry.refusal_code = Some(code.to_string());
        entry
    };

    let items = queue_items(&[refused("queue_conflict")], &open, now());
    assert_eq!(kinds(&items), ["queue_refused"]);
    assert_eq!(items[0].pr, Some(7));
    assert_eq!(items[0].severity, Severity::Action);
    assert_eq!(
        items[0].action.label,
        "Open a replacement PR from main with this PR's commits cherry-picked (main requires \
         linear history)"
    );
    assert!(
        !items[0].action.label.to_lowercase().contains("merge"),
        "queueing or merging it again is refused again: {}",
        items[0].action.label
    );
    assert!(
        items[0]
            .reason
            .starts_with("Replay refused: queue_conflict."),
        "{}",
        items[0].reason
    );
    assert!(items[0].reason.contains("never joined the queue"));
    assert_eq!(
        api(&items[0]),
        None,
        "queueing it again is refused again, so there is nothing to replay"
    );

    // Merge commits are the same question: the base wants linear history.
    let items = queue_items(&[refused("queue_merge_commits")], &open, now());
    assert_eq!(kinds(&items), ["queue_refused"]);
    assert!(items[0].action.label.starts_with("Open a replacement PR"));

    // A diff the replay did not reproduce is fixed by a new head.
    let items = queue_items(&[refused("queue_mismatch")], &open, now());
    assert_eq!(kinds(&items), ["queue_refused"]);
    assert_eq!(items[0].action.label, "Push a new head");
    assert_eq!(api(&items[0]), None);

    // A refusal the queue has no code of its own for: reading what it
    // reported and queueing again is the step, and the queue route does it.
    let items = queue_items(&[refused("queue_internal")], &open, now());
    assert_eq!(
        api(&items[0]),
        Some((
            "POST",
            "/api/v1/repos/acme/widgets/pulls/7/queue",
            Value::Null
        ))
    );
}

fn queued(number: u64, minutes_old: i64) -> QueueEntry {
    let mut entry = queue_entry(number, QueueState::Building, 0);
    entry.repo = "jeryu/jeryu-web".to_string();
    entry.enqueued_at = (now() - Duration::minutes(minutes_old)).to_rfc3339();
    entry.reason = None;
    entry
}

#[test]
fn a_pull_request_the_merge_queue_is_holding_is_not_ready_to_merge() {
    let pulls = [pull(
        7,
        45,
        PullPosture {
            can_merge: true,
            checks_green: true,
            approvals: 1,
            required_approvals: 1,
            ..PullPosture::default()
        },
    )];
    // Nothing queued: the PR has been mergeable for 45 minutes and waits on a
    // person, which is the rule this one qualifies.
    assert_eq!(
        kinds(&pull_items(&pulls, &[], now())),
        ["pr_ready_to_merge"]
    );

    // Building, and still inside the window a queue gate normally takes.
    assert!(pull_items(&pulls, &[queued(7, 15)], now()).is_empty());

    // Another pull request's entry says nothing about this one.
    assert_eq!(
        kinds(&pull_items(&pulls, &[queued(8, 15)], now())),
        ["pr_ready_to_merge"]
    );

    // Building far longer than a gate takes: worth a look, waiting on nobody.
    let items = pull_items(&pulls, &[queued(7, 95)], now());
    assert_eq!(kinds(&items), ["queue_stuck"]);
    assert_eq!(items[0].severity, Severity::Watch);
    assert_eq!(items[0].pr, Some(7));
    assert!(
        items[0].title.contains("for 95 minutes"),
        "{}",
        items[0].title
    );
    assert!(items[0].reason.contains("onto main"));
    assert!(items[0].action.label.starts_with("Check the gate runners"));

    // A queued pull request a reviewer has since blocked is still blocked.
    let blocked = [pull(
        7,
        45,
        PullPosture {
            changes_requested: 1,
            ..PullPosture::default()
        },
    )];
    assert_eq!(
        kinds(&pull_items(&blocked, &[queued(7, 15)], now())),
        ["pr_changes_requested"]
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
            interval_seconds: None,
            current: None,
            last: last.map(|(conclusion, pr)| GateRunnerResult {
                repo: "jeryu/jeryu-web".to_string(),
                pr: Some(pr),
                sha: "b761244b76371995527bfe7795e98492703553a8".to_string(),
                recipe: "review".to_string(),
                conclusion: conclusion.to_string(),
                target: None,
                reason: None,
                seconds: 12,
                finished_at: now() - Duration::minutes(3),
            }),
            code: None,
            tools: Vec::new(),
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
        &hosts(),
    );
    assert_eq!(kinds(&healthy), ["reviewer_stuck"]);
    assert!(
        healthy[0]
            .reason
            .contains("larger than the reviewer accepts")
    );
    assert_eq!(healthy[0].pr, Some(35));

    // Only a reviewer and a gate slot silent for ten minutes are left.
    let down = runner_items(&[stuck, stale_gate.clone()], &open, false, now(), &hosts());
    assert_eq!(kinds(&down), ["reviewer_stuck", "gate_runner_down"]);
    assert_eq!(down[1].severity, Severity::Critical);
    assert!(down[1].action.command.is_some());
    assert_eq!(
        down[1].action.run_in.as_deref(),
        Some("gate-box, any directory")
    );
    // No open PR and nothing queued: a silent gate is nobody's problem yet.
    let only_stale = std::slice::from_ref(&stale_gate);
    assert!(runner_items(only_stale, &BTreeSet::new(), false, now(), &hosts()).is_empty());
    assert_eq!(
        kinds(&runner_items(
            only_stale,
            &BTreeSet::new(),
            true,
            now(),
            &hosts()
        )),
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
    let items = worker_items(&families, &workers, &hosts());
    assert_eq!(kinds(&items), ["workers_down"]);
    assert_eq!(items[0].family.as_deref(), Some("jeryu"));
    assert_eq!(items[0].severity, Severity::Critical);
    assert_eq!(
        items[0].action.command.as_deref(),
        Some("systemctl --user status todoq-supervisor@jeryu")
    );
    // The family's own rows say which machine its supervisor is on.
    assert_eq!(
        items[0].action.run_in.as_deref(),
        Some("xbabe0, any directory")
    );
    assert_eq!(
        items[0].next_step,
        "Check the todoq supervisor on the worker host: on xbabe0, any directory, run \
         `systemctl --user status todoq-supervisor@jeryu`"
    );

    // A family nobody ever reported for, and one whose rows name no host:
    // the release host is where todoq runs.
    let mut nameless = worker("jeryu", "w1", false);
    nameless.heartbeat.host = " ".to_string();
    for rows in [Vec::new(), vec![nameless]] {
        let items = worker_items(&families[..1], &rows, &hosts());
        assert_eq!(kinds(&items), ["workers_down"]);
        assert_eq!(
            items[0].action.run_in.as_deref(),
            Some("release-box, any directory")
        );
    }

    // Two machines have reported for the family: the newest report wins.
    let mut moved = worker("jeryu", "supervisor", false);
    moved.heartbeat.host = "xbabe3".to_string();
    moved.last_seen = "2026-09-19T12:50:00Z".to_string();
    let items = worker_items(
        &families[..1],
        &[worker("jeryu", "w1", false), moved],
        &hosts(),
    );
    assert_eq!(
        items[0].action.run_in.as_deref(),
        Some("xbabe3, any directory")
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
        family_label: None,
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
        // Production runs an older release than the newest attempt.
        current_release: Some("prod-20260919T101500Z-9a1c0de-unsigned".to_string()),
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
fn a_failed_deploy_that_left_production_on_the_same_release_is_only_a_watch() {
    let live = "5fbe0ef2824d526ce03996cfa0cccca4ac3611d8";
    let release = "prod-20260922T161333Z-5fbe0ef-unsigned";

    // The release deployed fine, then was deployed again and the switch exited 1
    // without touching production: the live deployment is still the successful one.
    let mut facts = production(live, "2026-09-22T16:17:00Z", "failure");
    facts.current_release = Some(release.to_string());
    if let Some(latest) = facts.latest.as_mut() {
        latest.release = Some(release.to_string());
    }
    let items = release_items(None, None, &[facts.clone()], &hosts());
    assert_eq!(kinds(&items), ["deploy_failed"]);
    assert_eq!(items[0].severity, Severity::Watch);
    assert!(
        items[0].title.contains("production still runs it"),
        "{}",
        items[0].title
    );
    assert!(
        items[0].reason.contains("nothing changed"),
        "{}",
        items[0].reason
    );

    // A failure on a release production does not run is still critical.
    let mut moved_on = facts.clone();
    moved_on.current_release = Some("prod-20260922T120000Z-283416e-unsigned".to_string());
    let items = release_items(None, None, &[moved_on], &hosts());
    assert_eq!(items[0].severity, Severity::Critical);

    // Without release names in the payloads the commits decide.
    let mut by_sha = facts;
    by_sha.current_release = None;
    if let Some(latest) = by_sha.latest.as_mut() {
        latest.release = None;
        latest.sha = "01dfe680a6de5e02da4e9aa7821534742aa46d7e".to_string();
    }
    let items = release_items(None, None, &[by_sha], &hosts());
    assert_eq!(items[0].severity, Severity::Critical);
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
        &hosts(),
    );
    assert_eq!(kinds(&waiting), ["release_staged"]);
    assert_eq!(
        waiting[0].action.command.as_deref(),
        Some("scripts/release/deploy-release.sh prod-20260919T130210Z-01dfe68-unsigned")
    );
    assert!(waiting[0].reason.contains("283416e282"));
    assert!(
        !waiting[0].reason.contains("failed"),
        "{}",
        waiting[0].reason
    );

    // A failed attempt at this very release says why, from the status description.
    let mut attempt = production(live, "2026-09-19T12:29:36Z", "failure");
    if let Some(latest) = attempt.latest.as_mut() {
        latest.release = Some("prod-20260919T130210Z-01dfe68-unsigned".to_string());
        latest.description = Some("switch.sh exited 1: health check timed out".to_string());
    }
    let retry = release_items(Some(&staged), None, &[attempt], &hosts());
    assert_eq!(kinds(&retry), ["release_staged", "deploy_failed"]);
    assert_eq!(retry[1].severity, Severity::Critical);
    assert!(
        retry[0]
            .reason
            .ends_with("The last deploy of it failed: switch.sh exited 1: health check timed out."),
        "{}",
        retry[0].reason
    );
    assert!(waiting[0].next_step.contains("deploy-release.sh"));
    // The script is a path inside the repository the event names.
    assert_eq!(
        waiting[0].action.run_in.as_deref(),
        Some("release-box, in a jeryu/jeryu-deploy checkout")
    );

    // An event that names no repository still says where, without inventing one.
    let mut anonymous = staged.clone();
    anonymous.repo = None;
    let unnamed = release_items(Some(&anonymous), None, &[], &hosts());
    assert_eq!(kinds(&unnamed), ["release_staged"]);
    assert_eq!(
        unnamed[0].action.run_in.as_deref(),
        Some("release-box, in a checkout of the repository that staged it")
    );

    // Deployed: the same sha is live, or something newer than the staging is.
    assert!(
        release_items(
            Some(&staged),
            None,
            &[production(next, "2026-09-19T13:10:00Z", "success")],
            &hosts()
        )
        .is_empty()
    );
    assert!(
        release_items(
            Some(&staged),
            None,
            &[production(live, "2026-09-19T13:30:00Z", "success")],
            &hosts()
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
        kinds(&release_items(
            Some(&staged),
            Some(&gave_up),
            &prod_ok,
            &hosts()
        )),
        ["release_stage_failed"]
    );
    assert!(release_items(Some(&staged), Some(&retrying), &prod_ok, &hosts()).is_empty());
    assert!(release_items(Some(&staged), Some(&older), &prod_ok, &hosts()).is_empty());

    let failed = release_items(
        None,
        None,
        &[production(live, "2026-09-19T12:29:36Z", "failure")],
        &hosts(),
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
    assert_eq!(body["schema_version"], "jeryu.attention/v1.1");
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
    // A command item says where; an item without a command has no such key,
    // so an older web build sees the action it always saw.
    assert_eq!(
        of_kind("release_staged")["action"],
        json!({
            "label": "Deploy the staged release",
            "command": "scripts/release/deploy-release.sh prod-1",
            "run_in": "xbabe0, in a jeryu/jeryu-deploy checkout",
        })
    );
    assert_eq!(
        blocked["action"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["api", "command", "label"]
    );
    // The step as a call, beside the prose that spells it out.
    assert_eq!(
        of_kind("shift_without_pr")["action"]["api"],
        json!({
            "method": "POST",
            "path": "/api/v1/shift/shifts/jeryu/pr",
            "body": {"branch": "nightshift/2026-09-18"},
        })
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

/// An agent that replays an item's `action.api` as it stands fixes what the
/// item is about: the next collect no longer reports it.
#[tokio::test]
async fn replaying_an_items_api_call_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let (router, admin) = shift_forge(dir.path());
    let inbox = || async {
        let response = router
            .clone()
            .oneshot(request(HttpMethod::GET, "/api/v1/attention", &admin, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        body_json(response).await
    };
    let item_of = |body: &Value, kind: &str| {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["kind"] == kind)
            .cloned()
    };

    let before = inbox().await;
    let call = item_of(&before, "shift_without_pr").expect("the shift branch has no PR")["action"]
        ["api"]
        .clone();
    assert_eq!(call["method"], "POST");

    let replayed = router
        .clone()
        .oneshot(request(
            HttpMethod::POST,
            call["path"].as_str().unwrap(),
            &admin,
            Some(call["body"].clone()),
        ))
        .await
        .unwrap();
    assert_eq!(replayed.status(), StatusCode::OK);

    // The inbox keeps its answer for a few seconds; opening the pull request
    // invalidates it, so the next read is of the state the call left behind.
    let after = inbox().await;
    assert_eq!(
        item_of(&after, "shift_without_pr"),
        None,
        "the branch has a pull request now: {after}"
    );
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
            api: None,
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

#[test]
fn a_failing_mirror_is_one_item_however_many_repositories() {
    assert!(mirror_items(&[], &hosts()).is_empty());
    let failure = |repo: &str, minutes_ago: i64, ever: bool| MirrorFailure {
        repo: repo.to_string(),
        failed_at: now() - Duration::minutes(minutes_ago),
        last_success_at: ever.then(|| now() - Duration::days(30)),
        reason: format!(
            "git push https://x-access-token:***@github.com/neverhuman/{repo}.git abc:refs/heads/main \
             failed: remote: Invalid username or token. Password authentication is not supported."
        ),
    };
    let failures: Vec<MirrorFailure> = [
        "jeryu/jeryu-web",
        "jeryu/jeryu-core",
        "jeryu/jeryu-deploy",
        "jeryu/jeryu-tool",
        "jeryu/jeryu-jira",
        "jeryu/jeryu-cache",
    ]
    .iter()
    .enumerate()
    .map(|(i, repo)| failure(repo, 10 + i as i64, false))
    .collect();
    let items = mirror_items(&failures, &hosts());
    assert_eq!(kinds(&items), ["mirror_failing"]);
    let item = &items[0];
    assert_eq!(item.id, "mirror-failing");
    assert_eq!(item.severity, Severity::Action);
    assert_eq!(
        item.title,
        "The GitHub mirror is failing for 6 repositories"
    );
    // The reason leads with what git said, not with the command line.
    assert!(
        item.reason
            .starts_with("git says: remote: Invalid username or token."),
        "{}",
        item.reason
    );
    assert!(item.reason.contains(
        "jeryu/jeryu-cache, jeryu/jeryu-core, jeryu/jeryu-deploy, jeryu/jeryu-jira and 2 more"
    ));
    assert!(
        item.reason
            .contains("No push has ever succeeded from this host.")
    );
    assert!(!item.reason.contains("x-access-token"));
    assert_eq!(
        item.action.run_in.as_deref(),
        Some("forge-box, as the user the forge runs as")
    );
    // The oldest failure dates the item.
    assert_eq!(item.repo.as_deref(), Some("jeryu/jeryu-cache"));

    let mixed = [
        failure("jeryu/jeryu-web", 5, true),
        failure("jeryu/jeryu-core", 9, false),
    ];
    let item = &mirror_items(&mixed, &hosts())[0];
    assert_eq!(
        item.title,
        "The GitHub mirror is failing for 2 repositories"
    );
    assert!(
        item.reason
            .contains("1 of them have never had a successful push.")
    );
}

#[test]
fn hosts_default_to_the_three_machines_and_ignore_blank_overrides() {
    let defaults = Hosts::default();
    assert_eq!(
        (
            defaults.release.as_str(),
            defaults.gate.as_str(),
            defaults.forge.as_str()
        ),
        ("xbabe0", "xbabe2", "atomicsoul")
    );
    let blank = Hosts::from_lookup(|name| (name == "JERYU_GATE_HOST").then(|| "  ".to_string()));
    assert_eq!(blank, defaults);
    assert_eq!(hosts().gate, "gate-box");
    assert_eq!(Hosts::anywhere("xbabe0"), "xbabe0, any directory");
    assert_eq!(
        Hosts::checkout("xbabe0", "jeryu/jeryu-deploy"),
        "xbabe0, in a jeryu/jeryu-deploy checkout"
    );
}

#[test]
fn a_mirror_with_github_only_work_alarms_per_repository_and_names_the_commits() {
    assert!(divergence_items(&[]).is_empty());
    let drifts = [
        MirrorDrift {
            repo: "jeryu/jeryu-web".to_string(),
            github_slug: "neverhuman/jeryu-web".to_string(),
            branch_state: Some("diverged from the forge".to_string()),
            github_head: Some("c0ffee".to_string()),
            github_only_commits: vec!["c0ffee".to_string(), "decade".to_string()],
            tag_drift: Vec::new(),
            since: Some(now() - Duration::minutes(4)),
        },
        MirrorDrift {
            repo: "jeryu/jeryu-core".to_string(),
            github_slug: "neverhuman/jeryu-core".to_string(),
            branch_state: None,
            github_head: Some("beef01".to_string()),
            github_only_commits: Vec::new(),
            tag_drift: vec!["GitHub holds v5.0.0 at beef01 and the forge at 01beef".to_string()],
            since: Some(now()),
        },
    ];
    let items = divergence_items(&drifts);
    assert_eq!(kinds(&items), ["mirror_diverged", "mirror_diverged"]);
    let branch = &items[0];
    assert_eq!(branch.id, "mirror-diverged-jeryu/jeryu-web");
    assert_eq!(branch.severity, Severity::Critical);
    assert_eq!(
        branch.title,
        "GitHub has work the forge does not for jeryu/jeryu-web"
    );
    assert!(
        branch
            .reason
            .contains("2 commits the forge does not (c0ffee, decade)"),
        "{}",
        branch.reason
    );
    assert!(
        branch.reason.contains("nothing was forced"),
        "{}",
        branch.reason
    );
    // The repository page names the provider first, so the row is a live link.
    assert_eq!(branch.href, "/repos/jeryu/jeryu/jeryu-web");

    let tags = &items[1];
    assert!(
        tags.reason
            .contains("GitHub holds v5.0.0 at beef01 and the forge at 01beef"),
        "{}",
        tags.reason
    );
}

/// What a worker could not get past decides the step a person is offered:
/// releasing an over-budget todo stops it again, and an owner's task is not a
/// worker's to pick up at all, so neither item offers a release.
#[test]
fn a_blocked_todos_kind_picks_its_label() {
    let blocked = |id: &str, kind: BlockKind, note: &str| ShiftTodo {
        note: note.to_string(),
        block_kind: Some(kind),
        ..todo(id, "blocked")
    };
    let items = todo_items(
        "acme",
        &[
            blocked(
                "t-owner",
                BlockKind::OwnerTask,
                "OWNER: only alton can pick the name.",
            ),
            blocked(
                "t-budget",
                BlockKind::OverBudget,
                "Spent $12 of the $5 cap over 3 attempts.",
            ),
            blocked(
                "t-repo",
                BlockKind::UnknownRepo,
                "It asks for acme-ops, which the family config does not list.",
            ),
            blocked(
                "t-agent",
                BlockKind::AgentBlocked,
                "The gate fails on main too.",
            ),
        ],
        true,
        now(),
    );
    assert_eq!(kinds(&items), ["todo_blocked"; 4]);
    let label = |at: usize| items[at].action.label.as_str();
    assert_eq!(label(0), "Do it, then mark done");
    assert_eq!(label(1), "Close and refile smaller, or raise the cap");
    assert_eq!(label(2), "Add the repo to the family config, then release");
    assert_eq!(label(3), "Release the todo");
    // Only the steps a todo transition performs whole carry a call: marking
    // an owner's task done, releasing a blocked one.
    let call = |action: &str, id: &str| {
        Some((
            "POST",
            format!("/api/v1/shift/todos/acme/{id}/action"),
            json!({ "action": action }),
        ))
    };
    let called =
        |at: usize| api(&items[at]).map(|(method, path, body)| (method, path.to_string(), body));
    assert_eq!(called(0), call("done", "t-owner"));
    assert_eq!(called(3), call("release", "t-agent"));
    assert_eq!(called(1), None, "releasing it stops it again at once");
    assert_eq!(called(2), None, "the family config comes first");
    // An over-budget todo never offers a release, in the label or the step.
    assert!(!items[1].action.label.contains("Release"), "{:?}", items[1]);
    assert!(!items[1].next_step.contains("Release"), "{:?}", items[1]);
    assert!(
        items[1]
            .reason
            .contains("Spent $12 of the $5 cap over 3 attempts.")
            && items[1].reason.contains("summed over every attempt"),
        "{}",
        items[1].reason
    );
    assert!(
        items[0].reason.contains("Only the owner can do this"),
        "{}",
        items[0].reason
    );
    assert!(
        items[2].reason.contains("the family config does not list"),
        "{}",
        items[2].reason
    );
    for item in &items {
        assert_eq!(item.severity, Severity::Action);
        assert_actionable(item);
    }
}

/// A parked todo is only worth watching until its date; once it passes, it
/// waits on a person again. Done and closed work asks for nobody.
#[test]
fn a_parked_todo_waits_quietly_until_its_date() {
    let parked = |id: &str, until: &str| ShiftTodo {
        park_until: until.to_string(),
        note: "Waits on the split.8 tag.".to_string(),
        ..todo(id, "parked")
    };
    let items = todo_items(
        "acme",
        &[
            parked("t-later", "2026-09-26T09:00:00Z"),
            parked("t-due", "2026-09-18T09:00:00Z"),
            parked("t-forever", ""),
            todo("t-done", "done"),
            todo("t-closed", "closed"),
        ],
        true,
        now(),
    );
    assert_eq!(
        kinds(&items),
        ["todo_parked", "todo_parked", "todo_parked"],
        "done and closed work asks for nobody"
    );
    assert_eq!(items[0].id, "todo-parked:acme:t-later");
    assert_eq!(items[0].severity, Severity::Watch);
    assert_eq!(
        items[0].action.label,
        "Nothing to do until the park runs out"
    );
    assert!(
        items[0]
            .reason
            .contains("parked until 2026-09-26T09:00:00Z"),
        "{}",
        items[0].reason
    );
    assert_eq!(
        api(&items[0]),
        None,
        "a park that has not run out is nobody's step yet"
    );
    assert_eq!(items[1].severity, Severity::Action);
    assert_eq!(items[1].action.label, "Release the todo or park it again");
    assert_eq!(
        api(&items[1]),
        Some((
            "POST",
            "/api/v1/shift/todos/acme/t-due/action",
            json!({"action": "release"})
        ))
    );
    assert!(
        items[1].reason.contains("ran out at"),
        "{}",
        items[1].reason
    );
    // A park with no date comes back only when a person decides.
    assert_eq!(items[2].severity, Severity::Action);
    assert!(
        items[2].reason.contains("no date it comes back on"),
        "{}",
        items[2].reason
    );
    for item in &items {
        assert_actionable(item);
        assert!(item.reason.starts_with("Waits on the split.8 tag. "));
    }
}

/// Parking is not only for todos: any attention item can be acknowledged
/// until a date, and until then the inbox leaves it out. Finishing a todo
/// removes its item outright, with no acknowledgement needed.
#[tokio::test]
async fn an_acknowledged_item_is_hidden_until_its_date() {
    let dir = tempfile::tempdir().unwrap();
    let (router, admin) = shift_forge(dir.path());
    let call = |method, uri: String, body: Option<Value>| {
        let router = router.clone();
        let admin = admin.clone();
        async move {
            let response = router
                .oneshot(request(method, &uri, &admin, body))
                .await
                .unwrap();
            (response.status(), body_json(response).await)
        }
    };
    let inbox = || {
        let call = &call;
        async move {
            let (status, body) = call(HttpMethod::GET, "/api/v1/attention".to_string(), None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body
        }
    };

    // The fixture's shift branch holds a finished todo with no pull request:
    // an item that is nobody's todo, so only an acknowledgement can defer it.
    let body = inbox().await;
    let item = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "shift_without_pr")
        .expect("the fixture branch has work and no pull request")
        .clone();
    let id = item["id"].as_str().unwrap().to_string();
    assert_eq!(
        id,
        "shift-without-pr:jeryu:jeryu-deploy:nightshift/2026-09-18"
    );
    let action_count = body["counts"]["action"].as_u64().unwrap();

    let (status, ack) = call(
        HttpMethod::POST,
        "/api/v1/attention/acks".to_string(),
        Some(json!({"item_id": id, "until": "2026-12-01T00:00:00Z",
                    "note": "the shift PR waits on the release"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ack}");
    assert_eq!(ack["item_id"], id.as_str());
    assert_eq!(ack["until"], "2026-12-01T00:00:00Z");
    assert_eq!(ack["acked_by"], "alice");

    // Gone from the list and from the counts, with no wait for the cache.
    let body = inbox().await;
    assert!(
        !body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == id.as_str()),
        "{body}"
    );
    assert_eq!(body["counts"]["action"], action_count - 1);
    let (_, acks) = call(HttpMethod::GET, "/api/v1/attention/acks".to_string(), None).await;
    assert_eq!(acks["acks"][0]["item_id"], id.as_str());
    assert_eq!(acks["acks"][0]["note"], "the shift PR waits on the release");

    // An acknowledgement says "not now", not "never": once its date has
    // passed the item is listed again.
    let (status, _) = call(
        HttpMethod::POST,
        "/api/v1/attention/acks".to_string(),
        Some(json!({"item_id": id, "until": "2026-09-20T00:00:00Z"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let body = inbox().await;
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == id.as_str()),
        "an acknowledgement that ran out hides nothing: {body}"
    );

    // Dropping the acknowledgement lists it again at once, and the record of
    // what was deferred goes with it.
    let (status, _) = call(
        HttpMethod::POST,
        "/api/v1/attention/acks".to_string(),
        Some(json!({"item_id": id, "until": "2026-12-01T00:00:00Z"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = call(
        HttpMethod::POST,
        "/api/v1/attention/acks".to_string(),
        Some(json!({"item_id": id, "until": Value::Null})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(
        inbox().await["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == id.as_str())
    );
    let (_, acks) = call(HttpMethod::GET, "/api/v1/attention/acks".to_string(), None).await;
    assert_eq!(acks["acks"], json!([]));

    for bad in [
        json!({"item_id": "", "until": "2026-12-01T00:00:00Z"}),
        json!({"item_id": id, "until": "december"}),
        json!({"until": "2026-12-01T00:00:00Z"}),
    ] {
        let (status, body) = call(
            HttpMethod::POST,
            "/api/v1/attention/acks".to_string(),
            Some(bad.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert_eq!(body["code"], "invalid_input", "{bad}");
    }
}

/// A todo marked done leaves the inbox: nothing waits on a person any more.
#[tokio::test]
async fn finishing_a_todo_removes_its_item() {
    let dir = tempfile::tempdir().unwrap();
    let (router, admin) = shift_forge(dir.path());
    let call = |method, uri: String, body: Option<Value>| {
        let router = router.clone();
        let admin = admin.clone();
        async move {
            let response = router
                .oneshot(request(method, &uri, &admin, body))
                .await
                .unwrap();
            (response.status(), body_json(response).await)
        }
    };
    let (status, filed) = call(
        HttpMethod::POST,
        "/api/v1/shift/todos".to_string(),
        Some(json!({"family": "jeryu", "text": "Cut the tag", "mode": "now"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{filed}");
    let id = filed["id"].as_str().unwrap().to_string();
    // Blocking it puts it in the inbox: a fresh untriaged todo is still the
    // workers' own to triage, so it asks for nobody yet.
    let (status, blocked) = call(
        HttpMethod::POST,
        format!("/api/v1/shift/todos/jeryu/{id}/action"),
        Some(json!({"action": "block", "note": "the tag does not exist"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{blocked}");
    let waiting = |body: &Value| {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "todo_blocked" && item["todo_id"] == id.as_str())
    };
    let (_, body) = call(HttpMethod::GET, "/api/v1/attention".to_string(), None).await;
    assert!(waiting(&body), "{body}");

    let (status, done) = call(
        HttpMethod::POST,
        format!("/api/v1/shift/todos/jeryu/{id}/action"),
        Some(json!({"action": "done", "note": "did it by hand"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    // The inbox is computed from current state, so the item is gone at once.
    let (_, body) = call(HttpMethod::GET, "/api/v1/attention".to_string(), None).await;
    assert!(!waiting(&body), "{body}");
}

/// The route of the web app each kind's href opens. `web_routes::WEB_ROUTES`
/// is the app's own list of patterns; this says which of them the inbox hands
/// people, so a rule that emits a path the app does not answer — or only
/// answers by sending the reader somewhere else — fails here.
const KIND_ROUTES: &[(&str, &str)] = &[
    ("deploy_failed", "/releases"),
    ("gate_runner_down", "/runners"),
    ("mirror_diverged", "/repos/:provider/:owner/*"),
    ("mirror_failing", "/repos"),
    // The pin's own item points at Releases; once a bump pull request is open,
    // that pull request is the next step.
    ("pin_behind", "/releases"),
    ("pin_behind", "/repos/:provider/:owner/*"),
    ("pr_awaiting_approval", "/repos/:provider/:owner/*"),
    ("pr_changes_requested", "/repos/:provider/:owner/*"),
    ("pr_checks_failing", "/repos/:provider/:owner/*"),
    ("pr_draft_waiting", "/repos/:provider/:owner/*"),
    ("pr_ready_to_merge", "/repos/:provider/:owner/*"),
    ("queue_failed", "/repos/:provider/:owner/*"),
    ("queue_refused", "/repos/:provider/:owner/*"),
    ("queue_stuck", "/repos/:provider/:owner/*"),
    ("release_stage_failed", "/activity"),
    ("release_staged", "/releases"),
    ("reviewer_stuck", "/repos/:provider/:owner/*"),
    ("shift_stranded_work", "/work"),
    ("shift_without_pr", "/repos/:provider/:owner/*"),
    ("shift_without_pr", "/work"),
    ("todo_blocked", "/work/:key"),
    ("todo_handoff", "/work/:key"),
    ("todo_parked", "/work/:key"),
    ("todo_stuck_claim", "/work/:key"),
    ("todo_untriaged", "/work/:key"),
    ("todo_waiting_on_blocker", "/work/:key"),
    ("workers_down", "/work"),
];

/// One item of every kind the inbox can emit, from the rules themselves.
fn one_of_every_kind() -> Vec<Item> {
    let family = "acme";
    let repo = "acme/widget-shop";
    let mut items = Vec::new();

    let mut blocked = todo("t-blocked", "blocked");
    blocked.note = "The widget-shop tag does not exist; a person must cut it.".to_string();
    let mut parked = todo("t-parked", "parked");
    parked.park_until = "2026-09-26T09:00:00Z".to_string();
    let mut untriaged = todo("t-untriaged", "open");
    untriaged.triaged = false;
    let mut stuck = todo("t-stuck", "claimed");
    stuck.claim_by = "dana@w1".to_string();
    stuck.lease_until = "2026-09-19T12:30:00Z".to_string();
    let mut waiting = todo("t-waiting", "open");
    waiting.blocked_by = vec!["t-blocked".to_string()];
    items.extend(todo_items(
        family,
        &[
            blocked,
            todo("t-handoff", "handoff"),
            parked,
            untriaged,
            stuck,
            waiting,
        ],
        true,
        now(),
    ));

    let shift_repo =
        |name: &str, pr: Option<&str>, todos: &[&str], review: Option<u64>| ShiftRepo {
            repo: name.to_string(),
            head: "9f8214948f1a8508fb95b1d3b941c162bf76a73a".to_string(),
            ahead: 2,
            behind: 0,
            pr: pr.map(|state| ShiftPr {
                number: 7,
                state: state.to_string(),
                url: format!("/repos/jeryu/{name}/pulls/7"),
            }),
            unmerged_todos: todos.iter().map(|id| (*id).to_string()).collect(),
            review_pr: review.map(|number| ShiftPr {
                number,
                state: "mergeable".to_string(),
                url: format!("/repos/jeryu/{name}/pulls/{number}"),
            }),
            reviewed_todos: Vec::new(),
        };
    items.extend(shift_items(
        family,
        &[ShiftBranch {
            family: "jeryu".to_string(),
            family_label: "jeryu".to_string(),
            branch: "nightshift/2026-09-19".to_string(),
            kind: "nightshift".to_string(),
            date: "2026-09-19".to_string(),
            repos: vec![
                // No pull request at all, and one replaced by another repo's.
                shift_repo("acme/widget-shop", None, &["t1"], None),
                shift_repo("acme/widget-api", Some("closed"), &["t2"], Some(78)),
                // Work that landed after the pull request merged.
                shift_repo("acme/widget-web", Some("merged"), &["t3"], None),
            ],
            todo_ids: vec!["t1".to_string()],
        }],
    ));
    items.extend(worker_items(
        &[(family.to_string(), 3)],
        &[worker(family, "w1", false)],
        &hosts(),
    ));

    let pin = |bump: Option<u64>| Pin {
        dependency: "acme/widget-core".to_string(),
        kind: "commit",
        source: "acme-split.lock.toml".to_string(),
        pinned_ref: "0a1b2c3d4e5f60718293a4b5c6d7e8f901234567".to_string(),
        pinned_sha: Some("0a1b2c3d4e5f60718293a4b5c6d7e8f901234567".to_string()),
        latest_sha: Some("89abcdef0123456789abcdef0123456789abcdef".to_string()),
        latest_at: Some("2026-09-19T10:00:00Z".to_string()),
        behind: 4,
        latest_green: Some(true),
        state: "behind",
        bump_pr: bump.map(|number| BumpPr {
            number,
            state: "open".to_string(),
            url: format!("/repos/jeryu/{repo}/pulls/{number}"),
        }),
        unreleased: vec![Unreleased {
            sha: "89abcdef0123456789abcdef0123456789abcdef".to_string(),
            subject: "feat: price the basket once".to_string(),
        }],
    };
    items.extend(pin_items(
        &[Consumer {
            repo: repo.to_string(),
            family: Some(family.to_string()),
            branch: "main".to_string(),
            pins: vec![pin(None), pin(Some(53))],
        }],
        &[],
        &hosts(),
        now(),
    ));

    items.extend(mirror_items(
        &[MirrorFailure {
            repo: repo.to_string(),
            failed_at: now() - Duration::hours(2),
            last_success_at: None,
            reason: "push failed: remote: Invalid username or token".to_string(),
        }],
        &hosts(),
    ));
    items.extend(divergence_items(&[MirrorDrift {
        repo: repo.to_string(),
        github_slug: "acme/widget-shop".to_string(),
        branch_state: Some("diverged".to_string()),
        github_head: Some("89abcdef".to_string()),
        github_only_commits: vec!["89abcdef".to_string()],
        tag_drift: Vec::new(),
        since: Some(now() - Duration::hours(3)),
    }]));

    // One pull request per posture, plus one the queue has been building for
    // longer than a gate takes.
    let posture = |posture: PullPosture, number: u64| PullFacts {
        repo: repo.to_string(),
        posture,
        ..pull(number, 45, PullPosture::default())
    };
    let building = {
        let mut entry = queue_entry(15, QueueState::Building, 2);
        entry.repo = repo.to_string();
        entry
    };
    items.extend(pull_items(
        &[
            posture(
                PullPosture {
                    changes_requested: 1,
                    ..PullPosture::default()
                },
                8,
            ),
            posture(
                PullPosture {
                    failing: vec!["acme/required".to_string()],
                    ..PullPosture::default()
                },
                9,
            ),
            posture(
                PullPosture {
                    checks_green: true,
                    required_approvals: 1,
                    ..PullPosture::default()
                },
                10,
            ),
            posture(
                PullPosture {
                    checks_green: true,
                    can_merge: true,
                    ..PullPosture::default()
                },
                11,
            ),
            // The same, while the queue builds it: the queue is the next step.
            posture(
                PullPosture {
                    checks_green: true,
                    can_merge: true,
                    ..PullPosture::default()
                },
                15,
            ),
        ],
        std::slice::from_ref(&building),
        now(),
    ));
    items.extend(draft_items(&[draft(12, 9, "main")], 3, now()));

    let open: BTreeSet<(String, u64)> = [
        (repo.to_string(), 13),
        (repo.to_string(), 14),
        ("jeryu/jeryu-web".to_string(), 35),
    ]
    .into();
    let failed = {
        let mut entry = queue_entry(13, QueueState::Failed, 2);
        entry.repo = repo.to_string();
        entry
    };
    let refused = {
        let mut entry = queue_entry(14, QueueState::Dequeued, 2);
        entry.repo = repo.to_string();
        entry.refusal_code = Some("queue_conflict".to_string());
        entry
    };
    items.extend(queue_items(&[failed, refused], &open, now()));
    // A reviewer that gave up on an open pull request, and no gate runner at
    // all: one item each.
    items.extend(runner_items(
        &[
            runner("review-1", &["redteam"], 30, Some(("hold", 35))),
            // A gate slot silent for ten minutes is a gate with no runner.
            runner("gate-1", &["pr-gate"], 600, None),
        ],
        &open,
        true,
        now(),
        &hosts(),
    ));

    let staged = release_event(
        7,
        "release.staged",
        "01dfe680a6de5e02da4e9aa7821534742aa46d7e",
        "2026-09-19T12:40:00Z",
        false,
    );
    let gave_up = release_event(
        9,
        "release.stage_failed",
        "283416e0a6de5e02da4e9aa7821534742aa46d7e",
        "2026-09-19T12:50:00Z",
        true,
    );
    items.extend(release_items(
        Some(&staged),
        Some(&gave_up),
        &[production(
            "5fbe0ef2824d526ce03996cfa0cccca4ac3611d8",
            "2026-09-19T12:27:00Z",
            "failure",
        )],
        &hosts(),
    ));
    items
}

/// Every kind's href opens a page of the web app, and the whole inbox is
/// covered: no kind is missing from [`KIND_ROUTES`] and none is listed there
/// that no rule emits.
#[test]
fn every_kind_opens_a_route_of_the_web_app() {
    let items = one_of_every_kind();
    let walked: BTreeSet<(&str, &str)> = items
        .iter()
        .map(|item| (item.kind, assert_web_route(item)))
        .collect();
    assert_eq!(
        walked,
        KIND_ROUTES.iter().copied().collect::<BTreeSet<_>>(),
        "the kinds the rules emit and the routes they open"
    );
}
