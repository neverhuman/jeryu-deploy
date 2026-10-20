//! One test per attention kind against the pure rules, then the route.

use std::collections::{BTreeMap, BTreeSet};

use axum::http::{Method as HttpMethod, StatusCode};
use chrono::{DateTime, Duration, TimeZone, Utc};
use jeryu_core::{ForgeCore, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::attention::{
    ATTENTION_SCHEMA, ApiCall, AttentionResponse, BoardFacts, BoardLane, Counts, Draft, DraftFacts,
    Filter, Hosts, Item, KINDS, LatestDeployment, MainRedFacts, MirrorDrift, MirrorFailure,
    ProductionFacts, PullFacts, PushedAfterMergeFacts, ReviewerOfflineThresholds, Severity,
    StrandedBranchFacts, SupersededPullFacts, board_items, budget_items, digest_items,
    divergence_items, draft_items, drop_shadowed_pull_rows, gate_definition_paths, main_red_items,
    mirror_items, only_matching, order, pin_items, pull_items, pushed_after_merge_items,
    queue_items, release_items, runner_items, shift_items, split_backlog, stranded_backlog_items,
    stranded_branch_items, superseded_pull_items, todo_items, web_routes, worker_items,
};
use super::pins::{BumpPr, Consumer, Pin, Unreleased};
use super::tests::{body_json, request, shift_forge};
use super::types::Event;
use crate::web::control_plane::{
    GateRunnerHeartbeat, GateRunnerRecord, GateRunnerResult, GateRunnerTask,
};
use crate::web::merge_queue::{QueueEntry, QueueState};
use crate::web::pulls::landed::Landed;
use crate::web::pulls::{GateFailure, PullPosture};
use crate::web::shift::{
    BlockKind, Heartbeat, ShiftBranch, ShiftPr, ShiftRepo, ShiftTodo, TodoStatus, WorkerRow,
};
use crate::web::{WebState, app};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 19, 13, 0, 0).unwrap()
}

#[test]
fn superseded_pull_rows_name_replacement_and_offer_close_call() {
    let old = SupersededPullFacts {
        author: "alice".into(),
        base_ref: "main".into(),
        source_repository: String::new(),
        repo: "acme/widgets".into(),
        number: 4,
        title: "first".into(),
        body: None,
        head_ref: "feature".into(),
        created_at: now(),
        status: "open",
        recorded_replacement: None,
    };
    let replacement = SupersededPullFacts {
        author: "alice".into(),
        base_ref: "main".into(),
        source_repository: String::new(),
        repo: "acme/widgets".into(),
        number: 5,
        title: "replacement".into(),
        body: Some("Replaces #4".into()),
        head_ref: "feature-r2".into(),
        created_at: now() + Duration::hours(1),
        status: "merged",
        recorded_replacement: None,
    };
    let items = superseded_pull_items(&[old, replacement]);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, "pr_superseded");
    assert!(items[0].title.contains("#4") && items[0].title.contains("#5"));
    assert!(items[0].reason.contains("merged"));
    assert_eq!(items[0].action.api.as_ref().unwrap().method, "PATCH");
    assert_eq!(
        items[0].action.api.as_ref().unwrap().body,
        Some(json!({"state": "closed", "superseded_by": 5}))
    );
}

#[test]
fn a_recorded_replacement_is_read_instead_of_derived() {
    let old = SupersededPullFacts {
        author: "alice".into(),
        base_ref: "main".into(),
        source_repository: String::new(),
        repo: "acme/widgets".into(),
        number: 141,
        title: "first go".into(),
        body: None,
        head_ref: "feature".into(),
        created_at: now(),
        status: "open",
        recorded_replacement: Some(145),
    };
    // Nothing about this pull request would derive the link: another author,
    // another base, another branch, and it says nothing about #141. The
    // recorded link is the record, so the row names it anyway.
    let replacement = SupersededPullFacts {
        author: "bruno".into(),
        base_ref: "release".into(),
        source_repository: String::new(),
        repo: "acme/widgets".into(),
        number: 145,
        title: "landed go".into(),
        body: None,
        head_ref: "cherry-pick".into(),
        created_at: now() + Duration::hours(2),
        status: "merged",
        recorded_replacement: None,
    };
    let derived = SupersededPullFacts {
        recorded_replacement: None,
        ..old.clone()
    };
    assert!(superseded_pull_items(&[derived, replacement.clone()]).is_empty());
    let items = superseded_pull_items(&[old, replacement]);
    assert_eq!(items.len(), 1);
    assert!(items[0].title.contains("#145"));
    assert_eq!(
        items[0].action.api.as_ref().unwrap().body,
        Some(json!({"state": "closed", "superseded_by": 145}))
    );
}

#[test]
fn replacement_text_and_branch_suffix_cannot_close_another_authors_or_scopes_pull() {
    let old = SupersededPullFacts {
        repo: "acme/widgets".into(),
        author: "alice".into(),
        base_ref: "main".into(),
        source_repository: String::new(),
        number: 4,
        title: "first".into(),
        body: None,
        head_ref: "feature".into(),
        created_at: now(),
        status: "open",
        recorded_replacement: None,
    };
    let newer = SupersededPullFacts {
        number: 5,
        title: "replaces #4".into(),
        body: Some("supersedes #4".into()),
        head_ref: "feature-r2".into(),
        created_at: now() + Duration::hours(1),
        ..old.clone()
    };
    assert_eq!(
        superseded_pull_items(&[old.clone(), newer.clone()]).len(),
        1
    );
    for field in ["author", "base", "source"] {
        let mut unrelated = newer.clone();
        match field {
            "author" => unrelated.author = "mallory".into(),
            "base" => unrelated.base_ref = "release".into(),
            _ => unrelated.source_repository = "mallory/widgets".into(),
        }
        assert!(
            superseded_pull_items(&[old.clone(), unrelated]).is_empty(),
            "{field}"
        );
    }
}

