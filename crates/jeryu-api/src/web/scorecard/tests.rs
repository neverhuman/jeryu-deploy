//! Scorecard arithmetic on invented fixtures (families acme and globex, host
//! node-a): one test per computed metric, the empty window and the previous
//! window.

use serde_json::{Value, json};

use super::compute::{CostRow, Ctx, DAY_MS, EventRow, FlowRow, Raw, metrics};
use super::{ScorecardQuery, StrictFields};
use crate::web::flow::Station;
use crate::web::outcomes::project::{Change, Leg};
use crate::web::pipeline::{EventStore, NewEvent};

const NOW: i64 = 1_760_000_000_000;
const H: i64 = 60 * 60 * 1000;

fn report(raw: &Raw, changes: &[Change], days: usize) -> Value {
    report_for(raw, changes, None, days)
}

fn report_for(raw: &Raw, changes: &[Change], repo: Option<&str>, days: usize) -> Value {
    let ctx = Ctx::new(raw, changes, repo, NOW, days);
    Value::Object(
        metrics(&ctx)
            .into_iter()
            .map(|(name, m)| (name.to_string(), serde_json::to_value(m).unwrap()))
            .collect(),
    )
}

fn visit(
    lot: &str,
    station: Station,
    entered: i64,
    started: Option<i64>,
    left: Option<i64>,
    direction: &str,
) -> FlowRow {
    FlowRow {
        lot_id: lot.to_string(),
        repo: Some("acme/widget".to_string()),
        station,
        entered_ms: entered,
        started_ms: started,
        left_ms: left,
        direction: direction.to_string(),
    }
}

/// A lot filed at `filed` that reaches production `16h + tail_h` later: two
/// hours queued and worked, a gate that waits 3h and runs 1h, a deploy of
/// `tail_h`.
fn journey(lot: &str, filed: i64, tail_h: i64) -> Vec<FlowRow> {
    use Station::{Deploy, Gate, Merge, Queue, Review, Stage, Work};
    let at = |h: i64| filed + h * H;
    vec![
        visit(lot, Queue, at(0), Some(at(1)), Some(at(2)), "forward"),
        visit(lot, Work, at(2), Some(at(2)), Some(at(10)), "forward"),
        visit(lot, Review, at(10), Some(at(11)), Some(at(12)), "forward"),
        visit(lot, Gate, at(10), Some(at(13)), Some(at(14)), "forward"),
        visit(lot, Merge, at(14), Some(at(14)), Some(at(15)), "forward"),
        visit(lot, Stage, at(15), None, Some(at(16)), "forward"),
        visit(
            lot,
            Deploy,
            at(16),
            Some(at(16)),
            Some(at(16 + tail_h)),
            "forward",
        ),
    ]
}

fn flow_fixture() -> Raw {
    let mut flow = Vec::new();
    // Three lots shipped inside the last 7 days: lead 20h, 30h, 40h.
    for (i, tail) in [4, 14, 24].into_iter().enumerate() {
        flow.extend(journey(
            &format!("20261001-00000{i}-aaaaaa"),
            NOW - (6 * DAY_MS) + i as i64 * 3 * H,
            tail,
        ));
    }
    // One shipped in the window before: lead 20h.
    flow.extend(journey("20260920-000009-bbbbbb", NOW - 12 * DAY_MS, 4));
    Raw {
        flow,
        flow_from: Some(NOW - 30 * DAY_MS),
        ..Raw::default()
    }
}

fn value(report: &Value, name: &str) -> Value {
    report[name]["value"].clone()
}

#[test]
fn lead_time_percentiles_come_from_raw_durations_with_previous_and_daily() {
    let r = report(&flow_fixture(), &[], 7);
    assert_eq!(value(&r, "lead_time_p50"), json!(108_000));
    assert_eq!(value(&r, "lead_time_p85"), json!(144_000));
    assert_eq!(r["lead_time_p50"]["sample_size"], 3);
    assert_eq!(r["lead_time_p50"]["previous"], json!(72_000));
    let daily = r["lead_time_p50"]["daily"].as_array().unwrap();
    assert_eq!(daily.len(), 7);
    assert_eq!(daily.iter().filter(|d| !d.is_null()).count(), 2);
}

