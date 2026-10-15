//! Human minutes per change: marks through the ledger, outcomes folded from
//! real events, and the report over both. Data is invented (families acme and
//! globex, repositories acme/widget and globex/parts, people dana and ilse).

use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::params;
use serde_json::json;

use super::{
    By, HumanMinutesReport, IDLE_GAP_MS, MARK_FLOOR_MS, Mark, Source, Window, parse_commits,
    pull_lot, report,
};
use crate::web::pipeline::{EventStore, NewEvent};

const T0: i64 = 1_760_000_000_000; // 2025-10-09T08:53:20Z
const MIN: i64 = 60_000;
const REPO: &str = "acme/widget";

fn store() -> (tempfile::TempDir, EventStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = EventStore::open(&dir.path().join("shift.sqlite")).unwrap();
    (dir, store)
}

/// A mark on `acme/widget#7` by `dana`, `minutes` after T0.
fn mark(store: &EventStore, source: Source, actor: &str, minutes: i64) {
    store
        .charge_human_minutes(
            &Mark::new(source, actor, T0 + minutes * MIN)
                .on_pull(REPO, 7)
                .in_family(Some("acme")),
        )
        .unwrap();
}

fn post(store: &EventStore, event: NewEvent, minutes: i64) {
    store.insert("forge", &event, T0 + minutes * MIN).unwrap();
}

fn merged(store: &EventStore, repo: &str, pr: i64, minutes: i64) {
    post(
        store,
        NewEvent {
            family: Some(repo.split('/').next().unwrap().to_string()),
            repo: Some(repo.to_string()),
            pr: Some(pr),
            event_id: Some(format!("merged-{repo}-{pr}")),
            ..NewEvent::forge("pr.merged", format!("{repo}#{pr} merged"))
        },
        minutes,
    );
}

/// The whole window around T0, so every charge above is inside it.
fn whole_window() -> Window {
    Window {
        since: NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
        until: NaiveDate::from_ymd_opt(2026, 12, 31).unwrap(),
    }
}

fn read(store: &EventStore, family: Option<&str>, by: By) -> HumanMinutesReport {
    read_window(store, family, by, whole_window())
}

fn read_window(
    store: &EventStore,
    family: Option<&str>,
    by: By,
    window: Window,
) -> HumanMinutesReport {
    let now = DateTime::<Utc>::from_timestamp_millis(T0).unwrap();
    store
        .with_conn(|conn| report(conn, family.map(str::to_string), window, by, now))
        .unwrap()
}

fn group<'a>(report: &'a HumanMinutesReport, name: &str) -> &'a super::Row {
    &report
        .groups
        .iter()
        .find(|row| row.group == name)
        .unwrap_or_else(|| panic!("group {name} in {:?}", report.groups))
        .row
}

#[test]
fn a_gap_inside_the_idle_cap_is_the_time_the_review_took() {
    let (_dir, store) = store();
    // Opening the files view, then submitting seven minutes later.
    mark(&store, Source::Review, "dana", 0);
    mark(&store, Source::Review, "dana", 7);
    let report = read(&store, Some("acme"), By::Family);
    assert_eq!(report.totals.review_minutes, 8.0, "one floor plus the gap");
    assert_eq!(report.totals.minutes, 8.0);
}

#[test]
fn a_gap_past_the_idle_cap_opens_a_new_session_instead_of_charging_the_wait() {
    let (_dir, store) = store();
    mark(&store, Source::Review, "dana", 0);
    // Three hours later: lunch, not review.
    mark(&store, Source::Review, "dana", 180);
    let report = read(&store, Some("acme"), By::Family);
    assert_eq!(report.totals.review_minutes, 2.0, "two session floors");
}

#[test]
fn a_gap_is_measured_per_person_so_two_reviewers_are_two_sessions() {
    let (_dir, store) = store();
    mark(&store, Source::Review, "dana", 0);
    mark(&store, Source::Review, "ilse", 4);
    mark(&store, Source::Review, "dana", 6);
    let report = read(&store, Some("acme"), By::Family);
    // dana: floor + six; ilse: floor.
    assert_eq!(report.totals.review_minutes, 8.0);
    assert!(
        serde_json::to_string(&report)
            .unwrap()
            .find("dana")
            .is_none(),
        "no answer ever names a person"
    );
}

