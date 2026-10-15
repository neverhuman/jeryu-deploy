use serde_json::{Value, json};

use super::*;

fn event(seq: i64, ts: &str, kind: &str, pr: i64) -> Event {
    Event {
        seq,
        ts: ts.to_string(),
        event_id: None,
        source: "forge".to_string(),
        kind: kind.to_string(),
        reporter: "forge".to_string(),
        actor: None,
        family: Some("acme".to_string()),
        family_label: Some("acme".to_string()),
        repo: Some("acme/widgets".to_string()),
        pr: Some(pr),
        sha: Some("aaa".to_string()),
        todo_id: None,
        shift: None,
        outcome: None,
        needs_human: false,
        summary: String::new(),
        reason: None,
        cost_usd: None,
        seconds: None,
        log_tail: None,
        log_url: None,
        detail: Some(json!({"reviewer_kind": HUMAN})),
    }
}

fn with(mut e: Event, outcome: Option<&str>, detail: Value) -> Event {
    e.outcome = outcome.map(str::to_string);
    e.detail = Some(detail);
    e
}

#[test]
fn a_touched_file_is_caught_and_an_untouched_one_is_not_acted() {
    let finding = |path: Option<&str>| Finding {
        key: "k".into(),
        repo: "acme/widgets".into(),
        pr: 1,
        sha: Some("aaa".into()),
        reviewer_kind: HUMAN.into(),
        path: path.map(str::to_string),
    };
    let touched = vec!["src/a.rs".to_string()];
    assert_eq!(
        verdict_for(&finding(Some("src/a.rs")), "bbb", &touched).0,
        "caught"
    );
    assert_eq!(
        verdict_for(&finding(Some("src/b.rs")), "bbb", &touched).0,
        "not_acted"
    );
    assert_eq!(verdict_for(&finding(None), "bbb", &[]).0, "caught");
    assert_eq!(verdict_for(&finding(None), "aaa", &[]).0, "not_acted");
}

#[test]
fn catch_rate_counts_escapes_in_the_denominator_by_reviewer_kind() {
    let mut events = Vec::new();
    // Agent: two findings on PR 1, one caught (reviewer-resolved), one open.
    for (seq, id) in [(1, "f1"), (2, "f2")] {
        let mut e = with(
            event(seq, "2026-10-01T10:00:00Z", FINDING_KIND, 1),
            Some("high"),
            json!({"reviewer_kind": "agent", "path": "src/a.rs"}),
        );
        e.event_id = Some(id.to_string());
        e.source = "pr-redteam".into();
        events.push(e);
    }
    events.push(with(
        event(3, "2026-10-01T11:00:00Z", RESOLVED_KIND, 1),
        Some("caught"),
        json!({"finding": "f1"}),
    ));
    // Human: one finding on PR 2 never acted on.
    events.push(with(
        event(4, "2026-10-01T10:00:00Z", FINDING_KIND, 2),
        Some("blocking"),
        json!({"reviewer_kind": "human"}),
    ));
    events.push(with(
        event(5, "2026-10-02T00:00:00Z", RESOLVED_KIND, 2),
        Some("not_acted"),
        json!({"finding": "seq-4"}),
    ));
    // PR 3 passed agent review, then was reverted: escaped past the agent.
    let mut passed = event(6, "2026-10-01T10:00:00Z", "pr.reviewed", 3);
    passed.outcome = Some("approve".into());
    events.push(passed);
    events.push(event(7, "2026-10-03T00:00:00Z", DEFECT_KIND, 3));
    // A defect on a change that never passed review is not an escape.
    events.push(event(8, "2026-10-03T00:00:00Z", DEFECT_KIND, 4));

    let report = report(&events, None);
    assert_eq!(report.agent.findings, 2);
    assert_eq!(
        (report.agent.caught, report.agent.open, report.agent.escaped),
        (1, 1, 1)
    );
    assert_eq!(report.agent.catch_rate, Some(0.5));
    assert_eq!((report.human.findings, report.human.not_acted), (1, 1));
    assert_eq!(report.human.catch_rate, None);
    assert_eq!(report.total.caught, 1);
    assert_eq!(report.total.escaped, 1);
}