#[test]
fn lead_time_spread_is_p85_minus_p50() {
    let r = report(&flow_fixture(), &[], 7);
    assert_eq!(value(&r, "lead_time_spread"), json!(36_000));
}

#[test]
fn station_breakdown_splits_queue_from_process_per_station() {
    let r = report(&flow_fixture(), &[], 7);
    let gate = &r["station_breakdown"]["value"]["gate"];
    assert_eq!(gate["total_p50_s"], 14_400);
    assert_eq!(gate["queue_p50_s"], 10_800);
    assert_eq!(gate["process_p50_s"], 3_600);
    assert_eq!(
        r["station_breakdown"]["value"]["deploy"]["total_p50_s"],
        50_400
    );
    // Arrival and exit measure dwell; they do not measure active execution.
    assert_eq!(
        r["station_breakdown"]["value"]["stage"]["queue_p50_s"],
        Value::Null
    );
    assert_eq!(
        r["station_breakdown"]["value"]["stage"]["total_p50_s"],
        3_600
    );
    assert_eq!(r["station_breakdown"]["sample_size"], 3);
}

#[test]
fn flow_efficiency_is_active_time_over_lead_time_when_every_start_is_measured() {
    // Arithmetic-only fixture: current producers do not measure Stage starts.
    let mut raw = flow_fixture();
    for row in raw.flow.iter_mut().filter(|r| r.station == Station::Stage) {
        row.started_ms = Some(row.entered_ms);
    }
    let r = report(&raw, &[], 7);
    // 17/20, 27/30, 37/40 hours: median 0.9.
    assert_eq!(value(&r, "flow_efficiency"), json!(0.9));
    assert_eq!(r["flow_efficiency"]["previous"], json!(0.85));
}

#[test]
fn flow_efficiency_skips_a_lot_with_an_unclassified_visit() {
    let mut raw = flow_fixture();
    for row in raw
        .flow
        .iter_mut()
        .filter(|r| matches!(r.station, Station::Merge | Station::Stage))
    {
        row.started_ms = None;
    }
    let r = report(&raw, &[], 7);
    assert_eq!(r["flow_efficiency"]["value"], Value::Null);
    assert_eq!(
        r["flow_efficiency"]["unknown"]["reason"],
        "execution_starts_not_recorded"
    );
}

#[test]
fn wip_counts_open_visits_by_station_and_state() {
    let mut raw = flow_fixture();
    raw.flow.push(visit(
        "acme/widget#5",
        Station::Gate,
        NOW - 2 * H,
        None,
        None,
        "open",
    ));
    raw.flow.push(visit(
        "acme/widget#6",
        Station::Review,
        NOW - 3 * H,
        Some(NOW - H),
        None,
        "open",
    ));
    raw.flow.push(visit(
        "20261002-000001-cccccc",
        Station::Work,
        NOW - 9 * H,
        None,
        None,
        "hold",
    ));
    let r = report(&raw, &[], 7);
    let v = &r["wip_by_station"]["value"];
    assert_eq!(v["gate"], json!({"waiting": 1, "active": 0, "held": 0}));
    assert_eq!(v["review"], json!({"waiting": 0, "active": 1, "held": 0}));
    assert_eq!(v["work"], json!({"waiting": 0, "active": 0, "held": 1}));
    assert_eq!(r["wip_by_station"]["sample_size"], 3);
    // A week ago nothing of these was on the line yet.
    assert_eq!(r["wip_by_station"]["previous"]["gate"]["waiting"], 0);
}