#[test]
fn a_mark_that_does_not_move_forward_is_not_more_time() {
    let (_dir, store) = store();
    mark(&store, Source::Commit, "dana", 10);
    // The same commit, pushed again to a second branch.
    mark(&store, Source::Commit, "dana", 10);
    mark(&store, Source::Commit, "dana", 5);
    let report = read(&store, Some("acme"), By::Family);
    assert_eq!(report.totals.commit_minutes, 1.0);
}

#[test]
fn each_source_is_labelled_and_summed_on_its_own() {
    let (_dir, store) = store();
    mark(&store, Source::Review, "dana", 0);
    mark(&store, Source::Review, "dana", 5);
    mark(&store, Source::Commit, "dana", 20);
    mark(&store, Source::Session, "dana", 30);
    mark(&store, Source::Triage, "dana", 40);
    let report = read(&store, Some("acme"), By::Family);
    assert_eq!(report.totals.review_minutes, 6.0);
    assert_eq!(report.totals.commit_minutes, 1.0);
    assert_eq!(report.totals.session_minutes, 1.0);
    assert_eq!(report.totals.triage_minutes, 1.0);
    assert_eq!(report.totals.minutes, 9.0);
    let labelled: Vec<_> = report
        .sources
        .iter()
        .map(|row| (row.source, row.minutes))
        .collect();
    assert_eq!(
        labelled,
        vec![
            ("review", 6.0),
            ("commit", 1.0),
            ("session", 1.0),
            ("triage", 1.0)
        ]
    );
    assert!(report.sources.iter().all(|row| !row.label.is_empty()));
    assert!(!report.not_counted.is_empty(), "what is left out is said");
}

#[test]
fn minutes_are_divided_by_the_changes_that_succeeded_only() {
    let (_dir, store) = store();
    // Two changes land; one of them is reverted and charged as a defect.
    mark(&store, Source::Review, "dana", 0);
    mark(&store, Source::Review, "dana", 9);
    merged(&store, REPO, 7, 10);
    store
        .charge_human_minutes(
            &Mark::new(Source::Review, "dana", T0 + 20 * MIN)
                .on_pull(REPO, 8)
                .in_family(Some("acme")),
        )
        .unwrap();
    merged(&store, REPO, 8, 21);
    post(
        &store,
        NewEvent {
            family: Some("acme".to_string()),
            repo: Some(REPO.to_string()),
            pr: Some(8),
            detail: Some(json!({"cause": "revert"})),
            ..NewEvent::forge("defect.linked", "acme/widget#8 was reverted")
        },
        30,
    );
    let report = read(&store, Some("acme"), By::Family);
    assert_eq!(report.totals.changes, 2);
    assert_eq!(report.totals.successful_changes, 1);
    assert_eq!(report.totals.failed_changes, 1);
    // 10 + 1 minutes over the one change that stuck.
    assert_eq!(report.totals.minutes, 11.0);
    assert_eq!(report.totals.minutes_per_successful_change, Some(11.0));
}

#[test]
fn a_change_nothing_landed_yet_has_no_rate_and_still_holds_its_minutes() {
    let (_dir, store) = store();
    mark(&store, Source::Review, "dana", 0);
    let report = read(&store, Some("acme"), By::Family);
    assert_eq!(report.totals.minutes, 1.0);
    assert_eq!(report.totals.changes, 0);
    assert_eq!(report.totals.minutes_per_successful_change, None);
}