#[test]
fn superseded_pull_rows_follow_r_chains_and_ignore_newest_or_other_repositories() {
    let pull =
        |number, repo: &str, head: &str, created_at, body: Option<&str>| SupersededPullFacts {
            author: "alice".into(),
            base_ref: "main".into(),
            source_repository: String::new(),
            repo: repo.into(),
            number,
            title: format!("pull {number}"),
            body: body.map(str::to_string),
            head_ref: head.into(),
            created_at,
            status: "open",
            recorded_replacement: None,
        };
    let rows = superseded_pull_items(&[
        pull(1, "acme/widgets", "feature", now(), None),
        pull(
            2,
            "acme/widgets",
            "feature-r2",
            now() + Duration::minutes(1),
            None,
        ),
        pull(
            3,
            "acme/widgets",
            "feature-r3",
            now() + Duration::minutes(2),
            Some("Supersedes #2"),
        ),
        pull(
            4,
            "other/widgets",
            "feature-r4",
            now() + Duration::minutes(3),
            Some("replaces #1"),
        ),
    ]);
    assert_eq!(
        rows.iter().map(|row| row.pr.unwrap()).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(rows[0].title.contains("#3"));
    assert!(rows[1].title.contains("#3"));
}

fn todo(id: &str, status: &str) -> ShiftTodo {
    ShiftTodo {
        hypothesis: Default::default(),
        outcome_check_due_at: None,
        id: id.to_string(),
        family: "jeryu".to_string(),
        family_label: "jeryu".to_string(),
        title: format!("Title of {id}"),
        reworks: Vec::new(),
        body: String::new(),
        repos: vec!["jeryu-deploy".to_string()],
        mode: "now".to_string(),
        priority: 2,
        blocked_by: Vec::new(),
        blocked_on: Vec::new(),
        status: TodoStatus::parse(status).expect("known status"),
        attempts: 0,
        requested_by: "operator".to_string(),
        filed_at: "2026-09-19T12:00:00Z".to_string(),
        claim_by: String::new(),
        lease_until: String::new(),
        lease_live: false,
        shift: String::new(),
        change_set: String::new(),
        commits: BTreeMap::new(),
        landing: Vec::new(),
        merged: false,
        released: None,
        pr: None,
        prs: Vec::new(),
        cost_usd: None,
        triage_cost_usd: None,
        claimed_at: None,
        claim_model: None,
        note: String::new(),
        park_until: String::new(),
        assignee: None,
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

fn reviewer_thresholds() -> ReviewerOfflineThresholds {
    ReviewerOfflineThresholds {
        heartbeat_minutes: 10,
        pass_minutes: 15,
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
    stuck.claim_by = "operator@node-0/w1".to_string();
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
    assert!(items[3].reason.contains("operator@node-0/w1"));
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
        author: "rel-bot".to_string(),
        head_sha: "b761244b76371995527bfe7795e98492703553a8".to_string(),
        head_ref: format!("work/pr-{number}"),
        base_ref: "main".to_string(),
        updated_at: now() - Duration::minutes(minutes_old),
        posture,
        changed_paths: vec!["crates/jeryu-api/src/web/pulls.rs".to_string()],
        landed: None,
    }
}

/// A pull request whose change is already on its base gets one row, the one
/// that says to close it. Gate-definition changes still require human review.
#[test]
fn a_pull_request_already_on_its_base_asks_only_to_be_closed() {
    let ready = PullPosture {
        checks_green: true,
        can_merge: true,
        ..PullPosture::default()
    };
    let red = PullPosture {
        failing: vec!["ci/fast".to_string()],
        ..PullPosture::default()
    };
    let pulls = [
        PullFacts {
            landed: Some(Landed::Ancestor),
            changed_paths: vec!["ops/ci/pr-ci.sh".to_string()],
            ..pull(4, 600, ready)
        },
        PullFacts {
            landed: Some(Landed::Content),
            ..pull(5, 600, red)
        },
    ];
    let items = pull_items(&pulls, &[], now(), &hosts());
    assert_eq!(
        kinds(&items),
        [
            "pr_changes_own_gate",
            "pr_already_landed",
            "pr_already_landed"
        ]
    );
    let ancestor = &items[1];
    assert_eq!(ancestor.id, "pr-already-landed:jeryu/jeryu-web:4");
    assert_eq!(ancestor.severity, Severity::Action);
    assert_eq!(ancestor.title, "jeryu/jeryu-web#4 is already on main");
    assert!(
        ancestor
            .reason
            .contains("its head commit b761244 is already in the history of main"),
        "{}",
        ancestor.reason
    );
    assert!(
        items[2].reason.contains("squash, a cherry-pick"),
        "{}",
        items[2].reason
    );
    assert_eq!(items[2].href, "/repos/jeryu/jeryu/jeryu-web/pulls/5");
    // The row carries the close itself: closing a pull request whose change
    // has shipped is one step, and the route comments the landing commit
    // before it closes anything.
    assert_eq!(
        api(ancestor),
        Some((
            "POST",
            "/api/v1/repos/jeryu/jeryu-web/pulls/4/close",
            Value::Null
        ))
    );
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
        // Ready for less than half an hour, so it is still its own row.
        pull(
            8,
            15,
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
    let items = pull_items(&pulls, &[], now(), &hosts());
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
    // A required check that failed is re-run by gating the same head again.
    assert_eq!(
        api(&items[1]),
        Some((
            "POST",
            "/api/v1/repos/jeryu/jeryu-web/pulls/2/regate",
            Value::Null
        ))
    );
    // A check the base does not require is not the gate's context, so
    // re-gating would not re-run it: items[4] is that posture.
    assert_eq!(api(&items[4]), None);
}

/// The gate runner's check run has no details_url and its log is in a
/// mode-0700 directory on the gate host, so "open the failing check" sent a
/// reader nowhere. When the runner published a cause, the row says the cause,
/// the attempt and where that attempt's log is.
#[test]
fn a_failing_gate_row_says_what_the_runner_published() {
    let attempt = "5f1d1b3e-2a0c-4f5e-9f2a-7c1d0b8e4a61";
    let pulls = [pull(
        2,
        5,
        PullPosture {
            failing: vec!["jeryu-web/required".to_string()],
            gate_failure: Some(GateFailure {
                title: "Operator gate failure (exit 101): step failed (exit 1): ops/ci/check.sh"
                    .to_string(),
                attempt: Some(attempt.to_string()),
            }),
            ..PullPosture::default()
        },
    )];
    let items = pull_items(&pulls, &[], now(), &hosts());
    assert_eq!(kinds(&items), ["pr_checks_failing"]);
    let item = &items[0];
    assert!(
        item.reason.contains(
            "The `pr-gate-runner` check run on this head says: Operator gate failure \
             (exit 101): step failed (exit 1): ops/ci/check.sh."
        ),
        "{}",
        item.reason
    );
    assert!(
        item.reason.contains(&format!(
            "Attempt {attempt}: its log is \
             `~/gate-runner/attempts/jeryu-jeryu-web-{}/{attempt}/build.log` on gate-box",
            pulls[0].head_sha
        )),
        "{}",
        item.reason
    );
    assert!(
        !item.reason.contains("checks panel"),
        "the panel links nothing for this check: {}",
        item.reason
    );
    assert_eq!(
        item.action.label,
        "Fix step failed (exit 1): ops/ci/check.sh, then re-run the gate"
    );
    assert!(
        item.next_step
            .starts_with("Fix step failed (exit 1): ops/ci/check.sh, then re-run the gate: open "),
        "{}",
        item.next_step
    );

    // A failure the runner published no cause for keeps the panel as the way
    // in: there is nothing truer to say.
    let quiet = [pull(
        3,
        5,
        PullPosture {
            failing: vec!["jeryu-web/required".to_string()],
            ..PullPosture::default()
        },
    )];
    let items = pull_items(&quiet, &[], now(), &hosts());
    assert!(items[0].reason.contains("checks panel"), "{:?}", items[0]);
    assert_eq!(
        items[0].action.label,
        "Open the failing check and fix or re-run it"
    );
}

/// The gate that runs on every push to main reports through `main.status`
/// events, so the attention row's facts come from the open red interval of
/// the main-health projection: who turned it red, and what failed.
#[test]
fn a_red_main_from_the_push_gate_names_the_change_and_the_failing_tests() {
    let open = crate::web::pipeline::main_health::RedInterval {
        sha: "bbb2222".to_string(),
        since: "2026-09-19T11:00:00Z".to_string(),
        until: None,
        seconds: 600,
        fixed_by: None,
        culprit_pr: Some(7),
        flake: false,
        tests: vec!["suite::slow_case".to_string()],
    };
    let facts = super::attention::gate_failure("acme/widget", &open);
    assert_eq!(facts.first_red, open.since);
    assert!(facts.first_step.contains("acme/widget#7"), "{facts:?}");
    assert!(facts.first_step.contains("suite::slow_case"), "{facts:?}");

    let quiet = crate::web::pipeline::main_health::RedInterval {
        culprit_pr: None,
        tests: Vec::new(),
        ..open
    };
    let facts = super::attention::gate_failure("acme/widget", &quiet);
    assert_eq!(facts.first_step, "the required gate failed on main");
}

#[test]
fn main_red_is_its_own_attention_row_and_annotates_pr_failures() {
    let main = MainRedFacts {
        repo: "jeryu/jeryu-web".to_string(),
        sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        since: "2026-09-19T11:00:00Z".to_string(),
        first_step: "step failed (exit 1): ops/ci/check.sh".to_string(),
        log_id: "5f1d1b3e-2a0c-4f5e-9f2a-7c1d0b8e4a61".to_string(),
    };
    let rows = main_red_items(std::slice::from_ref(&main));
    assert_eq!(kinds(&rows), ["main_red"]);
    assert_eq!(rows[0].severity, Severity::Critical);
    assert_eq!(rows[0].repo.as_deref(), Some("jeryu/jeryu-web"));
    assert_eq!(rows[0].sha.as_deref(), Some(main.sha.as_str()));
    assert_eq!(rows[0].since.as_deref(), Some(main.since.as_str()));
    assert_eq!(rows[0].action.label, "Fix main before merging PRs into it");
    assert!(rows[0].reason.contains(&main.first_step), "{:?}", rows[0]);
    assert!(rows[0].reason.contains(&main.log_id), "{:?}", rows[0]);

    let pulls = [pull(
        2,
        5,
        PullPosture {
            failing: vec!["jeryu-web/required".to_string()],
            main_failure: Some(GateFailure {
                title: main.first_step,
                attempt: Some(main.log_id),
            }),
            ..PullPosture::default()
        },
    )];
    let items = pull_items(&pulls, &[], now(), &hosts());
    assert_eq!(kinds(&items), ["pr_checks_failing"]);
    assert!(
        items[0].reason.contains("current main is red too"),
        "{}",
        items[0].reason
    );
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

/// The gate runs the recipe at the pull request's own head, so a diff that
/// touches the recipe, the CI scripts it calls or the jankurai and policy files
/// those read is a change judging itself, and waits on a person whatever the
/// merge posture says. Ordinary work -- tests included -- is not flagged.
#[test]
fn a_pull_request_that_changes_its_own_gate_waits_on_a_person() {
    assert!(gate_definition_paths(&[]).is_empty());
    let ordinary = [
        "crates/jeryu-api/src/web/pulls.rs",
        "crates/jeryu-api/src/web/pulls/tests.rs",
        "tests/sandbox_escape_matrix.sh",
        "docs/jankurai-gate.md",
        "README.md",
    ]
    .map(str::to_string);
    assert!(gate_definition_paths(&ordinary).is_empty());
    let gate = [
        "crates/thing/tests/it.rs",
        "policies/trust-tiers.toml",
        "ops/ci/pr-ci.sh",
        "Justfile",
        "agent/jankurai-gate.toml",
        "ops/jankurai/report.toml",
    ]
    .map(str::to_string);
    assert_eq!(
        gate_definition_paths(&gate),
        [
            "Justfile",
            "agent/jankurai-gate.toml",
            "ops/ci/pr-ci.sh",
            "ops/jankurai/report.toml",
            "policies/trust-tiers.toml",
        ]
    );

    // Green and mergeable: exactly the case worth a person's eyes, so the
    // ready row and this one are both raised.
    let green = PullPosture {
        checks_green: true,
        can_merge: true,
        ..PullPosture::default()
    };
    let items = pull_items(
        &[PullFacts {
            changed_paths: vec!["ops/ci/pr-ci.sh".to_string()],
            ..pull(7, 15, green)
        }],
        &[],
        now(),
        &hosts(),
    );
    assert_eq!(kinds(&items), ["pr_changes_own_gate", "pr_ready_to_merge"]);
    let item = &items[0];
    assert_eq!(item.severity, Severity::Action);
    assert_eq!(item.pr, Some(7));
    assert_eq!(item.id, "pr-changes-own-gate:jeryu/jeryu-web:7");
    assert_eq!(item.href, "/repos/jeryu/jeryu/jeryu-web/pulls/7");
    assert_eq!(api(item), None);
    assert!(
        item.reason.contains("changes ops/ci/pr-ci.sh"),
        "{}",
        item.reason
    );
    assert!(
        item.reason.contains("rewrote the check that reports on it"),
        "{}",
        item.reason
    );
    assert!(
        item.next_step
            .starts_with("Read the gate change against the base branch")
    );

    // A diff that rewrites the whole gate names the first few and counts the
    // rest, so the reason stays one sentence.
    let many: Vec<String> = (0..9)
        .map(|step| format!("ops/ci/step-{step}.sh"))
        .collect();
    let items = pull_items(
        &[PullFacts {
            changed_paths: many,
            ..pull(9, 45, PullPosture::default())
        }],
        &[],
        now(),
        &hosts(),
    );
    assert_eq!(kinds(&items), ["pr_changes_own_gate"]);
    assert!(
        items[0].reason.contains("ops/ci/step-5.sh and 3 more"),
        "{}",
        items[0].reason
    );
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
        enqueued_by: "merge-bot".to_string(),
        approvers: Vec::new(),
        attempts: Vec::new(),
        reason: Some("the gate failed on abc and def".to_string()),
        refusal_code: None,
        conflict_paths: Vec::new(),
        landed_sha: None,
        after: None,
        landed_with: None,
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
        kinds(&pull_items(&pulls, &[], now(), &hosts())),
        ["pr_ready_to_merge"]
    );

    // Building, and still inside the window a queue gate normally takes.
    assert!(pull_items(&pulls, &[queued(7, 15)], now(), &hosts()).is_empty());

    // Another pull request's entry says nothing about this one.
    assert_eq!(
        kinds(&pull_items(&pulls, &[queued(8, 15)], now(), &hosts())),
        ["pr_ready_to_merge"]
    );

    // Building far longer than a gate takes: worth a look, waiting on nobody.
    let items = pull_items(&pulls, &[queued(7, 95)], now(), &hosts());
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
        kinds(&pull_items(&blocked, &[queued(7, 15)], now(), &hosts())),
        ["pr_changes_requested"]
    );
}

/// Several pull requests ready to merge for half an hour is the reviewer
/// having stopped merging, which one critical row says and twenty-six
/// ordinary action rows hide.
#[test]
fn pull_requests_nobody_merged_for_half_an_hour_are_one_stalled_merger() {
    let ready = |number: u64, minutes_old: i64| {
        pull(
            number,
            minutes_old,
            PullPosture {
                can_merge: true,
                checks_green: true,
                approvals: 1,
                required_approvals: 1,
                ..PullPosture::default()
            },
        )
    };

    let items = pull_items(
        &[ready(7, 45), ready(8, 45), ready(9, 32)],
        &[],
        now(),
        &hosts(),
    );
    assert_eq!(kinds(&items), ["merger_stalled"]);
    assert_eq!(items[0].severity, Severity::Critical);
    assert_eq!(items[0].id, "merger-stalled");
    assert!(
        items[0].title.contains("3 pull requests"),
        "{}",
        items[0].title
    );
    assert!(
        items[0].title.contains("up to 45 minutes"),
        "{}",
        items[0].title
    );
    // The count, the oldest age, every folded pull request and the one
    // command that says what the reviewer last did.
    let reason = &items[0].reason;
    assert!(reason.contains("3 pull requests have passed"), "{reason}");
    assert!(reason.contains("ready for 45 minutes"), "{reason}");
    assert!(
        reason.contains("Waiting: jeryu/jeryu-web#7, jeryu/jeryu-web#8, jeryu/jeryu-web#9."),
        "{reason}"
    );
    assert!(
        reason.contains("journalctl --user -u pr-redteam | grep \"waiting to merge\""),
        "{reason}"
    );
    assert_eq!(items[0].href, "/repos");
    assert_eq!(api(&items[0]), None);

    // One ready pull request is one owner's decision, whatever its age.
    let one = pull_items(&[ready(7, 45)], &[], now(), &hosts());
    assert_eq!(kinds(&one), ["pr_ready_to_merge"]);
    assert_eq!(one[0].severity, Severity::Action);

    // Ready for ten minutes: the window the merger is still inside.
    let fresh = pull_items(&[ready(7, 10), ready(8, 10)], &[], now(), &hosts());
    assert_eq!(kinds(&fresh), ["pr_ready_to_merge", "pr_ready_to_merge"]);

    // A long-ready pair folds; one ready for ten minutes keeps its own row.
    let mixed = pull_items(
        &[ready(7, 45), ready(8, 45), ready(9, 10)],
        &[],
        now(),
        &hosts(),
    );
    assert_eq!(kinds(&mixed), ["pr_ready_to_merge", "merger_stalled"]);
    assert_eq!(mixed[0].pr, Some(9));
}

/// A campaign is one change pushed to one branch in many repositories, and
/// the same review on each of its pull requests is one decision: one row names
/// the branch, the count and the repositories and links every pull request.
#[test]
fn pull_requests_on_one_branch_with_one_verdict_are_one_row() {
    let blocked = |repo: &str, number: u64| PullFacts {
        repo: repo.to_string(),
        head_ref: "dev/jankurai-v3-receipts".to_string(),
        ..pull(
            number,
            20,
            PullPosture {
                changes_requested: 1,
                ..PullPosture::default()
            },
        )
    };

    let items = pull_items(
        &[
            blocked("jeryu/jeryu-web", 41),
            blocked("jeryu/jeryu-core", 42),
            blocked("jeryu/jeryu-api", 43),
            blocked("jeryu/jeryu-tool", 44),
        ],
        &[],
        now(),
        &hosts(),
    );
    assert_eq!(kinds(&items), ["pr_branch_campaign"]);
    let row = &items[0];
    assert_eq!(row.severity, Severity::Action);
    assert_eq!(
        row.id,
        "pr-branch-campaign:pr-changes-requested:dev/jankurai-v3-receipts"
    );
    assert!(
        row.title
            .contains("4 pull requests on dev/jankurai-v3-receipts"),
        "{}",
        row.title
    );
    // The branch, the count, the number of repositories and every pull
    // request, so the row says what it stands for.
    let reason = &row.reason;
    assert!(reason.contains("4 open pull requests"), "{reason}");
    assert!(reason.contains("across 4 repositories"), "{reason}");
    assert!(
        reason.contains("the branch dev/jankurai-v3-receipts"),
        "{reason}"
    );
    assert!(
        reason.contains(
            "jeryu/jeryu-api#43, jeryu/jeryu-core#42, jeryu/jeryu-tool#44, jeryu/jeryu-web#41"
        ),
        "{reason}"
    );
    // Each pull request is linked, so a reader opens them without the prose.
    let links = row.pulls.as_ref().expect("the pull requests it stands for");
    assert_eq!(
        links
            .iter()
            .map(|link| link.href.as_str())
            .collect::<Vec<_>>(),
        [
            "/repos/jeryu/jeryu/jeryu-api/pulls/43",
            "/repos/jeryu/jeryu/jeryu-core/pulls/42",
            "/repos/jeryu/jeryu/jeryu-tool/pulls/44",
            "/repos/jeryu/jeryu/jeryu-web/pulls/41",
        ]
    );
    assert_eq!(row.href, "/search?q=dev/jankurai-v3-receipts");
    assert_eq!(api(row), None);
    // One step, the one every folded row asked for.
    assert_eq!(
        row.action.label,
        "Read the review and push a fix, or dismiss it"
    );

    // Two pull requests on the branch are two rows: nothing to collapse.
    let two = pull_items(
        &[
            blocked("jeryu/jeryu-web", 41),
            blocked("jeryu/jeryu-core", 42),
        ],
        &[],
        now(),
        &hosts(),
    );
    assert_eq!(
        kinds(&two),
        ["pr_changes_requested", "pr_changes_requested"]
    );
    assert_eq!(two[0].pr, Some(41));

    // Four pull requests on one branch with different verdicts are not one
    // decision: only the three sharing a verdict fold.
    let mut mixed = vec![
        blocked("jeryu/jeryu-web", 41),
        blocked("jeryu/jeryu-core", 42),
        blocked("jeryu/jeryu-api", 43),
    ];
    mixed.push(PullFacts {
        posture: PullPosture {
            checks_green: true,
            required_approvals: 1,
            ..PullPosture::default()
        },
        ..blocked("jeryu/jeryu-tool", 44)
    });
    assert_eq!(
        kinds(&pull_items(&mixed, &[], now(), &hosts())),
        ["pr_awaiting_approval", "pr_branch_campaign"]
    );

    // The same verdict on three different branches stays three rows.
    let spread = [
        pull(
            41,
            20,
            PullPosture {
                changes_requested: 1,
                ..PullPosture::default()
            },
        ),
        pull(
            42,
            20,
            PullPosture {
                changes_requested: 1,
                ..PullPosture::default()
            },
        ),
        pull(
            43,
            20,
            PullPosture {
                changes_requested: 1,
                ..PullPosture::default()
            },
        ),
    ];
    assert_eq!(
        kinds(&pull_items(&spread, &[], now(), &hosts())).len(),
        3,
        "three branches are three decisions"
    );
}

/// A pull request the reviewer gave up on carries the changes it requested:
/// `reviewer_stuck` says both, so the posture's row is dropped (live:
/// veox/jain-domain#5 showed up as two rows).
#[test]
fn a_reviewer_that_requested_changes_and_gave_up_is_one_row() {
    let repo = "jeryu/jeryu-web".to_string();
    let open: BTreeSet<(String, u64)> = [(repo.clone(), 35)].into();
    let mut items = pull_items(
        &[PullFacts {
            number: 35,
            ..pull(
                35,
                45,
                PullPosture {
                    changes_requested: 1,
                    ..PullPosture::default()
                },
            )
        }],
        &[],
        now(),
        &hosts(),
    );
    items.extend(runner_items(
        &[
            runner("xbabe0/pr-redteam", &["redteam"], 30, Some(("hold", 35))),
            // A live gate slot, so the inbox has nothing else to say.
            runner("xbabe2/slot0", &["pr-gate"], 30, None),
        ],
        &heads_at(&open, TEST_HEAD),
        0,
        false,
        reviewer_thresholds(),
        now(),
        &hosts(),
    ));
    assert_eq!(kinds(&items), ["pr_changes_requested", "reviewer_stuck"]);

    drop_shadowed_pull_rows(&mut items);
    assert_eq!(kinds(&items), ["reviewer_stuck"]);
    assert_eq!(items[0].pr, Some(35));

    // Changes requested on another pull request stands on its own.
    let mut elsewhere = pull_items(
        &[pull(
            36,
            45,
            PullPosture {
                changes_requested: 1,
                ..PullPosture::default()
            },
        )],
        &[],
        now(),
        &hosts(),
    );
    elsewhere.extend(runner_items(
        &[
            runner("xbabe0/pr-redteam", &["redteam"], 30, Some(("hold", 35))),
            // A live gate slot, so the inbox has nothing else to say.
            runner("xbabe2/slot0", &["pr-gate"], 30, None),
        ],
        &heads_at(&open, TEST_HEAD),
        0,
        false,
        reviewer_thresholds(),
        now(),
        &hosts(),
    ));
    drop_shadowed_pull_rows(&mut elsewhere);
    assert_eq!(
        kinds(&elsewhere),
        ["pr_changes_requested", "reviewer_stuck"]
    );
}

#[test]
fn a_held_web_pin_bump_replaces_the_generic_changes_requested_row() {
    let repo = "acme/globex-deploy".to_string();
    let mut items = pull_items(
        &[PullFacts {
            repo: repo.clone(),
            number: 243,
            title: "release: pin jeryu-web 427bebe".to_string(),
            author: "rel-bot".to_string(),
            head_sha: "b761244b76371995527bfe7795e98492703553a8".to_string(),
            head_ref: "auto/pin-web-427bebe".to_string(),
            base_ref: "main".to_string(),
            landed: None,
            updated_at: now() - Duration::minutes(45),
            posture: PullPosture {
                changes_requested: 1,
                ..PullPosture::default()
            },
            changed_paths: vec!["globex-split.lock.toml".to_string()],
        }],
        &[],
        now(),
        &hosts(),
    );
    let mut pin = Pin {
        dependency: "acme/jeryu-web".to_string(),
        kind: "commit",
        source: "globex-split.lock.toml".to_string(),
        pinned_ref: "8afe03c49bbdf1ad26d1282095561b50c840bf0e".to_string(),
        pinned_sha: Some("8afe03c49bbdf1ad26d1282095561b50c840bf0e".to_string()),
        latest_sha: Some("427bebecb848d7b7bb37ecc71521d7461072694d".to_string()),
        latest_at: Some("2026-09-19T12:00:00Z".to_string()),
        behind: 7,
        latest_green: Some(true),
        state: "behind",
        bump_pr: Some(BumpPr {
            number: 243,
            state: "open".to_string(),
            url: "/repos/jeryu/acme/globex-deploy/pulls/243".to_string(),
            title: "release: pin jeryu-web 427bebe".to_string(),
            head_ref: "auto/pin-web-427bebe".to_string(),
            head_sha: "b761244b76371995527bfe7795e98492703553a8".to_string(),
            created_at: "2026-09-19T12:30:00Z".to_string(),
            changes_requested: true,
            blocking_review: Some("The bundle checksum does not match the lock.".to_string()),
        }),
        unreleased: vec![Unreleased {
            sha: "427bebecb848d7b7bb37ecc71521d7461072694d".to_string(),
            subject: "ship the calmer inbox".to_string(),
        }],
    };
    items.extend(pin_items(
        &[Consumer {
            repo: repo.clone(),
            family: Some("acme".to_string()),
            branch: "main".to_string(),
            pins: vec![pin.clone()],
        }],
        &[],
        &hosts(),
        now(),
    ));
    assert_eq!(kinds(&items), ["pr_changes_requested", "pin_behind"]);

    drop_shadowed_pull_rows(&mut items);
    assert_eq!(kinds(&items), ["pin_behind"]);
    assert_eq!(items[0].repo.as_deref(), Some(repo.as_str()));
    assert_eq!(items[0].pr, Some(243));

    pin.bump_pr.as_mut().unwrap().changes_requested = false;
    let mut unheld = pull_items(
        &[PullFacts {
            repo: repo.clone(),
            number: 243,
            title: "release: pin jeryu-web 427bebe".to_string(),
            author: "rel-bot".to_string(),
            head_sha: "b761244b76371995527bfe7795e98492703553a8".to_string(),
            head_ref: "auto/pin-web-427bebe".to_string(),
            base_ref: "main".to_string(),
            landed: None,
            updated_at: now() - Duration::minutes(45),
            posture: PullPosture {
                changes_requested: 1,
                ..PullPosture::default()
            },
            changed_paths: Vec::new(),
        }],
        &[],
        now(),
        &hosts(),
    );
    unheld.extend(pin_items(
        &[Consumer {
            repo,
            family: Some("acme".to_string()),
            branch: "main".to_string(),
            pins: vec![pin],
        }],
        &[],
        &hosts(),
        now(),
    ));
    drop_shadowed_pull_rows(&mut unheld);
    assert_eq!(
        kinds(&unheld),
        ["pr_changes_requested", "pin_behind"],
        "only the dedicated held web-pin row shadows the generic review row"
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
            host: "node-2".to_string(),
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
                kind: None,
                exit_code: None,
                finished_at: now() - Duration::minutes(3),
            }),
            code: None,
            tools: Vec::new(),
        },
        reporter: "ci-bot".to_string(),
        received_at: now() - Duration::seconds(seconds_ago),
    }
}