#[test]
fn holds_are_aged_and_split_by_whether_anyone_acted() {
    let mut raw = flow_fixture();
    raw.flow.push(visit(
        "20261002-000001-cccccc",
        Station::Work,
        NOW - 5 * DAY_MS,
        None,
        None,
        "hold",
    ));
    raw.flow.push(visit(
        "20261002-000002-dddddd",
        Station::Work,
        NOW - 2 * H,
        None,
        None,
        "hold",
    ));
    raw.events.push(EventRow {
        seq: 1,
        ts_ms: NOW - DAY_MS,
        kind: "todo.action".to_string(),
        repo: None,
        pr: None,
        sha: None,
        todo_id: Some("20261002-000001-cccccc".to_string()),
        outcome: Some("released".to_string()),
        detail: None,
    });
    let r = report(&raw, &[], 7);
    assert_eq!(r["holds_by_age_and_action"]["value"], 2);
    let d = &r["holds_by_age_and_action"]["detail"];
    assert_eq!(d["acting"], 1);
    assert_eq!(d["not_acting"], 1);
    assert_eq!(
        d["by_age"][0],
        json!({"age": "under_1d", "held": 1, "acting": 0})
    );
    assert_eq!(
        d["by_age"][2],
        json!({"age": "3d_to_7d", "held": 1, "acting": 1})
    );
    assert_eq!(r["holds_by_age_and_action"]["previous"], Value::Null);
}

#[test]
fn gate_queue_wait_is_arrival_to_start() {
    let r = report(&flow_fixture(), &[], 7);
    assert_eq!(
        r["gate_queue_wait"]["value"],
        json!({"p50_s": 10_800, "p85_s": 10_800})
    );
    assert_eq!(r["gate_queue_wait"]["sample_size"], 3);
    assert_eq!(r["gate_queue_wait"]["previous"]["p50_s"], 10_800);
}

