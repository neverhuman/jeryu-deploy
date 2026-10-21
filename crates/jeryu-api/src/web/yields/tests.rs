//! Yield: the report's arithmetic on invented todos, then the route.

use std::path::Path;

use axum::http::{Request as HttpRequest, StatusCode, header};
use chrono::{NaiveDate, TimeZone, Utc};
use jeryu_core::{ForgeCore, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::report::{By, ENOUGH_MERGED, Request, Window, build};
use super::{YieldQuery, window};
use crate::web::pipeline::LedgerRow;
use crate::web::shift::tests::run_git;
use crate::web::shift::todo_file::TodoFile;
use crate::web::shift::{Attempt, ShiftTodo, TodoPr, TodoStatus};
use crate::web::{WebState, app};

fn day(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
}

fn attempt(model: &str, outcome: &str, cost: f64, started: &str) -> Attempt {
    Attempt {
        by: "operator@node-a/w1".to_string(),
        host: "node-a".to_string(),
        slot: "w1".to_string(),
        model: model.to_string(),
        session: String::new(),
        started: started.to_string(),
        ended: String::new(),
        outcome: outcome.to_string(),
        cost_usd: Some(cost),
        note: String::new(),
        shift: String::new(),
        agent: None,
        factory: None,
        run: None,
        experiment: None,
        arm_p: None,
        cost_estimated: None,
        usage: None,
    }
}

fn todo(
    id: &str,
    family: &str,
    status: TodoStatus,
    merged: bool,
    attempts: Vec<Attempt>,
) -> ShiftTodo {
    let mut file = TodoFile::new(id.to_string(), family.to_string(), format!("Title {id}"));
    file.status = status;
    file.repos = vec!["widget".to_string()];
    file.requested_by = "operator".to_string();
    let mut todo = file.to_api(Utc::now());
    todo.merged = merged;
    todo.worked_by = attempts;
    todo
}

#[test]
fn rework_is_charged_to_the_original_arm_across_a_two_step_chain() {
    let mut original = todo(
        "a",
        "acme",
        TodoStatus::Done,
        true,
        vec![attempt("model-a", "done", 1.0, "2026-10-05T01:00:00Z")],
    );
    original.worked_by[0].experiment = Some("routing".into());
    original.worked_by[0].agent = Some("arm-a".into());
    let mut redo = todo(
        "b",
        "acme",
        TodoStatus::Done,
        true,
        vec![attempt("model-b", "done", 2.0, "2026-10-06T01:00:00Z")],
    );
    redo.reworks = vec!["a".into()];
    redo.worked_by[0].experiment = Some("routing".into());
    redo.worked_by[0].agent = Some("arm-b".into());
    let mut second_redo = todo(
        "c",
        "acme",
        TodoStatus::Done,
        true,
        vec![attempt("model-c", "done", 3.0, "2026-10-07T01:00:00Z")],
    );
    second_redo.reworks = vec!["b".into()];
    second_redo.worked_by[0].experiment = Some("routing".into());
    second_redo.worked_by[0].agent = Some("arm-c".into());
    let request = Request {
        window: Window {
            since: day("2026-10-01"),
            until: day("2026-10-14"),
        },
        family: None,
        by: By::Experiment,
        now: Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap(),
    };
    let result =
        serde_json::to_value(build(&request, &[original, redo, second_redo], &[], &[])).unwrap();
    let arms = result["experiments"][0]["arms"].as_array().unwrap();
    let arm_a = arms.iter().find(|arm| arm["agent"] == "arm-a").unwrap();
    let arm_b = arms.iter().find(|arm| arm["agent"] == "arm-b").unwrap();
    assert_eq!(arm_a["row"]["first_pass"], 0);
    assert_eq!(arm_a["row"]["reworked"], 1);
    close(&arm_a["row"]["rework_usd"], 2.0);
    close(&arm_a["row"]["usd_per_merged"], 3.0);
    assert_eq!(arm_b["row"]["reworked"], 1);
    close(&arm_b["row"]["rework_usd"], 3.0);
    close(&arm_b["row"]["spend_usd"], 2.0);
    assert_eq!(result["costliest"].as_array().unwrap()[1]["rework"], true);
}

#[test]
fn rework_accounting_keeps_same_id_todos_in_separate_families() {
    let a = todo(
        "shared-id",
        "acme",
        TodoStatus::Done,
        true,
        vec![attempt("model-a", "done", 1.0, "2026-10-05T01:00:00Z")],
    );
    let b = todo(
        "shared-id",
        "other",
        TodoStatus::Done,
        true,
        vec![attempt("model-b", "done", 2.0, "2026-10-05T01:00:00Z")],
    );
    let mut redo = todo(
        "redo",
        "acme",
        TodoStatus::Done,
        true,
        vec![attempt("model-c", "done", 3.0, "2026-10-06T01:00:00Z")],
    );
    redo.reworks = vec!["shared-id".into()];
    let request = Request {
        window: Window {
            since: day("2026-10-01"),
            until: day("2026-10-14"),
        },
        family: None,
        by: By::Model,
        now: Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap(),
    };
    let result = serde_json::to_value(build(&request, &[a, b, redo], &[], &[])).unwrap();
    let groups = result["groups"].as_array().unwrap();
    let a = groups.iter().find(|row| row["group"] == "model-a").unwrap();
    let b = groups.iter().find(|row| row["group"] == "model-b").unwrap();
    close(&a["rework_usd"], 3.0);
    close(&b["rework_usd"], 0.0);
    assert_eq!(b["first_pass"], 1);
}

fn pr(repo: &str, number: u64) -> TodoPr {
    TodoPr {
        repo: repo.to_string(),
        number,
        state: "merged".to_string(),
        url: format!("/acme/{repo}/pulls/{number}"),
    }
}

fn review(repo: &str, pr: i64, usd: f64, detail: Option<Value>) -> LedgerRow {
    LedgerRow {
        repo: Some(repo.to_string()),
        pr: Some(pr),
        cost_usd: usd,
        detail,
    }
}

/// Four in-window todos and one outside it:
/// - a: merged, a ratelimited attempt then a done one (first pass), triage 0.25
/// - b: merged, a retry on model-a then done on model-b through an experiment
/// - c: blocked (scrap), a killed estimated attempt then a blocked one
/// - d: open (pending), one routed retry
/// - e: merged, but every attempt started before the window
///
/// a and b ride the same shift pull request, reviewed for 1.00 + 0.50.
pub(super) fn scenario() -> (Vec<ShiftTodo>, Vec<LedgerRow>) {
    let mut a1 = attempt("model-a", "ratelimit", 0.5, "2026-10-04T23:30:00Z");
    a1.agent = Some("claude".to_string());
    // 01:00 at +02:00 is still Sunday in UTC: the week is the one before.
    let mut a2 = attempt("model-a", "done", 2.0, "2026-10-05T01:00:00+02:00");
    a2.usage = Some(
        json!({"model-a": {"input": 10, "output": 5, "cost_usd": 2.0,
        "cost_basis": "pricing@0000001"}}),
    );
    let mut a = todo("a", "acme", TodoStatus::Done, true, vec![a1, a2]);
    a.triage_cost_usd = Some(0.25);
    a.prs = vec![pr("widget", 7)];

    let b1 = attempt("model-a", "retry", 1.0, "2026-10-06T10:00:00Z");
    let mut b2 = attempt("model-b", "done", 3.0, "2026-10-07T10:00:00Z");
    b2.agent = Some("codex".to_string());
    b2.experiment = Some("exp-1".to_string());
    b2.arm_p = Some(0.25);
    let mut b = todo("b", "acme", TodoStatus::Done, true, vec![b1, b2]);
    b.prs = vec![pr("widget", 7)];

    let mut c1 = attempt("model-b", "killed", 0.4, "2026-10-11T08:00:00Z");
    c1.cost_estimated = Some(true);
    let c2 = attempt("model-b", "blocked", 0.6, "2026-10-11T09:00:00Z");
    let c = todo("c", "globex", TodoStatus::Blocked, false, vec![c1, c2]);

    let mut d1 = attempt("model-a", "retry", 1.0, "2026-10-12T08:00:00Z");
    d1.experiment = Some("exp-1".to_string());
    d1.arm_p = Some(0.75);
    let d = todo("d", "globex", TodoStatus::Open, false, vec![d1]);

    let e = todo(
        "e",
        "acme",
        TodoStatus::Done,
        true,
        vec![attempt("model-a", "done", 9.0, "2026-09-20T08:00:00Z")],
    );

    let reviews = vec![
        review(
            "acme/widget",
            7,
            1.0,
            Some(json!({"usage": {"model-c": {"cost_usd": 1.0, "cost_basis": "config"}}})),
        ),
        review("acme/widget", 7, 0.5, None),
        // Another pull request nothing in view carries.
        review("acme/widget", 8, 4.0, None),
    ];
    (vec![a, b, c, d, e], reviews)
}

fn report(by: By) -> Value {
    let (todos, reviews) = scenario();
    let request = Request {
        window: Window {
            since: day("2026-10-01"),
            until: day("2026-10-14"),
        },
        family: None,
        by,
        now: Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap(),
    };
    serde_json::to_value(build(&request, &todos, &reviews, &[])).unwrap()
}

pub(super) fn close(value: &Value, want: f64) {
    let got = value
        .as_f64()
        .unwrap_or_else(|| panic!("{value} is a number"));
    assert!((got - want).abs() < 1e-6, "{got} != {want}");
}

#[test]
fn totals_count_spend_against_where_each_todo_ended_up() {
    let report = report(By::Model);
    assert_eq!(report["schema_version"], "jeryu.yield/v1");
    assert_eq!(report["generated_at"], "2026-10-14T12:00:00Z");
    assert_eq!(
        report["window"],
        json!({"since": "2026-10-01", "until": "2026-10-14"})
    );
    assert_eq!(report["family"], Value::Null);
    assert_eq!(report["by"], "model");
    assert_eq!(
        report["price_sources"],
        json!(["config", "pricing@0000001"])
    );

    let totals = &report["totals"];
    assert_eq!(totals["todos"], 4, "e started before the window");
    assert_eq!(totals["attempts"], 7);
    assert_eq!(totals["merged"], 2);
    assert_eq!(
        totals["first_pass"], 1,
        "a's ratelimit is not a try; b needed two"
    );
    close(&totals["author_usd"], 8.5);
    // 1.50 of review on #7 split over a and b; #8 carries nothing in view.
    close(&totals["review_usd"], 1.5);
    close(&totals["triage_usd"], 0.25);
    close(&totals["spend_usd"], 10.25);
    close(&totals["main_usd"], 8.25);
    close(&totals["scrap_usd"], 1.0);
    close(&totals["pending_usd"], 1.0);
    close(&totals["killed_usd"], 0.4);
    close(&totals["estimated_usd"], 0.4);
    close(&totals["yield"], 8.25 / 9.25);
    close(&totals["usd_per_merged"], 4.125);
    // Merged spends 3.50 and 4.75: mean 4.125, sd 0.8839, half 1.225.
    close(&totals["usd_per_merged_ci95"][0], 2.9);
    close(&totals["usd_per_merged_ci95"][1], 5.35);
    close(&totals["first_pass_rate"], 0.5);

    let ids: Vec<&str> = report["costliest"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["b", "a", "c", "d"]);
    assert_eq!(
        report["costliest"][0],
        json!({"id": "b", "family": "acme", "title": "Title b", "status": "done",
               "merged": true, "rework": false, "spend_usd": 4.75, "attempts": 2,
               "agent": "codex", "model": "model-b"})
    );
    let scrap = report["scrap"].as_array().unwrap();
    assert_eq!(scrap.len(), 1);
    assert_eq!(scrap[0]["id"], "c");
    assert_eq!(
        scrap[0]["agent"], "claude",
        "an attempt without agent ran the default"
    );
}

#[test]
fn groups_take_attempt_spend_and_landing_outcomes() {
    let report = report(By::Model);
    let groups = report["groups"].as_array().unwrap();
    let names: Vec<&str> = groups
        .iter()
        .map(|g| g["group"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["model-a", "model-b"],
        "sorted by spend, highest first"
    );

    let a = &groups[0];
    assert_eq!(a["attempts"], 4);
    assert_eq!(a["todos"], 2, "a and d land on model-a");
    assert_eq!(a["merged"], 1);
    assert_eq!(a["first_pass"], 1);
    close(&a["author_usd"], 4.5);
    close(&a["review_usd"], 0.75);
    close(&a["triage_usd"], 0.25);
    close(&a["main_usd"], 4.5);
    close(&a["pending_usd"], 1.0);
    close(&a["spend_usd"], 5.5);
    close(&a["usd_per_merged"], 4.5);
    assert_eq!(a["usd_per_merged_ci95"], Value::Null, "one merged todo");

    let b = &groups[1];
    assert_eq!(b["attempts"], 3);
    assert_eq!(b["todos"], 2);
    assert_eq!(b["merged"], 1);
    assert_eq!(b["first_pass"], 0);
    close(&b["main_usd"], 3.75);
    close(&b["scrap_usd"], 1.0);
    close(&b["killed_usd"], 0.4);
    close(&b["estimated_usd"], 0.4);
    close(&b["yield"], 3.75 / 4.75);
    close(&b["first_pass_rate"], 0.0);

    let agents = report_names(By::Agent);
    assert_eq!(agents, ["claude", "codex"]);
    let experiments = report_names(By::Experiment);
    assert_eq!(
        experiments,
        [
            "(not routed)",
            "exp-1/codex:model-b",
            "exp-1/claude:model-a"
        ]
    );
    assert_eq!(report_names(By::Family), ["acme", "globex"]);
    assert_eq!(report_names(By::Repo), ["widget"]);
    assert_eq!(report_names(By::Requester), ["operator"]);
}

fn report_names(by: By) -> Vec<String> {
    report(by)["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["group"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn weeks_start_on_the_utc_monday_of_each_attempt() {
    let report = report(By::Week);
    let groups: Vec<(&str, f64)> = report["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["group"].as_str().unwrap(),
                g["spend_usd"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        [
            ("2026-10-05", 5.75),
            ("2026-09-28", 3.5),
            ("2026-10-12", 1.0)
        ]
    );
    assert_eq!(
        report["weekly"],
        json!([
            {"week": "2026-09-28", "group": "all", "merged": 1, "main_usd": 3.5, "usd_per_merged": 3.5},
            {"week": "2026-10-05", "group": "all", "merged": 1, "main_usd": 4.75, "usd_per_merged": 4.75},
            {"week": "2026-10-12", "group": "all", "merged": 0, "main_usd": 0.0, "usd_per_merged": null},
        ])
    );
    // Any other grouping splits each week by its groups.
    let weekly = report_weekly(By::Model);
    assert_eq!(
        weekly,
        [
            ("2026-09-28".to_string(), "model-a".to_string()),
            ("2026-10-05".to_string(), "model-a".to_string()),
            ("2026-10-05".to_string(), "model-b".to_string()),
            ("2026-10-12".to_string(), "model-a".to_string()),
        ]
    );
}

fn report_weekly(by: By) -> Vec<(String, String)> {
    report(by)["weekly"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            (
                w["week"].as_str().unwrap().to_string(),
                w["group"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn experiment_arms_compare_side_by_side_once_they_have_enough_data() {
    let report = report(By::Model);
    let experiments = report["experiments"].as_array().unwrap();
    assert_eq!(experiments.len(), 1);
    assert_eq!(experiments[0]["experiment"], "exp-1");
    let arms = experiments[0]["arms"].as_array().unwrap();
    assert_eq!(arms[0]["agent"], "claude");
    assert_eq!(arms[0]["model"], "model-a");
    close(&arms[0]["arm_p"], 0.75);
    assert_eq!(arms[0]["enough_data"], false);
    assert_eq!(arms[0]["row"]["merged"], 0);
    close(&arms[0]["row"]["pending_usd"], 1.0);
    assert_eq!(arms[1]["agent"], "codex");
    assert_eq!(arms[1]["model"], "model-b");
    close(&arms[1]["arm_p"], 0.25);
    assert_eq!(arms[1]["row"]["merged"], 1);
    close(&arms[1]["row"]["main_usd"], 3.75);

    // Twenty merged todos on one arm are enough to compare.
    let todos: Vec<ShiftTodo> = (0..ENOUGH_MERGED)
        .map(|n| {
            let mut run = attempt("model-b", "done", 1.0, "2026-10-02T00:00:00Z");
            run.experiment = Some("exp-2".to_string());
            todo(&format!("t{n}"), "acme", TodoStatus::Done, true, vec![run])
        })
        .collect();
    let request = Request {
        window: Window {
            since: day("2026-10-01"),
            until: day("2026-10-14"),
        },
        family: Some("acme".to_string()),
        by: By::Experiment,
        now: Utc::now(),
    };
    let built = serde_json::to_value(build(&request, &todos, &[], &[])).unwrap();
    let arm = &built["experiments"][0]["arms"][0];
    assert_eq!(arm["enough_data"], true);
    assert_eq!(arm["arm_p"], Value::Null, "no arm_p recorded");
    assert_eq!(built["family"], "acme");
    assert_eq!(built["totals"]["first_pass"], 20);
}

#[test]
fn merged_wins_over_status_and_scrap_is_only_unmerged_stops() {
    let run = |status, merged| {
        todo(
            "x",
            "acme",
            status,
            merged,
            vec![attempt("model-a", "done", 1.0, "2026-10-02T00:00:00Z")],
        )
    };
    let request = Request {
        window: Window {
            since: day("2026-10-01"),
            until: day("2026-10-14"),
        },
        family: None,
        by: By::Model,
        now: Utc::now(),
    };
    for (status, merged, bucket) in [
        (TodoStatus::Closed, true, "main_usd"),
        (TodoStatus::Closed, false, "scrap_usd"),
        (TodoStatus::Handoff, false, "scrap_usd"),
        (TodoStatus::Blocked, false, "scrap_usd"),
        (TodoStatus::Done, false, "pending_usd"),
        (TodoStatus::Claimed, false, "pending_usd"),
        (TodoStatus::Parked, false, "pending_usd"),
    ] {
        let built =
            serde_json::to_value(build(&request, &[run(status, merged)], &[], &[])).unwrap();
        close(&built["totals"][bucket], 1.0);
    }
    // Nothing in the window: every rate is undefined, not zero.
    let empty = serde_json::to_value(build(&request, &[], &[], &[])).unwrap();
    for rate in [
        "yield",
        "usd_per_merged",
        "usd_per_merged_ci95",
        "first_pass_rate",
    ] {
        assert_eq!(empty["totals"][rate], Value::Null, "{rate}");
    }
}

#[test]
fn the_window_defaults_to_28_days_and_refuses_backwards_ranges() {
    let today = day("2026-10-09");
    let query = |since: Option<&str>, until: Option<&str>| YieldQuery {
        since: since.map(str::to_string),
        until: until.map(str::to_string),
        ..YieldQuery::default()
    };
    assert_eq!(
        window(&query(None, None), today).unwrap(),
        Window {
            since: day("2026-09-11"),
            until: today
        }
    );
    assert_eq!(
        window(&query(None, Some("2026-10-01")), today)
            .unwrap()
            .since,
        day("2026-09-03")
    );
    assert!(window(&query(Some("2026-10-02"), Some("2026-10-01")), today).is_err());
}

const FAMILY_TOML: &str = r#"
[family]
name = "acme"
base_branch = "main"
landing = "shifts"
shift_tz = "UTC"

[[repo]]
name = "widget"
order = 1
"#;

const ROUTED_TODO: &str = r#"+++
id = "20261005-080000-aaaaaa"
family = "acme"
title = "Teach the widget to count"
repos = ["widget"]
mode = "night"
priority = 3
blocked_by = []
status = "blocked"
attempts = 1
requested_by = "operator"
filed_at = "2026-10-05T08:00:00Z"
claim_by = ""
lease_until = ""
shift = ""
change_set = ""
commits = {}
merged = false
note = ""
triaged = true
worked_by = [{ "by" = "operator@node-a/w1", "host" = "node-a", "slot" = "w1", "model" = "model-b", "session" = "s-1", "started" = "2026-10-05T08:01:00Z", "ended" = "2026-10-05T08:30:00Z", "outcome" = "blocked", "cost_usd" = 1.5, "note" = "", "shift" = "", "agent" = "codex", "experiment" = "exp-1", "arm_p" = 0.5 }]
triage_cost_usd = 0.125
+++
Count things.
"#;

/// A storage root with `acme/acme-todo.git`: a queue branch holding
/// family.toml and one routed, blocked todo.
fn acme_queue(root: &Path) {
    let owner = root.join("acme");
    std::fs::create_dir_all(&owner).unwrap();
    let work = root.join("work-todo");
    std::fs::create_dir_all(work.join("todos")).unwrap();
    run_git(&work, &["init", "-q"]);
    std::fs::write(work.join("family.toml"), FAMILY_TOML).unwrap();
    std::fs::write(
        work.join("todos/20261005-080000-aaaaaa-teach-the-widget-to-count.md"),
        ROUTED_TODO,
    )
    .unwrap();
    run_git(&work, &["checkout", "-q", "-b", "queue"]);
    run_git(&work, &["add", "."]);
    run_git(&work, &["commit", "-q", "-m", "seed"]);
    run_git(&owner, &["init", "-q", "--bare", "acme-todo.git"]);
    run_git(
        &work,
        &[
            "push",
            "-q",
            owner.join("acme-todo.git").to_str().unwrap(),
            "queue",
        ],
    );
}

fn get(uri: &str, token: &str) -> HttpRequest<axum::body::Body> {
    HttpRequest::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn json_of(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

#[tokio::test]
async fn yield_route_is_admin_only_refuses_bad_params_and_reads_the_queues() {
    let dir = tempfile::tempdir().unwrap();
    acme_queue(dir.path());
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    let admin = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let user = core
        .create_personal_access_token("bob", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new_with_git_storage(core, dir.path().to_path_buf())
            .with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let call = |uri: &str, token: &str| router.clone().oneshot(get(uri, token));

    let refused = call("/api/v1/yield", &user).await.unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_of(refused).await["code"], "permission_denied");

    for bad in [
        "/api/v1/yield?by=colour",
        "/api/v1/yield?since=10/01/2026",
        "/api/v1/yield?until=2026-13-01",
        "/api/v1/yield?since=2026-10-09&until=2026-10-01",
        "/api/v1/yield?sincee=2026-10-01",
    ] {
        let response = call(bad, &admin).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        let body = json_of(response).await;
        assert_eq!(body["code"], "invalid_query", "{bad}");
        assert!(body["repair_hint"].is_string(), "{bad}: house error shape");
    }
    let unknown = call("/api/v1/yield?family=initech", &admin).await.unwrap();
    assert_eq!(unknown.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json_of(unknown).await["code"], "family_unknown");

    let response = call(
        "/api/v1/yield?family=acme-split&since=2026-10-01&until=2026-10-14&by=experiment",
        &admin,
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_of(response).await;
    assert_eq!(body["family"], "acme", "either spelling, canonical back");
    assert_eq!(body["by"], "experiment");
    assert_eq!(body["totals"]["todos"], 1);
    close(&body["totals"]["author_usd"], 1.5);
    close(&body["totals"]["triage_usd"], 0.125);
    close(&body["totals"]["scrap_usd"], 1.625);
    assert_eq!(body["totals"]["yield"], 0.0);
    assert_eq!(body["groups"][0]["group"], "exp-1/codex:model-b");
    close(&body["experiments"][0]["arms"][0]["arm_p"], 0.5);
    assert_eq!(body["scrap"][0]["id"], "20261005-080000-aaaaaa");

    // The three headline numbers ride the same answer, over the same todos.
    assert_eq!(body["headline"]["exception_rate"]["todos"], 1);
    assert_eq!(body["headline"]["exception_rate"]["needed_human"], 1);
    assert_eq!(body["headline"]["exception_rate"]["rate"], 1.0);
    assert_eq!(body["headline"]["exception_rate"]["reasons"]["blocked"], 1);
    assert_eq!(body["headline"]["lead_time"]["filed_to_prod_s"]["n"], 0);
    assert_eq!(
        body["headline"]["lead_time"]["claim_to_main_s"]["p50"],
        Value::Null
    );
    assert_eq!(body["headline"]["bad_change_rate"]["changes"], 0);
    assert_eq!(body["headline"]["bad_change_rate"]["rate"], Value::Null);
    assert_eq!(
        body["headline"]["bad_change_rate"]["unknown"],
        "no_merged_changes"
    );

    let outside = call("/api/v1/yield?since=2026-11-01&until=2026-11-02", &admin)
        .await
        .unwrap();
    let outside = json_of(outside).await;
    assert_eq!(outside["totals"]["todos"], 0);
    assert_eq!(outside["headline"]["exception_rate"]["todos"], 0);
    assert_eq!(outside["headline"]["exception_rate"]["rate"], Value::Null);
}