/// The head the `pull` and `runner` helpers both put their pull request at.
const TEST_HEAD: &str = "b761244b76371995527bfe7795e98492703553a8";

/// The head a pull request moved to when its author pushed the fix.
const NEW_HEAD: &str = "edd08c5a1f2e4b6c8d0a2b4c6d8e0f1a2b3c4d5e";

/// A reviewer that holds `pr` at `TEST_HEAD` and is reviewing it again at
/// `head` right now.
fn rereviewing(id: &str, pr: u64, head: &str) -> GateRunnerRecord {
    let mut record = runner(id, &["redteam"], 30, Some(("hold", pr)));
    record.heartbeat.current = Some(GateRunnerTask {
        repo: "jeryu/jeryu-web".to_string(),
        pr: Some(pr),
        sha: head.to_string(),
        recipe: "review".to_string(),
        target: None,
        kind: Some("pr".to_string()),
        started_at: now() - Duration::minutes(2),
    });
    record
}

/// Every pull request of `open`, at `sha`: what `runner_items` reads to tell
/// a verdict on the current head from one on a head that has been replaced.
fn heads_at(open: &BTreeSet<(String, u64)>, sha: &str) -> BTreeMap<(String, u64), String> {
    open.iter()
        .map(|key| (key.clone(), sha.to_string()))
        .collect()
}