#[test]
fn eta_error_stays_unknown_because_no_eta_is_stored() {
    let r = report(&flow_fixture(), &[], 7);
    assert_eq!(r["eta_error"]["value"], Value::Null);
    assert!(
        r["eta_error"]["unknown"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("source_not_recorded")
    );
}

fn leg(repo: &str, pr: i64, merged: i64, shipped: Option<i64>, release: &str) -> Leg {
    Leg {
        repo: repo.to_string(),
        pr: Some(pr),
        sha: None,
        merged_ms: Some(merged),
        released_ms: None,
        deployed_ms: shipped,
        release: shipped.map(|_| release.to_string()),
    }
}

fn change(todo: &str, arm: &str, work: Option<&str>, legs: Vec<Leg>) -> Change {
    let filed = legs.iter().filter_map(|l| l.merged_ms).min().unwrap() - 2 * H;
    Change {
        lot_id: todo.to_string(),
        family: Some("acme".to_string()),
        todo_id: Some(todo.to_string()),
        work_kind: work.map(ToString::to_string),
        first_seen_ms: filed,
        filed_ms: Some(filed),
        done_ms: Some(filed + H),
        agent: Some("codex".to_string()),
        model: Some(arm.to_string()),
        legs,
        ..Change::default()
    }
}

/// With a 30 day window: A succeeds, B is reverted, C is reworked, D is
/// still in its observation; E and F shipped in the window before.
fn outcome_fixture() -> Vec<Change> {
    let d = DAY_MS;
    let a = change(
        "20261001-000001-aaaaaa",
        "model-a",
        Some("feature"),
        vec![leg(
            "acme/widget",
            11,
            NOW - 26 * d,
            Some(NOW - 25 * d),
            "w-1",
        )],
    );
    let mut b = change(
        "20261001-000002-bbbbbb",
        "model-b",
        Some("fix"),
        vec![leg(
            "acme/widget",
            12,
            NOW - 24 * d,
            Some(NOW - 25 * d + H),
            "w-1",
        )],
    );
    b.reverted_ms = Some(NOW - 20 * d);
    let mut c = change(
        "20261001-000003-cccccc",
        "model-a",
        None,
        vec![leg(
            "acme/gadget",
            13,
            NOW - 23 * d,
            Some(NOW - 22 * d),
            "g-1",
        )],
    );
    c.reworked_by = vec!["20261005-000001-ffffff".to_string()];
    let dd = change(
        "20261001-000004-dddddd",
        "model-a",
        Some("ops"),
        vec![leg(
            "acme/widget",
            14,
            NOW - 3 * d,
            Some(NOW - 2 * d),
            "w-2",
        )],
    );
    let e = change(
        "20260901-000001-eeeeee",
        "model-a",
        Some("feature"),
        vec![leg(
            "acme/widget",
            5,
            NOW - 50 * d,
            Some(NOW - 49 * d),
            "w-0",
        )],
    );
    let mut f = change(
        "20260901-000002-ffffff",
        "model-a",
        Some("feature"),
        vec![leg(
            "acme/widget",
            6,
            NOW - 48 * d,
            Some(NOW - 47 * d),
            "w-0",
        )],
    );
    f.rolled_back_ms = Some(NOW - 40 * d);
    vec![a, b, c, dd, e, f]
}

#[test]
fn change_failure_rate_counts_failed_over_observed_changes() {
    let r = report(&Raw::default(), &outcome_fixture(), 30);
    let m = &r["change_failure_rate"];
    assert_eq!(m["value"], json!(33.3));
    assert_eq!(m["sample_size"], 3);
    assert_eq!(m["detail"]["pending"], 1);
    assert_eq!(
        m["detail"]["by_arm"]["codex:model-b"],
        json!({"numerator": 1, "denominator": 1})
    );
    assert_eq!(m["previous"], json!(50.0));
}

#[test]
fn rework_rate_counts_reworked_over_observed_changes() {
    let r = report(&Raw::default(), &outcome_fixture(), 30);
    assert_eq!(r["rework_rate"]["value"], json!(33.3));
    assert_eq!(
        r["rework_rate"]["detail"]["by_arm"]["codex:model-a"]["numerator"],
        1
    );
    assert_eq!(r["rework_rate"]["previous"], json!(0.0));
}

#[test]
fn false_done_rate_counts_reworked_or_reverted_done_reports() {
    let r = report(&Raw::default(), &outcome_fixture(), 30);
    assert_eq!(r["false_done_rate"]["value"], json!(66.7));
    assert_eq!(r["false_done_rate"]["sample_size"], 3);
}

#[test]
fn successful_changes_are_counted_and_zero_only_where_recorded() {
    let r = report(&Raw::default(), &outcome_fixture(), 30);
    assert_eq!(r["successful_changes"]["value"], 1);
    assert_eq!(r["successful_changes"]["previous"], 1);
    // Shipped before the ledger's first change: unknown, not 0.
    let r = report(&Raw::default(), &outcome_fixture()[..1], 7);
    assert_eq!(r["successful_changes"]["value"], 0);
    assert_eq!(r["change_failure_rate"]["value"], Value::Null);
}

#[test]
fn completed_work_mix_counts_kinds_and_keeps_unclassified_apart() {
    let r = report(&Raw::default(), &outcome_fixture(), 30);
    let m = &r["completed_work_mix"];
    assert_eq!(
        m["value"],
        json!({"feature": 1, "fix": 1, "rework": 0, "ops": 1, "unclassified": 1})
    );
    assert_eq!(m["detail"]["share_of_classified_pct"]["fix"], json!(33.3));
    assert_eq!(m["sample_size"], 4);
}

#[test]
fn changes_per_release_is_the_batch_size_per_release() {
    let r = report(&Raw::default(), &outcome_fixture(), 30);
    // widget w-1 carries A and B; gadget g-1 carries C; widget w-2 carries D.
    assert_eq!(
        r["changes_per_release"]["value"],
        json!({"p50": 1, "p85": 2})
    );
    assert_eq!(r["changes_per_release"]["sample_size"], 3);
    assert_eq!(
        r["changes_per_release"]["previous"],
        json!({"p50": 2, "p85": 2})
    );
}

#[test]
fn completed_items_and_merged_prs_per_day() {
    let r = report(&Raw::default(), &outcome_fixture(), 30);
    assert_eq!(r["completed_work_items_per_day"]["value"], json!(0.133));
    assert_eq!(r["completed_work_items_per_day"]["sample_size"], 4);
    assert_eq!(r["prs_merged_per_day"]["value"], json!(0.133));
    // The ledger's first change is 50 days old: the previous window is
    // counted over the 20 days it was recording, not the 30 it spans.
    assert_eq!(r["prs_merged_per_day"]["previous"], json!(0.1));
}

#[test]
fn arrival_and_completion_rates_give_a_gap() {
    let r = report(&Raw::default(), &outcome_fixture(), 30);
    let v = &r["arrival_vs_completion"]["value"];
    assert_eq!(v["filed_per_day"], json!(0.133));
    assert_eq!(v["completed_per_day"], json!(0.133));
    assert_eq!(v["gap_per_day"], json!(0.0));
}

#[test]
fn merged_but_unreleased_counts_and_ages_the_unshipped() {
    let mut changes = outcome_fixture();
    changes.push(change(
        "20261001-000005-eeeeee",
        "model-a",
        None,
        vec![Leg {
            repo: "acme/widget".to_string(),
            pr: Some(15),
            merged_ms: Some(NOW - 4 * DAY_MS),
            ..Leg::default()
        }],
    ));
    let r = report(&Raw::default(), &changes, 30);
    assert_eq!(r["merged_but_unreleased"]["value"], 1);
    assert_eq!(
        r["merged_but_unreleased"]["detail"]["oldest_age_s"],
        4 * 86_400
    );
    // A week ago it was merged ... three days after that: not yet.
    assert_eq!(r["merged_but_unreleased"]["previous"], 0);
}

fn event(seq: i64, ts: i64, kind: &str, repo: &str, outcome: Option<&str>) -> EventRow {
    EventRow {
        seq,
        ts_ms: ts,
        kind: kind.to_string(),
        repo: Some(repo.to_string()),
        pr: None,
        sha: None,
        todo_id: None,
        outcome: outcome.map(ToString::to_string),
        detail: None,
    }
}

#[test]
fn releases_and_deploys_per_service_per_day_are_deduplicated() {
    let mut raw = Raw {
        events_from: Some(NOW - 30 * DAY_MS),
        ..Raw::default()
    };
    let mut push =
        |seq, kind: &str, repo: &str, release: &str, outcome: Option<&str>, days: i64| {
            let mut e = event(seq, NOW - days * DAY_MS, kind, repo, outcome);
            e.detail = Some(json!({"release": release}));
            raw.events.push(e);
        };
    push(1, "release.staged", "acme/widget", "w-1", None, 3);
    push(2, "release.staged", "acme/widget", "w-2", None, 2);
    push(3, "deploy.status", "acme/widget", "w-1", Some("success"), 3);
    push(4, "deploy.status", "acme/widget", "w-1", Some("success"), 3);
    push(5, "deploy.status", "acme/widget", "w-2", Some("failure"), 2);
    push(6, "release.staged", "globex/api", "a-9", None, 9);
    let r = report(&raw, &[], 7);
    let v = &r["releases_per_day"]["value"];
    assert_eq!(v["releases"]["acme/widget"], json!(0.286));
    assert_eq!(v["deploys"]["acme/widget"], json!(0.143));
    assert!(v["releases"].get("globex/api").is_none());
    assert_eq!(
        r["releases_per_day"]["previous"]["releases"]["globex/api"],
        json!(0.143)
    );
}

#[test]
fn gate_flake_rate_pairs_failures_and_passes_on_one_commit() {
    let mut raw = Raw {
        events_from: Some(NOW - 30 * DAY_MS),
        ..Raw::default()
    };
    for (seq, sha, outcome) in [
        (1, "s1", "failure"),
        (2, "s1", "success"),
        (3, "s2", "success"),
        (4, "s3", "failure"),
    ] {
        let mut e = event(
            seq,
            NOW - DAY_MS,
            "gate.finished",
            "acme/widget",
            Some(outcome),
        );
        e.pr = Some(11);
        e.sha = Some(sha.to_string());
        raw.events.push(e);
    }
    let r = report(&raw, &[], 7);
    assert_eq!(r["gate_flake_rate"]["value"], json!(33.3));
    assert_eq!(r["gate_flake_rate"]["sample_size"], 3);
}

/// One `todo.attempt_finished` done event with the criteria counts todoq writes.
fn done_attempt(
    seq: i64,
    ts: i64,
    todo: &str,
    state: &str,
    met: i64,
    total: i64,
    commits: Value,
) -> EventRow {
    let mut e = event(
        seq,
        ts,
        "todo.attempt_finished",
        "acme/widget",
        Some("done"),
    );
    e.todo_id = Some(todo.to_string());
    e.detail = Some(json!({
        "criteria_state": state,
        "criteria_met": met,
        "criteria_total": total,
        "commits": commits,
    }));
    e
}

fn criteria_fixture() -> Raw {
    let t = NOW - DAY_MS;
    Raw {
        events_from: Some(NOW - 30 * DAY_MS),
        events: vec![
            done_attempt(1, t, "a", "set", 2, 3, json!({"acme/widget": "s1"})),
            // The same todo reported done twice: only its last attempt counts.
            done_attempt(2, t, "a", "set", 3, 3, json!({"acme/widget": "s1"})),
            done_attempt(3, t, "b", "set", 1, 1, json!({"globex/api": "s2"})),
            // Filed before the block existed, and filed without criteria: uncovered.
            done_attempt(4, t, "c", "unknown", 0, 0, json!({"acme/widget": "s3"})),
            done_attempt(5, t, "d", "none", 0, 0, json!({"acme/widget": "s4"})),
        ],
        ..Raw::default()
    }
}

#[test]
fn acceptance_criteria_met_counts_checked_boxes_and_excludes_todos_without_any() {
    let m = &report(&criteria_fixture(), &[], 7)["acceptance_criteria_met"];
    assert_eq!(m["value"], json!(100.0)); // 3 of 3 on todo a, 1 of 1 on todo b
    assert_eq!(m["sample_size"], 2);
    assert_eq!(m["detail"]["criteria_met"], 4);
    assert_eq!(m["detail"]["criteria_listed"], 4);
    assert_eq!(m["detail"]["todos_without_criteria"], 2);
}

#[test]
fn acceptance_criteria_met_follows_the_repository_filter_and_is_unknown_without_samples() {
    let raw = criteria_fixture();
    let m = &report_for(&raw, &[], Some("globex/api"), 7)["acceptance_criteria_met"];
    // Todo b landed in globex/api; the event names acme/widget as its first landing.
    assert_eq!(m["sample_size"], 1);
    assert_eq!(m["detail"]["criteria_listed"], 1);
    let empty = Raw {
        events_from: Some(NOW - 30 * DAY_MS),
        ..Raw::default()
    };
    let m = &report(&empty, &[], 7)["acceptance_criteria_met"];
    assert_eq!(m["value"], Value::Null);
    assert_eq!(
        m["unknown"]["reason"],
        json!("no_acceptance_criteria_in_window")
    );
}

fn cost(
    ts: i64,
    kind: &str,
    repo: Option<&str>,
    pr: Option<i64>,
    todo: Option<&str>,
    usd: f64,
) -> CostRow {
    CostRow {
        ts_ms: ts,
        kind: kind.to_string(),
        repo: repo.map(ToString::to_string),
        pr,
        todo_id: todo.map(ToString::to_string),
        cost_usd: usd,
        seconds: Some(60),
    }
}

fn cost_fixture() -> Raw {
    let t = NOW - 10 * DAY_MS;
    Raw {
        costs: vec![
            cost(
                t,
                "todo.attempt_finished",
                None,
                None,
                Some("20261001-000001-aaaaaa"),
                2.0,
            ),
            cost(
                t,
                "todo.attempt_finished",
                None,
                None,
                Some("20261001-000001-aaaaaa"),
                1.0,
            ),
            cost(t, "pr.reviewed", Some("acme/widget"), Some(12), None, 0.5),
            cost(t, "pr.reviewed", Some("acme/widget"), Some(12), None, 0.5),
            cost(t, "pr.reviewed", Some("acme/widget"), Some(99), None, 0.25),
        ],
        ..Raw::default()
    }
}

#[test]
fn cost_per_merged_change_uses_changes_with_recorded_cost() {
    let r = report(&cost_fixture(), &outcome_fixture(), 30);
    let m = &r["cost_per_merged_change"];
    // A: 3.0 by todo; B: 1.0 by its pull request; C and D have none.
    assert_eq!(m["value"], json!(2.0));
    assert_eq!(m["sample_size"], 2);
    assert_eq!(m["detail"]["changes_without_recorded_cost"], 2);
    assert_eq!(m["detail"]["unattributed_usd"], json!(0.25));
}

#[test]
fn cost_per_successful_change_divides_by_successful_changes_only() {
    let r = report(&cost_fixture(), &outcome_fixture(), 30);
    assert_eq!(r["cost_per_successful_change"]["value"], json!(3.0));
    assert_eq!(r["cost_per_successful_change"]["sample_size"], 1);
}

#[test]
fn review_cost_is_spend_per_reviewed_change() {
    let r = report(&cost_fixture(), &[], 30);
    let m = &r["review_cost"];
    assert_eq!(m["value"], json!(0.625));
    assert_eq!(m["detail"]["reviews"], 3);
    assert_eq!(m["detail"]["reviewed_changes"], 2);
    assert_eq!(m["detail"]["total_usd"], json!(1.25));
    assert_eq!(m["detail"]["total_seconds"], 180);
}

#[test]
fn repo_filter_narrows_flow_and_events() {
    let mut raw = flow_fixture();
    for row in raw.flow.iter_mut() {
        row.repo = Some("globex/api".to_string());
    }
    let r = report_for(&raw, &[], Some("acme/widget"), 7);
    assert_eq!(r["lead_time_p50"]["value"], Value::Null);
    let r = report_for(&raw, &[], Some("globex/api"), 7);
    assert_eq!(r["lead_time_p50"]["sample_size"], 3);
}

#[test]
fn an_empty_ledger_makes_every_tile_unknown_never_zero() {
    let r = report(&Raw::default(), &[], 7);
    let tiles = r.as_object().unwrap();
    assert_eq!(tiles.len(), 37);
    for (name, tile) in tiles {
        assert_eq!(tile["value"], Value::Null, "{name}");
        assert_eq!(tile["sample_size"], 0, "{name}");
        assert!(tile["unknown"]["reason"].is_string(), "{name}");
        assert_eq!(tile["daily"].as_array().unwrap().len(), 7, "{name}");
        assert!(
            tile["daily"].as_array().unwrap().iter().all(Value::is_null),
            "{name}"
        );
    }
}

#[test]
fn only_supported_windows_are_accepted() {
    let ok = ScorecardQuery {
        window: Some("30d".into()),
        ..Default::default()
    };
    let bad = ScorecardQuery {
        window: Some("14d".into()),
        ..Default::default()
    };
    assert!(ok.check_values().is_ok());
    assert!(bad.check_values().is_err());
}

#[test]
fn read_from_the_event_store_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let store = EventStore::open(&dir.path().join("shift.sqlite")).unwrap();
    let ev = |kind: &str, outcome: Option<&str>| NewEvent {
        source: "todoq".to_string(),
        kind: kind.to_string(),
        summary: kind.to_string(),
        family: Some("acme".to_string()),
        todo_id: Some("20261001-000001-aaaaaa".to_string()),
        repo: (kind != "todo.filed").then(|| "acme/widget".to_string()),
        outcome: outcome.map(ToString::to_string),
        detail: Some(json!({"release": "w-1"})),
        ..NewEvent::default()
    };
    let start = NOW - 3 * DAY_MS;
    for (i, (kind, outcome)) in [
        ("todo.filed", None),
        ("todo.claimed", None),
        ("pr.merged", None),
        ("release.staged", None),
        ("deploy.status", Some("success")),
    ]
    .into_iter()
    .enumerate()
    {
        store
            .insert("node-a", &ev(kind, outcome), start + i as i64 * H)
            .unwrap();
    }
    let raw = store.scorecard_raw(Some("acme"), NOW, 7).unwrap();
    let changes = store.outcome_changes(Some("acme"), None).unwrap();
    let r = report(&raw, &changes, 7);
    assert_eq!(r["lead_time_p50"]["value"], json!(4 * 3600));
    assert_eq!(
        r["releases_per_day"]["value"]["deploys"]["acme/widget"],
        json!(0.333)
    );
    assert_eq!(r["completed_work_items_per_day"]["sample_size"], 1);
    let other = store.scorecard_raw(Some("globex"), NOW, 7).unwrap();
    assert!(other.flow.is_empty() && other.events.is_empty());
}