#[test]
fn a_change_an_incident_blames_is_not_a_successful_change() {
    let (_dir, store) = store();
    merged(&store, REPO, 7, 1);
    post(
        &store,
        NewEvent {
            source: "alert-webhook".to_string(),
            family: Some("acme".to_string()),
            detail: Some(json!({"incident_id": "inc-1", "changes": [pull_lot(REPO, 7)]})),
            ..NewEvent::forge("incident.caused_by", "checkout broke")
        },
        5,
    );
    let report = read(&store, Some("acme"), By::Family);
    assert_eq!(report.totals.successful_changes, 0);
    assert_eq!(report.totals.failed_changes, 1);
}

#[test]
fn minutes_follow_the_lot_the_flow_ledger_keeps_the_pull_request_under() {
    let (_dir, store) = store();
    store
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO flow_ledger (lot_id, family, repo, pr, station, entered_ms,
                   direction, source_seq_first, source_seq_last)
                 VALUES ('20261013-1200-abc', 'acme', ?1, 7, 'review', ?2, 'open', 1, 1)",
                params![REPO, T0],
            )
            .map_err(|err| err.to_string())
        })
        .unwrap();
    mark(&store, Source::Review, "dana", 0);
    store
        .charge_human_minutes(
            &Mark::new(Source::Triage, "dana", T0 + MIN)
                .on_todo("20261013-1200-abc")
                .in_family(Some("acme")),
        )
        .unwrap();
    let lots: Vec<String> = store
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT DISTINCT lot_id FROM change_human_minutes")
                .map_err(|err| err.to_string())?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|err| err.to_string())?;
            rows.collect::<Result<_, _>>()
                .map_err(|err| err.to_string())
        })
        .unwrap();
    assert_eq!(lots, vec!["20261013-1200-abc".to_string()]);
}

#[test]
fn a_window_counts_the_minutes_and_the_changes_inside_it() {
    let (_dir, store) = store();
    mark(&store, Source::Review, "dana", 0);
    merged(&store, REPO, 7, 1);
    let day_after = NaiveDate::from_ymd_opt(2025, 10, 10).unwrap();
    let report = read_window(
        &store,
        Some("acme"),
        By::Family,
        Window {
            since: day_after,
            until: day_after,
        },
    );
    assert_eq!(
        report.totals.minutes, 0.0,
        "the minutes fell the day before"
    );
    assert_eq!(report.totals.changes, 0);
}

#[test]
fn a_family_filter_leaves_another_familys_minutes_out() {
    let (_dir, store) = store();
    mark(&store, Source::Review, "dana", 0);
    store
        .charge_human_minutes(
            &Mark::new(Source::Review, "ilse", T0)
                .on_pull("globex/parts", 3)
                .in_family(Some("globex")),
        )
        .unwrap();
    let acme = read(&store, Some("acme"), By::Family);
    assert_eq!(acme.totals.minutes, 1.0);
    assert_eq!(acme.groups.len(), 1);
    let both = read(&store, None, By::Family);
    assert_eq!(both.totals.minutes, 2.0);
    assert_eq!(group(&both, "acme").minutes, 1.0);
    assert_eq!(group(&both, "globex").minutes, 1.0);
}

#[test]
fn grouping_by_repo_and_by_work_kind_splits_the_same_minutes() {
    let (_dir, store) = store();
    mark(&store, Source::Review, "dana", 0);
    // An auto-pin bump is a different kind of work from a change.
    post(
        &store,
        NewEvent {
            source: "auto-pin".to_string(),
            family: Some("acme".to_string()),
            repo: Some(REPO.to_string()),
            pr: Some(9),
            ..NewEvent::forge("pin.bump_opened", "bump jeryu-web")
        },
        1,
    );
    store
        .charge_human_minutes(
            &Mark::new(Source::Review, "dana", T0 + 2 * MIN)
                .on_pull(REPO, 9)
                .in_family(Some("acme")),
        )
        .unwrap();
    let by_repo = read(&store, Some("acme"), By::Repo);
    assert_eq!(group(&by_repo, REPO).minutes, 2.0);
    let by_kind = read(&store, Some("acme"), By::WorkKind);
    assert_eq!(group(&by_kind, "change").minutes, 1.0);
    assert_eq!(group(&by_kind, "pin_bump").minutes, 1.0);
}