#[test]
fn review_time_runs_from_ready_to_the_first_review_of_each_kind() {
    let mut human = event(3, "2026-10-01T10:30:00Z", "pr.review", 1);
    human.outcome = Some("request_changes".into());
    let mut agent = event(2, "2026-10-01T10:05:00Z", "pr.reviewed", 1);
    agent.outcome = Some("block".into());
    let mut later_human = event(4, "2026-10-01T12:00:00Z", "pr.review", 1);
    later_human.outcome = Some("approve".into());
    let events = vec![
        event(1, "2026-10-01T10:00:00Z", "pr.ready_for_review", 1),
        agent,
        human,
        later_human,
    ];
    let report = report(&events, None);
    assert_eq!(report.agent.review_seconds_median, Some(300.0));
    assert_eq!(report.human.review_seconds_median, Some(1800.0));
    assert_eq!(report.human.reviews_timed, 1);
}

#[test]
fn unavailable_event_history_is_an_error_instead_of_no_findings() {
    let dir = tempfile::tempdir().unwrap();
    let store = EventStore::open(&dir.path().join("events.sqlite")).unwrap();
    assert!(
        pr_events(&store, FINDING_KIND, "acme/widgets", 1)
            .unwrap()
            .is_empty()
    );
    store
        .with_conn(|conn| {
            conn.execute_batch("DROP TABLE pipeline_events")
                .map_err(|err| err.to_string())
        })
        .unwrap();
    let error = pr_events(&store, FINDING_KIND, "acme/widgets", 1).unwrap_err();
    assert!(error.contains("pipeline_events"), "{error}");
}

#[test]
fn an_out_of_range_pr_cannot_broaden_the_event_query() {
    let dir = tempfile::tempdir().unwrap();
    let store = EventStore::open(&dir.path().join("events.sqlite")).unwrap();
    assert!(pr_events(&store, FINDING_KIND, "acme/widgets", u64::MAX).is_err());
}

#[test]
fn review_diff_revisions_reject_options_and_symbolic_inputs() {
    for oid in [
        "abcdef0",
        "0123456789abcdef0123456789abcdef01234567",
        &"a".repeat(64),
    ] {
        assert!(is_commit_oid(oid), "valid commit identity: {oid}");
    }
    for input in [
        "--output=/tmp/review-catch",
        "HEAD",
        "abcdef",
        "abc-def0",
        "abcdef0:path",
        &"a".repeat(65),
    ] {
        assert!(!is_commit_oid(input), "refuse revision syntax: {input}");
    }
}

#[test]
fn a_service_route_approval_and_report_count_one_agent_escape() {
    let mut route = event(1, "2026-10-01T10:00:00Z", "pr.review", 1);
    route.actor = Some("review-service".into());
    route.detail = Some(json!({"reviewer_kind": AGENT}));
    route.outcome = Some("approve".into());
    let mut report_event = route.clone();
    report_event.seq = 2;
    report_event.kind = "pr.reviewed".into();
    let defect = event(3, "2026-10-01T12:00:00Z", DEFECT_KIND, 1);
    let measured = report(&[route, report_event, defect], None);
    assert_eq!(measured.agent.escaped, 1);
    assert_eq!(measured.human.escaped, 0);
    assert_eq!(measured.total.escaped, 1);
}

#[test]
fn an_unclassified_historical_review_is_not_assumed_human() {
    let mut review = event(1, "2026-10-01T10:00:00Z", "pr.review", 1);
    review.detail = None;
    review.outcome = Some("approve".into());
    let defect = event(2, "2026-10-01T12:00:00Z", DEFECT_KIND, 1);
    assert_eq!(report(&[review, defect], None).total.escaped, 0);
}

#[test]
fn revert_declarations_are_explicit_and_bounded() {
    assert_eq!(
        reverted_pr_numbers(
            "Reverts #12.\nSee also #99 and #100\nReverts #12\nReverts #0\nReverts #13, see #14"
        ),
        BTreeSet::from([12])
    );
    assert_eq!(
        reverted_pr_numbers(
            &(1..100_000)
                .map(|n| format!("Reverts #{n}\n"))
                .collect::<String>()
        )
        .len(),
        50
    );
    assert!(reverted_pr_numbers(&format!("{}Reverts #9", "prose\n".repeat(1_000))).is_empty());
}