#[test]
fn a_reviewer_without_a_verdict_and_a_gate_with_no_runner() {
    let open: BTreeSet<(String, u64)> = [("jeryu/jeryu-web".to_string(), 35)].into();
    let stuck = runner(
        "node-0/pr-redteam",
        &["redteam"],
        30,
        Some(("too_large", 35)),
    );
    let approved = runner(
        "node-0/pr-redteam-2",
        &["redteam"],
        30,
        Some(("approve", 35)),
    );
    let closed_pr = runner("node-0/pr-redteam-3", &["redteam"], 30, Some(("hold", 99)));
    let gate = runner("node-2/slot0", &["pr-gate"], 30, None);
    let stale_gate = runner("node-2/slot1", &["pr-gate"], 600, None);

    let healthy = runner_items(
        &[stuck.clone(), approved, closed_pr, gate, stale_gate.clone()],
        &heads_at(&open, TEST_HEAD),
        0,
        false,
        reviewer_thresholds(),
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
    let down = runner_items(
        &[stuck, stale_gate.clone()],
        &heads_at(&open, TEST_HEAD),
        0,
        false,
        reviewer_thresholds(),
        now(),
        &hosts(),
    );
    assert_eq!(kinds(&down), ["reviewer_stuck", "gate_runner_down"]);
    assert_eq!(down[1].severity, Severity::Critical);
    assert!(down[1].action.command.is_some());
    assert_eq!(
        down[1].action.run_in.as_deref(),
        Some("gate-box, any directory")
    );
    // No open PR and nothing queued: a silent gate is nobody's problem yet.
    let only_stale = std::slice::from_ref(&stale_gate);
    assert!(
        runner_items(
            only_stale,
            &BTreeMap::new(),
            0,
            false,
            reviewer_thresholds(),
            now(),
            &hosts()
        )
        .is_empty()
    );
    assert_eq!(
        kinds(&runner_items(
            only_stale,
            &BTreeMap::new(),
            0,
            true,
            reviewer_thresholds(),
            now(),
            &hosts()
        )),
        ["gate_runner_down"],
        "the merge queue waiting on a gate counts"
    );
}

/// One gate host hanging while another beats leaves its share ungated.
#[test]
fn a_silent_gate_host_is_raised_while_another_is_alive() {
    let open = BTreeMap::from([(("veox/app".to_string(), 7u64), TEST_HEAD.to_string())]);
    let on = |host: &str, id: &str, ago: i64| {
        let mut r = runner(id, &["pr-gate"], ago, None);
        r.heartbeat.host = host.to_string();
        r
    };
    let run = |records: &[GateRunnerRecord], open: &BTreeMap<(String, u64), String>| {
        runner_items(
            records,
            open,
            0,
            false,
            reviewer_thresholds(),
            now(),
            &hosts(),
        )
    };
    let both = [
        on("host-a", "host-a/slot0", 30),
        on("host-b", "host-b/slot0", 600),
    ];
    let rows = run(&both, &open);
    assert_eq!(kinds(&rows), ["gate_runner_down"]);
    assert_eq!(rows[0].severity, Severity::Critical);
    assert!(rows[0].title.contains("host-b"));
    assert!(rows[0].reason.contains("host-a"));
    assert!(rows[0].reason.contains("1 open pull request"));
    assert!(rows[0].action.command.is_some());
    assert_eq!(
        rows[0].action.run_in.as_deref(),
        Some("host-a, any directory")
    );
    assert!(rows[0].since.is_some());
    // One live slot keeps a host alive; nothing open means nothing is ungated.
    let mixed = [
        on("host-a", "host-a/slot0", 30),
        on("host-b", "host-b/slot0", 600),
        on("host-b", "host-b/slot1", 20),
    ];
    assert!(run(&mixed, &open).is_empty());
    assert!(run(&both, &BTreeMap::new()).is_empty());
    // Every host silent keeps the single all-silent row.
    let none = [
        on("host-a", "host-a/slot0", 600),
        on("host-b", "host-b/slot0", 700),
    ];
    let rows = run(&none, &open);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "No PR gate runner is reporting");
}

/// A hold is about the head it was taken on. Once the author pushes the fix
/// the held head is gone, so the hold asks nothing of anybody: the action
/// item goes, and while the reviewer is reading the new head the inbox says
/// that instead (live: jeryu/jeryu-ci-runner#109, held at 91beebd and fixed
/// by edd08c5, still read as stuck 39 minutes after the reviewer had moved).
#[test]
fn a_hold_on_a_head_that_has_been_replaced_is_not_a_stuck_reviewer() {
    let repo = "jeryu/jeryu-web".to_string();
    let open: BTreeSet<(String, u64)> = [(repo.clone(), 35)].into();
    let gate = runner("node-2/slot0", &["pr-gate"], 30, None);

    // Head B, the reviewer holding head A and already reading head B.
    let moved = runner_items(
        &[rereviewing("node-0/pr-redteam", 35, NEW_HEAD), gate.clone()],
        &heads_at(&open, NEW_HEAD),
        0,
        false,
        reviewer_thresholds(),
        now(),
        &hosts(),
    );
    assert_eq!(kinds(&moved), ["reviewer_rereviewing"]);
    let row = &moved[0];
    assert_eq!(row.severity, Severity::Watch);
    assert_eq!(row.repo.as_deref(), Some(repo.as_str()));
    assert_eq!(row.pr, Some(35));
    assert_eq!(row.sha.as_deref(), Some(NEW_HEAD));
    assert_eq!(row.since.as_deref(), Some("2026-09-19T12:58:00+00:00"));
    assert_eq!(
        row.title,
        format!(
            "Re-reviewing {} since 2026-09-19T12:58:00+00:00 (previous hold at {} is stale)",
            &NEW_HEAD[..7],
            &TEST_HEAD[..7]
        )
    );
    assert!(
        row.reason.contains("asks nothing of anybody"),
        "{}",
        row.reason
    );

    // Head B, and no reviewer is reading it: the stale hold is simply dropped.
    let quiet = runner_items(
        &[
            runner("node-0/pr-redteam", &["redteam"], 30, Some(("hold", 35))),
            gate.clone(),
        ],
        &heads_at(&open, NEW_HEAD),
        0,
        false,
        reviewer_thresholds(),
        now(),
        &hosts(),
    );
    assert!(quiet.is_empty(), "{quiet:?}");

    // The hold is on the head the pull request is at now: still an action item.
    let current = runner_items(
        &[
            runner("node-0/pr-redteam", &["redteam"], 30, Some(("hold", 35))),
            gate.clone(),
        ],
        &heads_at(&open, TEST_HEAD),
        0,
        false,
        reviewer_thresholds(),
        now(),
        &hosts(),
    );
    assert_eq!(kinds(&current), ["reviewer_stuck"]);
    assert_eq!(current[0].severity, Severity::Action);
    assert_eq!(current[0].sha.as_deref(), Some(TEST_HEAD));

    // The pull request is closed: the inbox has nothing to say either way.
    let closed = runner_items(
        &[rereviewing("node-0/pr-redteam", 35, NEW_HEAD), gate],
        &BTreeMap::new(),
        0,
        false,
        reviewer_thresholds(),
        now(),
        &hosts(),
    );
    assert!(closed.is_empty(), "{closed:?}");
}

/// A `CHANGES_REQUESTED` the author has already answered with a push is not
/// an action item either: the posture counts only the reviews in force on the
/// pull request's current head, so the row the inbox raises follows the head.
#[test]
fn changes_requested_on_a_head_that_has_been_replaced_raises_nothing() {
    let answered = pull(
        35,
        45,
        PullPosture {
            checks_green: true,
            required_approvals: 1,
            ..PullPosture::default()
        },
    );
    assert_eq!(
        kinds(&pull_items(&[answered], &[], now(), &hosts())),
        ["pr_awaiting_approval"],
        "the withdrawn verdict asks for nothing; the open approval still does"
    );

    // The same pull request with the verdict in force on the current head.
    let held = pull(
        35,
        45,
        PullPosture {
            changes_requested: 1,
            checks_green: true,
            required_approvals: 1,
            ..PullPosture::default()
        },
    );
    assert_eq!(
        kinds(&pull_items(&[held], &[], now(), &hosts())),
        ["pr_changes_requested"]
    );
}

#[test]
fn reviewer_offline_is_critical_when_reviewers_stop_reporting() {
    let open: BTreeSet<(String, u64)> = [("jeryu/jeryu-web".to_string(), 35)].into();
    let gate = runner("node-2/slot0", &["pr-gate"], 30, None);
    let stale = runner("node-0/redteam", &["redteam"], 660, Some(("approve", 35)));
    let fresh = runner("node-0/redteam", &["redteam"], 30, Some(("approve", 35)));

    let stale_items = runner_items(
        &[stale, gate.clone()],
        &heads_at(&open, TEST_HEAD),
        2,
        false,
        reviewer_thresholds(),
        now(),
        &hosts(),
    );
    assert_eq!(kinds(&stale_items), ["reviewer_offline"]);
    assert_eq!(stale_items[0].severity, Severity::Critical);
    assert_eq!(
        stale_items[0].since.as_deref(),
        Some("2026-09-19T12:49:00+00:00")
    );
    assert!(
        stale_items[0]
            .reason
            .contains("2 pull request(s) wait for review"),
        "{}",
        stale_items[0].reason
    );
    assert!(
        stale_items[0]
            .reason
            .contains("last heartbeat 2026-09-19 12:49:00 UTC"),
        "{}",
        stale_items[0].reason
    );

    let fresh_items = runner_items(
        &[fresh, gate],
        &heads_at(&open, TEST_HEAD),
        2,
        false,
        reviewer_thresholds(),
        now(),
        &hosts(),
    );
    assert!(fresh_items.is_empty(), "{fresh_items:?}");
}

#[test]
fn reviewer_offline_fires_when_review_passes_stop_completing() {
    let open: BTreeSet<(String, u64)> = [("jeryu/jeryu-web".to_string(), 35)].into();
    let gate = runner("node-2/slot0", &["pr-gate"], 30, None);
    let mut quiet = runner("node-0/redteam", &["redteam"], 30, Some(("approve", 35)));
    quiet
        .heartbeat
        .last
        .as_mut()
        .expect("review pass")
        .finished_at = now() - Duration::minutes(16);

    let items = runner_items(
        &[quiet, gate],
        &heads_at(&open, TEST_HEAD),
        1,
        false,
        reviewer_thresholds(),
        now(),
        &hosts(),
    );
    assert_eq!(kinds(&items), ["reviewer_offline"]);
    assert!(
        items[0]
            .reason
            .contains("No review pass has completed within the 15 minute site threshold"),
        "{}",
        items[0].reason
    );
}