#[test]
fn a_todo_filed_to_redo_a_change_is_rework_and_fails_the_change_it_redoes() {
    let (_dir, store) = store();
    post(
        &store,
        NewEvent {
            family: Some("acme".to_string()),
            repo: Some(REPO.to_string()),
            pr: Some(7),
            detail: Some(json!({"cause": "rework", "by": "20261013-1200-redo"})),
            ..NewEvent::forge("defect.linked", "acme/widget#7 is being redone")
        },
        1,
    );
    store
        .charge_human_minutes(
            &Mark::new(Source::Triage, "dana", T0 + 2 * MIN)
                .on_todo("20261013-1200-redo")
                .in_family(Some("acme")),
        )
        .unwrap();
    merged(&store, REPO, 7, 3);
    let by_kind = read(&store, Some("acme"), By::WorkKind);
    assert_eq!(group(&by_kind, "rework").minutes, 1.0);
    // The change that is being redone landed, and is failed.
    assert_eq!(by_kind.totals.failed_changes, 1);
    assert_eq!(by_kind.totals.successful_changes, 0);
}

#[test]
fn the_charge_rules_are_the_cap_and_the_floor() {
    assert_eq!(super::charged_ms(None, T0), MARK_FLOOR_MS);
    assert_eq!(super::charged_ms(Some(T0), T0 + 5 * MIN), 5 * MIN);
    assert_eq!(super::charged_ms(Some(T0), T0 + 600 * MIN), MARK_FLOOR_MS);
    assert_eq!(super::charged_ms(Some(T0), T0 + IDLE_GAP_MS), IDLE_GAP_MS);
    assert_eq!(super::charged_ms(Some(T0), T0 + 10), 10);
    assert_eq!(super::charged_ms(Some(T0), T0), 0);
    assert_eq!(super::charged_ms(Some(T0), T0 - 1), 0);
}

#[test]
fn commits_are_read_as_an_address_and_an_author_time() {
    let out = "Dana@Example.com\u{1f}1760000000\nbot@ci\u{1f}1760000600\n\u{1f}1760000700\n";
    assert_eq!(
        parse_commits(out),
        vec![
            super::PushedCommit {
                email: "dana@example.com".to_string(),
                at_ms: 1_760_000_000_000,
            },
            super::PushedCommit {
                email: "bot@ci".to_string(),
                at_ms: 1_760_000_600_000,
            },
        ]
    );
}

#[test]
fn a_presence_link_names_a_pull_request_or_a_todo() {
    assert_eq!(
        super::link_pull("acme/widget#12"),
        Some(("acme/widget".to_string(), 12))
    );
    assert_eq!(super::link_pull("acme/widget"), None);
    assert_eq!(super::link_pull("acme/widget#0"), None);
    assert_eq!(
        super::link_todo("acme:20261012-1200-abc123"),
        Some("20261012-1200-abc123".to_string())
    );
    assert_eq!(super::link_todo("  "), None);
}

#[test]
fn a_mark_nothing_can_be_attributed_to_is_dropped() {
    let (_dir, store) = store();
    store
        .charge_human_minutes(&Mark::new(Source::Triage, "dana", T0))
        .unwrap();
    store
        .charge_human_minutes(&Mark::new(Source::Triage, "   ", T0).on_pull(REPO, 7))
        .unwrap();
    let report = read(&store, None, By::Family);
    assert_eq!(report.totals.minutes, 0.0);
    assert!(report.groups.is_empty());
}

#[test]
fn frequent_marks_charge_elapsed_time_after_one_session_estimate() {
    let (_dir, store) = store();
    for second in 0..600 {
        store
            .charge_human_minutes(
                &Mark::new(Source::Review, "dana", T0 + second * 1_000).on_pull(REPO, 7),
            )
            .unwrap();
    }
    let report = read(&store, None, By::Repo);
    // The report publishes tenths of a minute; 659 seconds rounds to 11.0.
    let expected = 11.0;
    assert!((report.totals.review_minutes - expected).abs() < 0.000_001);
}