fn worker(family: &str, slot: &str, healthy: bool) -> WorkerRow {
    WorkerRow {
        heartbeat: Heartbeat {
            operator: "operator@node-0".to_string(),
            host: "node-0".to_string(),
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
            kind: "todoq".to_string(),
            detail: None,
            capacity: None,
            active: None,
            usage: None,
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
        Some("node-0, any directory")
    );
    assert_eq!(
        items[0].next_step,
        "Check the todoq supervisor on the worker host: on node-0, any directory, run \
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
    moved.heartbeat.host = "node-3".to_string();
    moved.last_seen = "2026-09-19T12:50:00Z".to_string();
    let items = worker_items(
        &families[..1],
        &[worker("jeryu", "w1", false), moved],
        &hosts(),
    );
    assert_eq!(
        items[0].action.run_in.as_deref(),
        Some("node-3, any directory")
    );
}

/// A `shift.exhausted` event as the todoq workers post it: what the operator
/// spent, the cap, and how many claimable todos were left waiting.
fn exhausted(seq: i64, family: &str, shift: &str, ts: &str, waiting: i64) -> Event {
    Event {
        seq,
        ts: ts.to_string(),
        event_id: None,
        source: "todoq".to_string(),
        kind: "shift.exhausted".to_string(),
        reporter: "operator".to_string(),
        actor: None,
        family_label: Some(family.to_string()),
        family: Some(family.to_string()),
        repo: None,
        pr: None,
        sha: None,
        todo_id: None,
        shift: Some(shift.to_string()),
        outcome: None,
        needs_human: true,
        summary: format!("{family}: shift budget spent"),
        reason: None,
        cost_usd: None,
        seconds: None,
        log_tail: None,
        log_url: None,
        detail: Some(json!({"spent_usd": 42.0, "budget_usd": 40.0, "waiting": waiting})),
    }
}

fn claimed(seq: i64, family: &str, shift: &str, ts: &str) -> Event {
    Event {
        kind: "todo.claimed".to_string(),
        needs_human: false,
        summary: format!("{family} w1 claimed a todo"),
        detail: None,
        todo_id: Some("20261003-020938-c66a5a".to_string()),
        ..exhausted(seq, family, shift, ts, 0)
    }
}

#[test]
fn a_spent_shift_budget_with_todos_waiting_needs_a_person() {
    let items = budget_items(
        &[exhausted(
            11,
            "acme",
            "nightshift/2026-10-03",
            "2026-10-03T06:00:00Z",
            3,
        )],
        &[],
    );
    assert_eq!(kinds(&items), ["shift_budget_spent"]);
    let item = &items[0];
    assert_eq!(item.severity, Severity::Action);
    assert_eq!(item.id, "shift-budget-spent:acme:nightshift/2026-10-03");
    assert_eq!(item.family.as_deref(), Some("acme"));
    assert_eq!(item.shift.as_deref(), Some("nightshift/2026-10-03"));
    assert_eq!(item.since.as_deref(), Some("2026-10-03T06:00:00Z"));
    assert!(item.title.contains("3 todo(s) wait"), "{}", item.title);
    assert!(
        item.reason.contains("$42.00 of the $40.00 shift budget"),
        "{}",
        item.reason
    );
    let spend = item.budget.as_ref().expect("the spend in numbers");
    assert_eq!(spend.spent_usd, Some(42.0));
    assert_eq!(spend.budget_usd, Some(40.0));
    assert_eq!(spend.waiting, Some(3));
    assert_actionable(item);
}

#[test]
fn a_shift_budget_item_clears_on_a_later_claim_or_a_newer_shift() {
    let spent = exhausted(
        11,
        "acme",
        "nightshift/2026-10-03",
        "2026-10-03T06:00:00Z",
        3,
    );

    // A claim after the event: the budget was raised, or the next shift took
    // the work up, and nothing waits on a person any more.
    assert!(
        budget_items(
            std::slice::from_ref(&spent),
            &[claimed(
                12,
                "acme",
                "nightshift/2026-10-04",
                "2026-10-04T01:00:00Z"
            )]
        )
        .is_empty()
    );
    // A claim from before it, or one of another family, says nothing.
    assert_eq!(
        kinds(&budget_items(
            std::slice::from_ref(&spent),
            &[
                claimed(9, "acme", "nightshift/2026-10-03", "2026-10-03T02:00:00Z"),
                claimed(
                    13,
                    "globex",
                    "nightshift/2026-10-03",
                    "2026-10-03T07:00:00Z"
                ),
            ]
        )),
        ["shift_budget_spent"]
    );

    // A newer shift's event replaces the one before it: one item per family.
    let newer = exhausted(
        20,
        "acme",
        "nightshift/2026-10-04",
        "2026-10-04T06:00:00Z",
        1,
    );
    let items = budget_items(&[newer, spent], &[]);
    assert_eq!(kinds(&items), ["shift_budget_spent"]);
    assert_eq!(items[0].shift.as_deref(), Some("nightshift/2026-10-04"));

    // Nothing waiting is a shift that ended tidily.
    assert!(
        budget_items(
            &[exhausted(
                11,
                "acme",
                "nightshift/2026-10-03",
                "2026-10-03T06:00:00Z",
                0
            )],
            &[]
        )
        .is_empty()
    );
}

#[tokio::test]
async fn the_attention_route_reads_a_spent_shift_budget_from_the_event_log() {
    let dir = tempfile::tempdir().unwrap();
    let (router, admin) = shift_forge(dir.path());
    let call = |method, uri: &str, token: &str, body: Option<Value>| {
        router.clone().oneshot(request(method, uri, token, body))
    };
    let posted = call(
        HttpMethod::POST,
        "/api/v1/events",
        &admin,
        Some(json!({
            "source": "todoq", "kind": "shift.exhausted", "family": "jeryu",
            "shift": "nightshift/2026-10-03", "needs_human": true,
            "summary": "jeryu: shift budget spent with 3 todos waiting",
            "detail": {"spent_usd": 42.0, "budget_usd": 40.0, "waiting": 3},
        })),
    )
    .await
    .unwrap();
    assert_eq!(posted.status(), StatusCode::CREATED);

    let body = body_json(
        call(HttpMethod::GET, "/api/v1/attention", &admin, None)
            .await
            .unwrap(),
    )
    .await;
    let item = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "shift_budget_spent")
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(item["severity"], "action");
    assert_eq!(item["budget"]["spent_usd"], 42.0);
    assert_eq!(item["budget"]["budget_usd"], 40.0);
    assert_eq!(item["budget"]["waiting"], 3);
    assert_eq!(item["href"], "/work?family=jeryu");
}

fn release_event(seq: i64, kind: &str, sha: &str, ts: &str, needs_human: bool) -> Event {
    Event {
        seq,
        ts: ts.to_string(),
        event_id: None,
        source: "auto-stage".to_string(),
        kind: kind.to_string(),
        reporter: "operator".to_string(),
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

/// One event where the rule reads every repository's newest one.
fn slice(event: &Event) -> &[Event] {
    std::slice::from_ref(event)
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
    let items = release_items(&[], &[], &[facts.clone()], &hosts());
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
    let items = release_items(&[], &[], &[moved_on], &hosts());
    assert_eq!(items[0].severity, Severity::Critical);

    // Without release names in the payloads the commits decide.
    let mut by_sha = facts;
    by_sha.current_release = None;
    if let Some(latest) = by_sha.latest.as_mut() {
        latest.release = None;
        latest.sha = "01dfe680a6de5e02da4e9aa7821534742aa46d7e".to_string();
    }
    let items = release_items(&[], &[], &[by_sha], &hosts());
    assert_eq!(items[0].severity, Severity::Critical);
}

/// With a release host polling for asks, a staged release is one click: the
/// row's button asks for that exact release, and once asked the row says who
/// asked and offers no second button. Without a poller it is the command only.
#[test]
fn a_staged_release_is_a_button_while_the_release_host_polls_for_asks() {
    let next = "01dfe680a6de5e02da4e9aa7821534742aa46d7e";
    let staged = release_event(10, "release.staged", next, "2026-09-19T13:03:26Z", false);
    let release = "prod-20260919T130210Z-01dfe68-unsigned";

    let no_poller = release_items(slice(&staged), &[], &[], &hosts());
    assert_eq!(no_poller[0].action.api, None);
    assert!(no_poller[0].action.command.is_some());

    let mut polled = hosts();
    polled
        .release_queue
        .poller_repos
        .push("jeryu/jeryu-deploy".into());
    let items = release_items(slice(&staged), &[], &[], &polled);
    let api = items[0].action.api.as_ref().expect("a deploy button");
    assert_eq!(api.path, "/api/v1/release-actions");
    assert_eq!(
        api.body,
        Some(json!({"action": "deploy", "repo": "jeryu/jeryu-deploy", "release": release}))
    );
    assert!(api.confirm.as_deref().is_some_and(|c| c.contains(release)));
    // The copyable command stays beside it, for a release host without a poller.
    assert!(items[0].action.command.is_some());

    polled
        .release_queue
        .open
        .push(crate::web::release_actions::ReleaseAction {
            id: "ra-1".into(),
            action: crate::web::release_actions::ActionKind::Deploy,
            repo: "jeryu/jeryu-deploy".into(),
            release: Some(release.into()),
            head: None,
            state: crate::web::release_actions::ActionState::Pending,
            requested_by: "operator".into(),
            requested_at: "2026-09-19T13:10:00Z".into(),
            claimed_by: None,
            claimed_at: None,
            finished_at: None,
            message: None,
            log_tail: None,
        });
    let asked = release_items(slice(&staged), &[], &[], &polled);
    assert_eq!(asked[0].action.api, None);
    assert_eq!(asked[0].action.label, "Deploy requested");
    assert!(
        asked[0].reason.contains("Asked by operator at 13:10Z"),
        "{}",
        asked[0].reason
    );
}

/// A release staged in one repository must not hide one staged in another:
/// each repository's newest staging brings its own item.
#[test]
fn every_repository_with_a_staged_release_gets_its_own_item() {
    let next = "01dfe680a6de5e02da4e9aa7821534742aa46d7e";
    let staged_in = |seq: i64, repo: &str, release: &str| {
        let mut event = release_event(seq, "release.staged", next, "2026-09-19T13:03:26Z", false);
        event.repo = Some(repo.to_string());
        event.detail = Some(json!({"release": release}));
        event
    };
    // What the store hands the rule: the newest staging of each of two
    // repositories of one family.
    let newest: Vec<Event> = vec![
        staged_in(10, "acme/acme-web", "web-20260919T130210Z"),
        staged_in(11, "acme/acme-api", "api-20260919T130500Z"),
    ];

    let items = release_items(&newest, &[], &[], &hosts());
    assert_eq!(kinds(&items), ["release_staged", "release_staged"]);
    assert_eq!(
        items
            .iter()
            .map(|item| (item.repo.as_deref().unwrap_or("-"), item.id.as_str()))
            .collect::<Vec<_>>(),
        [
            (
                "acme/acme-web",
                "release-staged:acme/acme-web:web-20260919T130210Z"
            ),
            (
                "acme/acme-api",
                "release-staged:acme/acme-api:api-20260919T130500Z"
            )
        ],
        "one item per repository, and an id that tells them apart"
    );

    // A staging that gave up pairs with its own repository's staging, not with
    // the newest staging anywhere: the newer one here is the other repository's.
    let gave_up = {
        let mut event = release_event(
            12,
            "release.stage_failed",
            next,
            "2026-09-19T13:20:00Z",
            true,
        );
        event.repo = Some("acme/acme-web".to_string());
        event
    };
    // acme-web's staged release is already live, so only the giving-up is left
    // there; acme-api's staging is untouched by it.
    let mut live = production(next, "2026-09-19T13:10:00Z", "success");
    live.repo = "acme/acme-web".to_string();
    live.latest = None;
    let items = release_items(&newest, std::slice::from_ref(&gave_up), &[live], &hosts());
    assert_eq!(
        kinds(&items),
        ["release_stage_failed", "release_staged"],
        "acme-web gave up after staging; acme-api is still waiting for a deploy"
    );
}

#[test]
fn a_staged_release_a_staging_that_gave_up_and_a_failed_deploy() {
    let live = "283416e2824d526ce03996cfa0cccca4ac3611d8";
    let next = "01dfe680a6de5e02da4e9aa7821534742aa46d7e";
    let staged = release_event(10, "release.staged", next, "2026-09-19T13:03:26Z", false);

    // Production runs an older commit, deployed before the staging.
    let waiting = release_items(
        slice(&staged),
        &[],
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
    let retry = release_items(slice(&staged), &[], &[attempt], &hosts());
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
    let unnamed = release_items(slice(&anonymous), &[], &[], &hosts());
    assert_eq!(kinds(&unnamed), ["release_staged"]);
    assert_eq!(
        unnamed[0].action.run_in.as_deref(),
        Some("release-box, in a checkout of the repository that staged it")
    );

    // A line that changes directory and loads the site settings itself runs as
    // pasted, so it is not wrapped in "in a <repo> checkout".
    let mut whole = staged.clone();
    let line = "cd ~/deploy-checkout && set -a && . ~/.config/jeryu/release.env && set +a \
                && scripts/release/deploy-release.sh prod-20260919T130210Z-01dfe68-unsigned";
    whole.detail = Some(json!({
        "release": "prod-20260919T130210Z-01dfe68-unsigned",
        "deploy_command": line,
    }));
    let pasted = release_items(slice(&whole), &[], &[], &hosts());
    assert_eq!(pasted[0].action.command.as_deref(), Some(line));
    assert_eq!(
        pasted[0].action.run_in.as_deref(),
        Some("release-box, any directory")
    );

    // Deployed: the same sha is live, or something newer than the staging is.
    assert!(
        release_items(
            slice(&staged),
            &[],
            &[production(next, "2026-09-19T13:10:00Z", "success")],
            &hosts()
        )
        .is_empty()
    );
    assert!(
        release_items(
            slice(&staged),
            &[],
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
            slice(&staged),
            slice(&gave_up),
            &prod_ok,
            &hosts()
        )),
        ["release_stage_failed"]
    );
    assert!(release_items(slice(&staged), slice(&retrying), &prod_ok, &hosts()).is_empty());
    assert!(release_items(slice(&staged), slice(&older), &prod_ok, &hosts()).is_empty());

    let failed = release_items(
        &[],
        &[],
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
    assert_eq!(body["schema_version"], "jeryu.attention/v1.6");
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
            "run_in": "the release host, in a jeryu/jeryu-deploy checkout",
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

    // The staged release names only its repository; it takes that
    // repository's family, so a family filter and the summary keep it.
    assert_eq!(of_kind("release_staged")["family"], "jeryu", "{body}");
    // `families` counts every family's rows over the whole inbox, and a
    // filtered, paged call answers the same summary.
    let families = body["families"].clone();
    let rows: u64 = families
        .as_array()
        .unwrap()
        .iter()
        .map(|family| {
            let subjects = &family["subjects"];
            ["critical", "action", "watch"]
                .iter()
                .map(|severity| subjects[severity].as_u64().unwrap())
                .sum::<u64>()
        })
        .sum();
    assert_eq!(rows, items.len() as u64, "{families}");
    let narrow = call(
        HttpMethod::GET,
        "/api/v1/attention?family=jeryu&severity=critical&limit=1",
        &admin,
        None,
    )
    .await
    .unwrap();
    let narrow = body_json(narrow).await;
    assert_eq!(narrow["items"].as_array().unwrap().len(), 1, "{narrow}");
    assert_eq!(narrow["families"], families);

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

/// The acceptance case of R4: a pull request that edits `ops/ci/pr-ci.sh`
/// reaches the inbox behind /needs-you, with a reason saying why a person has
/// to look. Nothing about the gate's conclusion changes; this is the flag.
#[tokio::test]
async fn a_pull_request_editing_the_ci_script_reaches_the_inbox() {
    let dir = tempfile::tempdir().unwrap();
    let (router, admin) = shift_forge(dir.path());
    let open_pr = |changed: Value| {
        router.clone().oneshot(request(
            HttpMethod::POST,
            "/api/v3/repos/jeryu/jeryu-deploy/pulls",
            &admin,
            Some(json!({
                "title": "Teach the gate a new step",
                "head": "alice/gate",
                "base": "main",
                "head_sha": "7b4d2c6b8e0f1a3d53f1c9e0b6a2d4857c1b0e9f",
                "changed_files": changed,
            })),
        ))
    };
    let created = open_pr(json!(["ops/ci/pr-ci.sh", "crates/jeryu-api/src/web.rs"]))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED, "{:?}", created);
    let number = body_json(created).await["number"].as_u64().unwrap();

    let body = body_json(
        router
            .clone()
            .oneshot(request(HttpMethod::GET, "/api/v1/attention", &admin, None))
            .await
            .unwrap(),
    )
    .await;
    let item = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "pr_changes_own_gate")
        .unwrap_or_else(|| panic!("no pr_changes_own_gate item in {body}"));
    assert_eq!(item["pr"], number);
    assert_eq!(item["repo"], "jeryu/jeryu-deploy");
    assert_eq!(item["severity"], "action");
    let reason = item["reason"].as_str().unwrap();
    assert!(reason.contains("changes ops/ci/pr-ci.sh"), "{reason}");
    assert!(reason.contains("own head"), "{reason}");
    // Only the gate-defining path is named: the ordinary file in the same diff
    // is not what makes this a question for a person.
    assert!(!reason.contains("web.rs"), "{reason}");
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

/// A lane the board draws red: one stage in state `bad`, as the collector
/// reported it.
fn red_lane() -> BoardLane {
    BoardLane {
        id: "cloud-app".to_string(),
        name: "Cloud app".to_string(),
        red_stages: vec!["prod: 4.2.0 is three releases behind stage".to_string()],
    }
}

fn board(observed_at: &str, lanes: Vec<BoardLane>, problems: Vec<&str>) -> BoardFacts {
    BoardFacts {
        family: "acme".to_string(),
        observed_at: observed_at.to_string(),
        lanes,
        problems: problems.into_iter().map(str::to_string).collect(),
    }
}

/// A red lane is one item per lane, pointing at the lane on the board; an
/// unread source is one item for the family; a board nobody refreshed is a
/// `watch` dated by the snapshot it is stuck on. A fresh board with nothing
/// red asks for nobody.
#[test]
fn a_red_lane_an_unread_source_and_a_board_that_stopped_arriving() {
    let fresh = "2026-09-19T12:55:00Z";
    let quiet = board(fresh, vec![], vec![]);
    assert_eq!(board_items(&[quiet], now()), Vec::new());

    let green = BoardLane {
        id: "website".to_string(),
        name: "Website".to_string(),
        red_stages: Vec::new(),
    };
    let one_red = board(fresh, vec![red_lane(), green], vec![]);
    let items = board_items(&[one_red], now());
    assert_eq!(kinds(&items), ["release_board_problem"]);
    assert_eq!(items[0].href, "/releases/family/acme#lane-cloud-app");
    assert_eq!(items[0].severity, Severity::Action);
    assert_eq!(items[0].family.as_deref(), Some("acme"));
    assert!(items[0].title.contains("Cloud app"), "{:?}", items[0]);
    assert!(
        items[0].reason.contains("1 stage of the lane is red")
            && items[0].reason.contains("three releases behind"),
        "{:?}",
        items[0]
    );
    assert_eq!(items[0].since, None);

    // Two hours of silence from the collector, with a source it could not
    // read on its last run.
    let stale = board(
        "2026-09-19T11:00:00Z",
        vec![red_lane()],
        vec!["fleet-nodes: ssh to node-2.example timed out"],
    );
    let items = board_items(&[stale], now());
    assert_eq!(
        kinds(&items),
        [
            "release_board_problem",
            "release_board_problem",
            "release_board_problem"
        ]
    );
    let severities: Vec<Severity> = items.iter().map(|item| item.severity).collect();
    assert_eq!(
        severities,
        [Severity::Action, Severity::Action, Severity::Watch]
    );
    assert!(
        items[1].reason.contains("ssh to node-2.example timed out"),
        "{:?}",
        items[1]
    );
    assert_eq!(items[2].href, "/releases/family/acme");
    assert_eq!(items[2].since.as_deref(), Some("2026-09-19T11:00:00Z"));
    // Every item of a board is its own row, and acknowledging one leaves the
    // others: the ids differ.
    let ids: BTreeSet<&str> = items.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids.len(), 3);

    // A snapshot with no readable date cannot be called fresh.
    let undated = board("", vec![], vec![]);
    let items = board_items(&[undated], now());
    assert_eq!(kinds(&items), ["release_board_problem"]);
    assert_eq!(items[0].severity, Severity::Watch);
    assert_eq!(items[0].since, None);
}

/// The inbox reads the stored boards, so a board PUT by a collector reaches
/// Needs you with the lane's own link.
#[test]
fn a_stored_board_with_a_red_lane_reaches_the_inbox() {
    let state = WebState::new(ForgeCore::new());
    let snapshot = json!({
        "schema": "jeryu.release_board.v1",
        "family": "acme",
        "observed_at": "2026-09-19T12:55:00Z",
        "summary": "One lane red.",
        "collector": {
            "host": "collector-1", "version": "1", "trigger": "timer", "duration_ms": 10
        },
        "lanes": [{
            "id": "cloud-app",
            "name": "Cloud app",
            "source": "acme/cloud-app",
            "owner_family": "acme",
            "stages": [{
                "id": "prod",
                "name": "prod",
                "version": "4.2.0",
                "state": "bad",
                "status": "4.2.0 is three releases behind stage",
                "known_by": "reported"
            }]
        }]
    });
    state
        .release_boards
        .put(
            "acme",
            serde_json::from_value(snapshot).expect("a v1 board"),
            now(),
        )
        .expect("the board is accepted");

    let board: Vec<Item> = super::attention::collect(&state, now())
        .items
        .into_iter()
        .filter(|item| item.kind == "release_board_problem")
        .collect();
    assert_eq!(board.len(), 1, "{board:?}");
    assert_eq!(board[0].href, "/releases/family/acme#lane-cloud-app");
    assert_eq!(board[0].family.as_deref(), Some("acme"));
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
    assert_eq!(item.action.run_in, None);
    assert_eq!(item.action.command, None);
    assert!(!item.reason.contains("deploy key"));
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
fn unset_hosts_fall_back_to_their_role_and_blank_overrides_are_ignored() {
    // No host default: a machine name is site data. An unset role is shown as
    // the role, so the instruction is still true wherever it is read.
    let unset = Hosts::default();
    assert_eq!(
        (
            unset.release.as_str(),
            unset.gate.as_str(),
            unset.forge.as_str()
        ),
        ("the release host", "the gate host", "the forge host")
    );
    let blank = Hosts::from_lookup(|name| (name == "JERYU_GATE_HOST").then(|| "  ".to_string()));
    assert_eq!(blank, unset);
    assert_eq!(hosts().gate, "gate-box");
    assert_eq!(Hosts::anywhere("node-0"), "node-0, any directory");
    assert_eq!(
        Hosts::checkout("node-0", "jeryu/jeryu-deploy"),
        "node-0, in a jeryu/jeryu-deploy checkout"
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
                "OWNER: only operator can pick the name.",
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
    ("main_red", "/repos/:provider/:owner/*"),
    ("merger_stalled", "/repos"),
    ("mirror_diverged", "/repos/:provider/:owner/*"),
    ("mirror_failing", "/repos"),
    // The pin's own item points at Releases; once a bump pull request is open,
    // that pull request is the next step.
    ("pin_behind", "/releases"),
    ("pin_behind", "/repos/:provider/:owner/*"),
    ("pr_already_landed", "/repos/:provider/:owner/*"),
    ("pr_awaiting_approval", "/repos/:provider/:owner/*"),
    // One row for a whole branch opens the search that lists exactly the pull
    // requests it stands for, in every repository they are in.
    ("pr_branch_campaign", "/search"),
    ("pr_changes_own_gate", "/repos/:provider/:owner/*"),
    ("pr_changes_requested", "/repos/:provider/:owner/*"),
    ("pr_checks_failing", "/repos/:provider/:owner/*"),
    ("pr_draft_waiting", "/repos/:provider/:owner/*"),
    ("pr_ready_to_merge", "/repos/:provider/:owner/*"),
    ("pr_superseded", "/repos/:provider/:owner/*"),
    ("pushed_after_merge", "/repos/:provider/:owner/*"),
    ("queue_failed", "/repos/:provider/:owner/*"),
    ("queue_refused", "/repos/:provider/:owner/*"),
    ("queue_stuck", "/repos/:provider/:owner/*"),
    ("release_board_problem", "/releases/family/:family"),
    ("release_stage_failed", "/activity"),
    ("release_staged", "/releases"),
    ("reviewer_offline", "/runners"),
    ("reviewer_rereviewing", "/repos/:provider/:owner/*"),
    ("reviewer_stuck", "/repos/:provider/:owner/*"),
    ("scorecard_digest", "/scorecard"),
    ("shift_budget_spent", "/work"),
    ("shift_stranded_work", "/work"),
    ("shift_without_pr", "/repos/:provider/:owner/*"),
    ("shift_without_pr", "/work"),
    ("stranded_branch", "/repos/:provider/:owner/*"),
    ("todo_blocked", "/work/:key"),
    ("todo_handoff", "/work/:key"),
    ("todo_parked", "/work/:key"),
    ("todo_check_outcome", "/work/:key"),
    ("todo_stuck_claim", "/work/:key"),
    ("todo_untriaged", "/work/:key"),
    ("todo_waiting_on_blocker", "/work/:key"),
    ("workers_down", "/work"),
];

/// One item of every kind the inbox can emit, from the rules themselves.
/// A branch holding one commit its pull request's merge did not take.
fn pushed_after_merge(repo: &str, merged: bool) -> PushedAfterMergeFacts {
    PushedAfterMergeFacts {
        repo: repo.to_string(),
        branch: "dev/widget-tax".to_string(),
        number: 115,
        title: "Widget tax".to_string(),
        base_ref: "main".to_string(),
        merged,
        commits: vec!["f2bed08".to_string()],
        commit_count: 1,
        closed_at: Some(now() - Duration::days(4)),
    }
}

fn stranded_branch(repo: &str, closed_pr: Option<u64>) -> StrandedBranchFacts {
    StrandedBranchFacts {
        repo: repo.to_string(),
        branch: "dayshift/2026-10-11".to_string(),
        default_branch: "main".to_string(),
        commit_count: 2,
        newest_sha: "f2bed08".to_string(),
        newest_subject: "finish invoice export".to_string(),
        newest_at: Some(now() - Duration::hours(2)),
        closed_pr,
        scanned_at: None,
    }
}

/// A commit pushed to a branch whose pull request already merged reaches
/// nothing, so the row names the pull request, the commit and the one act that
/// clears it: a new pull request from the branch.
#[test]
fn a_push_to_a_merged_branch_asks_for_a_new_pull_request() {
    let items = pushed_after_merge_items(&[pushed_after_merge("acme/widget-shop", true)]);
    assert_eq!(items.len(), 1, "{items:?}");
    let item = &items[0];
    assert_eq!(item.kind, "pushed_after_merge");
    assert_eq!(item.severity, Severity::Action);
    assert_eq!(item.repo.as_deref(), Some("acme/widget-shop"));
    assert_eq!(item.pr, Some(115));
    assert_eq!(item.sha.as_deref(), Some("f2bed08"));
    assert_eq!(item.href, "/repos/jeryu/acme/widget-shop/pulls/115");
    assert!(item.title.contains("dev/widget-tax"), "{}", item.title);
    assert!(item.reason.contains("already merged"), "{}", item.reason);
    assert!(item.reason.contains("f2bed08"), "{}", item.reason);
    assert!(item.reason.contains("not on main"), "{}", item.reason);
    assert!(
        item.next_step
            .contains("Open a new pull request from the branch"),
        "{}",
        item.next_step
    );
    assert_eq!(
        item.since.as_deref(),
        Some("2026-09-15T13:00:00Z"),
        "the row waits from the day the pull request closed"
    );
}

/// A pull request somebody closed unmerged strands its branch the same way,
/// and the row names it for what it is: a closed pull request's branch that
/// still holds commits.
#[test]
fn a_push_to_a_branch_closed_unmerged_says_it_was_not_merged() {
    let items = pushed_after_merge_items(&[pushed_after_merge("acme/widget-shop", false)]);
    assert!(
        items[0].reason.contains("branch still holds commits"),
        "{}",
        items[0].reason
    );
    assert!(
        !items[0].reason.contains("without merging") && !items[0].reason.contains("already"),
        "{}",
        items[0].reason
    );
    assert!(items[0].title.contains("closed #115"), "{}", items[0].title);
}

/// Nothing pushed after a merge, nothing to say: the rule has no standing row
/// of its own.
#[test]
fn no_stranded_push_raises_nothing() {
    assert!(pushed_after_merge_items(&[]).is_empty());
}

#[test]
fn a_stranded_branch_names_the_repo_branch_count_and_newest_subject() {
    let items = stranded_branch_items(&[stranded_branch("acme/widget-shop", None)]);
    assert_eq!(items.len(), 1, "{items:?}");
    let item = &items[0];
    assert_eq!(item.kind, "stranded_branch");
    assert_eq!(item.severity, Severity::Action);
    assert_eq!(item.repo.as_deref(), Some("acme/widget-shop"));
    assert_eq!(item.sha.as_deref(), Some("f2bed08"));
    assert_eq!(item.href, "/repos/jeryu/acme/widget-shop");
    assert!(item.title.contains("acme/widget-shop"), "{}", item.title);
    assert!(item.title.contains("dayshift/2026-10-11"), "{}", item.title);
    assert!(item.title.contains("2 unlanded commits"), "{}", item.title);
    assert!(
        item.title.contains("finish invoice export"),
        "{}",
        item.title
    );
    assert!(
        item.next_step.contains("Open a pull request"),
        "{}",
        item.next_step
    );
}

#[test]
fn a_capped_stranded_count_is_reported_as_a_lower_bound() {
    let mut fact = stranded_branch("acme/widget-shop", None);
    fact.commit_count = 200;
    let items = stranded_branch_items(&[fact]);
    assert!(items[0].title.contains("at least 200 unlanded commits"));
    assert!(
        items[0]
            .reason
            .contains("at least 200 patch-unique commits")
    );
}

#[test]
fn a_stranded_closed_pr_branch_recommends_a_fresh_branch() {
    let items = stranded_branch_items(&[stranded_branch("acme/widget-shop", Some(7))]);
    assert!(
        items[0].next_step.contains("fresh branch from main")
            && items[0].next_step.contains("cherry-pick -x"),
        "{}",
        items[0].next_step
    );
}

fn aged_branch(repo: &str, branch: &str, days: i64) -> StrandedBranchFacts {
    let mut fact = stranded_branch(repo, None);
    fact.branch = branch.to_string();
    fact.newest_at = Some(now() - Duration::days(days));
    fact
}

/// A first run that finds months of leftovers adds one row per repository,
/// naming the count and linking to the list; fresh branches stay individual.
#[test]
fn old_stranded_branches_arrive_as_one_row_per_repository() {
    let facts = vec![
        aged_branch("acme/widget-shop", "old/a", 90),
        aged_branch("acme/widget-shop", "old/b", 40),
        aged_branch("acme/widget-shop", "old/c", 10),
        aged_branch("acme/widget-shop", "fresh/d", 1),
        aged_branch("acme/lone", "old/only", 30),
        aged_branch("acme/other", "old/x", 20),
        aged_branch("acme/other", "old/y", 20),
    ];
    let (rest, backlogs) = split_backlog(facts, now());
    let rest: Vec<_> = rest
        .iter()
        .map(|f| (f.repo.as_str(), f.branch.as_str()))
        .collect();
    assert_eq!(
        rest,
        [("acme/widget-shop", "fresh/d"), ("acme/lone", "old/only")],
        "fresh branches and a lone old one stay individual"
    );
    let items = stranded_backlog_items(&backlogs, now());
    assert_eq!(items.len(), 2, "{items:?}");
    let shop = items
        .iter()
        .find(|item| item.repo.as_deref() == Some("acme/widget-shop"))
        .unwrap();
    assert_eq!(shop.kind, "stranded_branch");
    assert!(
        shop.title
            .contains("3 old branches hold commits not on main: review / delete"),
        "{}",
        shop.title
    );
    assert_eq!(
        shop.href,
        "/api/v1/repos/acme/widget-shop/stranded-branches"
    );
    assert_eq!(shop.since.as_deref(), Some("2026-06-21T13:00:00Z"));
    assert!(!shop.reason.contains("Scanned"), "{}", shop.reason);
    assert!(shop.reason.contains("old/a"), "{}", shop.reason);
    assert_ne!(items[0].id, items[1].id);
}

#[test]
fn the_backlog_row_says_how_long_ago_it_was_scanned() {
    let facts: Vec<_> = ["old/a", "old/b"]
        .iter()
        .map(|branch| {
            let mut fact = aged_branch("acme/widget-shop", branch, 30);
            fact.scanned_at = Some(now() - Duration::minutes(25));
            fact
        })
        .collect();
    let (_, backlogs) = split_backlog(facts, now());
    let items = stranded_backlog_items(&backlogs, now());
    assert!(
        items[0].reason.contains("Scanned 25 min ago."),
        "{}",
        items[0].reason
    );
}

#[test]
fn the_backlog_row_names_a_bounded_number_of_branches() {
    let facts: Vec<_> = (0..40)
        .map(|n| aged_branch("acme/widget-shop", &format!("old/{n:02}"), 30))
        .collect();
    let (rest, backlogs) = split_backlog(facts, now());
    assert!(rest.is_empty());
    let items = stranded_backlog_items(&backlogs, now());
    assert_eq!(items.len(), 1);
    assert!(
        items[0].title.contains("40 old branches"),
        "{}",
        items[0].title
    );
    assert!(
        items[0].reason.contains("and 35 more"),
        "{}",
        items[0].reason
    );
}

fn one_of_every_kind() -> Vec<Item> {
    let family = "acme";
    let repo = "acme/widget-shop";
    let mut items = Vec::new();

    items.extend(superseded_pull_items(&[
        SupersededPullFacts {
            author: "alice".into(),
            base_ref: "main".into(),
            source_repository: String::new(),
            repo: repo.into(),
            number: 40,
            title: "older".into(),
            body: None,
            head_ref: "feature".into(),
            created_at: now(),
            status: "open",
            recorded_replacement: None,
        },
        SupersededPullFacts {
            author: "alice".into(),
            base_ref: "main".into(),
            source_repository: String::new(),
            repo: repo.into(),
            number: 41,
            title: "newer".into(),
            body: Some("replaces #40".into()),
            head_ref: "feature-r2".into(),
            created_at: now() + Duration::minutes(1),
            status: "open",
            recorded_replacement: None,
        },
    ]));

    let mut due_check = crate::web::shift::todo_file::TodoFile::new(
        "due-check".into(),
        "acme".into(),
        "Checkout".into(),
    )
    .to_api(now());
    due_check.hypothesis.hypothesis = "Users finish checkout faster.".into();
    due_check.outcome_check_due_at = Some(now().to_rfc3339());
    items.extend(todo_items("acme", &[due_check], true, now()));
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
            title: "release: pin widget-core".to_string(),
            head_ref: "auto/pin-widget-core-89abcde".to_string(),
            head_sha: "89abcdef0123456789abcdef0123456789abcdef".to_string(),
            created_at: "2026-09-19T12:00:00Z".to_string(),
            changes_requested: false,
            blocking_review: None,
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
    items.extend(main_red_items(&[MainRedFacts {
        repo: repo.to_string(),
        sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        since: "2026-09-19T11:00:00Z".to_string(),
        first_step: "step failed (exit 1): ops/ci/check.sh".to_string(),
        log_id: "5f1d1b3e-2a0c-4f5e-9f2a-7c1d0b8e4a61".to_string(),
    }]));
    items.extend(pushed_after_merge_items(&[pushed_after_merge(repo, true)]));
    items.extend(stranded_branch_items(&[stranded_branch(repo, None)]));

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
            // A second one ready as long: the two fold into `merger_stalled`.
            posture(
                PullPosture {
                    checks_green: true,
                    can_merge: true,
                    ..PullPosture::default()
                },
                16,
            ),
            // Ready for a quarter of an hour: its own `pr_ready_to_merge` row.
            PullFacts {
                updated_at: now() - Duration::minutes(15),
                ..posture(
                    PullPosture {
                        checks_green: true,
                        can_merge: true,
                        ..PullPosture::default()
                    },
                    17,
                )
            },
            // The same, while the queue builds it: the queue is the next step.
            posture(
                PullPosture {
                    checks_green: true,
                    can_merge: true,
                    ..PullPosture::default()
                },
                15,
            ),
            // A head that rewrote the gate that judges it.
            PullFacts {
                changed_paths: vec!["ops/ci/pr-ci.sh".to_string()],
                ..posture(PullPosture::default(), 18)
            },
        ],
        std::slice::from_ref(&building),
        now(),
        &hosts(),
    ));
    // One change pushed to one branch in three repositories, each pull
    // request blocked by the same review: one campaign row.
    let campaign = |name: &str, number: u64| PullFacts {
        repo: format!("acme/{name}"),
        head_ref: "acme/basket-once".to_string(),
        ..posture(
            PullPosture {
                changes_requested: 1,
                ..PullPosture::default()
            },
            number,
        )
    };
    items.extend(pull_items(
        &[
            campaign("widget-shop", 21),
            campaign("widget-api", 22),
            campaign("widget-web", 23),
        ],
        &[],
        now(),
        &hosts(),
    ));
    items.extend(draft_items(&[draft(12, 9, "main")], 3, now()));
    items.extend(pull_items(
        &[PullFacts {
            repo: repo.to_string(),
            landed: Some(Landed::Content),
            ..pull(19, 45, PullPosture::default())
        }],
        &[],
        now(),
        &hosts(),
    ));

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
        &heads_at(&open, TEST_HEAD),
        1,
        true,
        reviewer_thresholds(),
        now(),
        &hosts(),
    ));
    items.extend(runner_items(
        &[runner("gate-2", &["pr-gate"], 30, None)],
        &heads_at(&open, TEST_HEAD),
        1,
        false,
        reviewer_thresholds(),
        now(),
        &hosts(),
    ));
    // A hold left on a head that has been replaced, with the reviewer already
    // on the new one: the informational row, not an action item.
    items.extend(runner_items(
        &[
            rereviewing("review-2", 35, NEW_HEAD),
            runner("gate-3", &["pr-gate"], 30, None),
        ],
        &heads_at(&open, NEW_HEAD),
        1,
        false,
        reviewer_thresholds(),
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
    items.extend(digest_items(
        &[digest_event(
            "2026-09-19T00:00:00Z",
            Some(serde_json::to_value(digest_of(true)).unwrap()),
        )],
        now(),
    ));
    items.extend(budget_items(
        &[exhausted(
            21,
            family,
            "nightshift/2026-09-19",
            "2026-09-19T06:00:00Z",
            3,
        )],
        &[],
    ));
    items.extend(board_items(
        &[BoardFacts {
            family: family.to_string(),
            observed_at: "2026-09-19T11:00:00Z".to_string(),
            lanes: vec![red_lane()],
            problems: vec!["fleet-nodes: ssh to node-2.example timed out".to_string()],
        }],
        now(),
    ));
    items.extend(release_items(
        slice(&staged),
        slice(&gave_up),
        &[production(
            "5fbe0ef2824d526ce03996cfa0cccca4ac3611d8",
            "2026-09-19T12:27:00Z",
            "failure",
        )],
        &hosts(),
    ));
    items
}

/// A recorded weekly Scorecard digest, as the job writes it: `warnings` is
/// what makes the row a decision rather than a reading.
fn digest_of(warnings: bool) -> crate::web::scorecard::digest::Digest {
    use crate::web::scorecard::digest::{Digest, Line, OldestUnownedHold, Warning};
    Digest {
        schema_version: "jeryu.scorecard.digest/v1".to_string(),
        generated_at: "2026-09-19T00:00:00Z".to_string(),
        window: "7d".to_string(),
        family: Some("acme".to_string()),
        headline: vec![
            Line {
                metric: "lead_time_p50".to_string(),
                label: "lead time p50".to_string(),
                unit: "seconds".to_string(),
                value: Some(12_000.0),
                previous: Some(6_000.0),
                change: Some(6_000.0),
                change_pct: Some(100.0),
                direction: "worse".to_string(),
                unknown: None,
                sample_size: 9,
            },
            Line {
                metric: "review_catch_rate".to_string(),
                label: "review catch rate".to_string(),
                unit: "percent".to_string(),
                value: None,
                previous: None,
                change: None,
                change_pct: None,
                direction: "unknown".to_string(),
                unknown: Some("source_not_landed: review finding outcomes".to_string()),
                sample_size: 0,
            },
        ],
        false_done_by_arm: Vec::new(),
        false_done_arms_omitted: 0,
        oldest_unowned_hold: Some(OldestUnownedHold {
            age: "over_7d".to_string(),
            unowned: 2,
            held: 3,
        }),
        warnings: if warnings {
            vec![Warning {
                guardrail: "lead_time_p50".to_string(),
                guardrail_label: "lead time p50".to_string(),
                unit: "seconds".to_string(),
                previous: 6_000.0,
                value: 12_000.0,
                volume: "prs_merged_per_day".to_string(),
                volume_label: "pull requests merged per day".to_string(),
                volume_previous: 2.0,
                volume_value: 5.0,
            }]
        } else {
            Vec::new()
        },
    }
}

/// The digest event the inbox reads, recorded at `ts`.
fn digest_event(ts: &str, detail: Option<Value>) -> Event {
    Event {
        kind: "scorecard.digest".to_string(),
        source: "forge".to_string(),
        reporter: "forge".to_string(),
        needs_human: false,
        summary: "Weekly scorecard (7d)".to_string(),
        detail,
        family: None,
        family_label: None,
        shift: None,
        todo_id: None,
        ts: ts.to_string(),
        ..exhausted(21, "acme", "nightshift/2026-09-19", ts, 0)
    }
}

/// The week's numbers are one row carrying the payload, so a surface shows
/// them without parsing the reason, and a traded guardrail makes it a decision.
#[test]
fn the_weekly_digest_row_carries_the_payload_and_names_the_trade() {
    let digest = digest_of(true);
    let detail = serde_json::to_value(&digest).unwrap();
    let items = digest_items(&[digest_event("2026-09-19T00:00:00Z", Some(detail))], now());
    assert_eq!(items.len(), 1);
    let row = &items[0];
    assert_eq!(row.kind, "scorecard_digest");
    assert_eq!(row.severity, Severity::Action);
    assert_eq!(row.digest.as_ref(), Some(&digest));
    assert_eq!(row.family.as_deref(), Some("acme"));
    assert_eq!(row.since.as_deref(), Some("2026-09-19T00:00:00Z"));
    assert!(
        row.reason.contains("lead time p50 3h 20m"),
        "{}",
        row.reason
    );
    // An unknown tile says so rather than reading as a zero.
    assert!(
        row.reason
            .contains("review catch rate unknown (source_not_landed"),
        "{}",
        row.reason
    );
    assert!(row.reason.contains("over_7d"), "{}", row.reason);
    assert!(
        row.reason
            .contains("while pull requests merged per day rose from 2 to 5"),
        "{}",
        row.reason
    );
    assert!(row.action.command.is_none());
    assert!(row.action.api.is_none());
}

/// A week with nothing traded away is a reading, not a decision.
#[test]
fn a_week_with_no_trade_off_is_a_watch_row() {
    let detail = serde_json::to_value(digest_of(false)).unwrap();
    let items = digest_items(&[digest_event("2026-09-19T00:00:00Z", Some(detail))], now());
    assert_eq!(items[0].severity, Severity::Watch);
    assert_eq!(items[0].action.label, "Read the week's numbers");
}

/// Last week's digest has been replaced by this week's, so it leaves the
/// inbox; an event with no payload the inbox can read shows no row at all.
#[test]
fn a_stale_or_unreadable_digest_shows_no_row() {
    let detail = serde_json::to_value(digest_of(false)).unwrap();
    assert!(
        digest_items(
            &[digest_event("2026-09-10T00:00:00Z", Some(detail.clone()))],
            now()
        )
        .is_empty()
    );
    assert!(digest_items(&[digest_event("2026-09-19T00:00:00Z", None)], now()).is_empty());
    assert!(
        digest_items(
            &[digest_event(
                "2026-09-19T00:00:00Z",
                Some(json!({"half": "a payload"}))
            )],
            now()
        )
        .is_empty()
    );
    assert!(digest_items(&[], now()).is_empty());
}

/// Every kind the rules emit is in [`KINDS`], the closed set `?kind=` accepts,
/// and nothing is listed there that no rule emits: a kind a reader may filter
/// by is one the inbox can actually answer with.
#[test]
fn the_published_kinds_are_the_kinds_the_rules_emit() {
    let emitted: BTreeSet<&str> = one_of_every_kind().iter().map(|item| item.kind).collect();
    assert_eq!(emitted, KINDS.iter().copied().collect::<BTreeSet<_>>());
}

/// A filter keeps only the rows asked for and recounts the severities for
/// them, so a filtered inbox's counts describe the rows it returned.
#[test]
fn a_filter_keeps_only_the_rows_asked_for_and_recounts_them() {
    let mut items = one_of_every_kind();
    // The same item in another family, to prove the filter drops it.
    let mut elsewhere = items[0].clone();
    elsewhere.id = format!("{}:globex", elsewhere.id);
    elsewhere.family = Some("globex".to_string());
    elsewhere.family_label = Some("globex".to_string());
    items.push(elsewhere);
    let response = AttentionResponse {
        schema_version: ATTENTION_SCHEMA,
        branch_discovery: crate::web::stranded_branches::COVERAGE_NOTICE,
        generated_at: "2026-09-19T13:00:00Z".to_string(),
        counts: Counts::default(),
        items,
    };
    let filtered = |filter: Filter| only_matching(&response, &filter);

    let acme = filtered(Filter {
        family: Some("acme".to_string()),
        ..Filter::default()
    });
    assert!(
        acme.items
            .iter()
            .all(|item| item.family.as_deref() == Some("acme")),
        "{:?}",
        acme.items.iter().map(|i| &i.family).collect::<Vec<_>>()
    );
    assert!(!acme.items.is_empty());
    assert_eq!(acme.branch_discovery, response.branch_discovery);
    assert_eq!(
        acme.counts.critical + acme.counts.action + acme.counts.watch,
        acme.items.len()
    );
    // An item of no family (a mirror, or a pin outside one) is not acme's.
    assert!(response.items.iter().any(|item| item.family.is_none()));
    assert!(response.items.len() > acme.items.len() + 1);

    let critical = filtered(Filter {
        severity: Some(Severity::Critical),
        ..Filter::default()
    });
    assert!(
        critical
            .items
            .iter()
            .all(|item| item.severity == Severity::Critical)
    );
    assert_eq!(critical.counts.action, 0);
    assert_eq!(critical.counts.watch, 0);
    assert_eq!(critical.counts.critical, critical.items.len());

    let blocked = filtered(Filter {
        family: Some("acme".to_string()),
        kind: Some("todo_blocked".to_string()),
        ..Filter::default()
    });
    assert!(
        blocked
            .items
            .iter()
            .all(|item| { item.kind == "todo_blocked" && item.family.as_deref() == Some("acme") })
    );
    assert!(!blocked.items.is_empty());

    // No filter at all keeps everything.
    assert!(!Filter::default().any());
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

#[test]
fn hypothesis_reminder_is_due_only_after_deployment_and_clears_after_a_check() {
    use crate::web::shift::hypothesis::{CheckAfter, Hypothesis, Outcome};
    use crate::web::shift::todo_file::TodoFile;
    let now = Utc::now();
    let mut file = TodoFile::new("idea-1".into(), "acme".into(), "Checkout".into());
    file.status = TodoStatus::Done;
    file.requested_by = "owner".into();
    let mut todo = file.to_api(now);
    todo.hypothesis = Hypothesis {
        hypothesis: "Users finish checkout faster.".into(),
        check_after: Some(CheckAfter::Days(7)),
        ..Default::default()
    };
    assert!(todo_items("acme", &[todo.clone()], true, now).is_empty());
    todo.outcome_check_due_at = Some((now + chrono::Duration::days(1)).to_rfc3339());
    assert!(todo_items("acme", &[todo.clone()], true, now).is_empty());
    todo.outcome_check_due_at = Some(now.to_rfc3339());
    let items = todo_items("acme", &[todo.clone()], true, now);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, "todo_check_outcome");
    assert_eq!(items[0].assignee.as_deref(), Some("owner"));
    assert_eq!(items[0].action.label, "Check outcome");
    todo.assignee = Some("holder".into());
    assert_eq!(
        todo_items("acme", &[todo.clone()], true, now)[0]
            .assignee
            .as_deref(),
        Some("holder")
    );
    for outcome in [Outcome::Met, Outcome::NotMet, Outcome::Unclear] {
        todo.hypothesis.outcome = outcome;
        assert!(todo_items("acme", &[todo.clone()], true, now).is_empty());
    }
}
