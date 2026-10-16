//! The digest's reading of a Scorecard report, on invented data (family
//! acme, repository acme/widget).
//!
//! The first tests compute a real report with [`super::super::compute`], so a
//! tile the digest reads by name has to still be there and still carry
//! `value`, `previous` and the `detail` counts it reads; the rest drive the
//! shapes a report can answer with, including unknown tiles.

use serde_json::{Map, Value, json};

use super::{Digest, amount, build, phrase};
use crate::web::flow::Station;
use crate::web::outcomes::project::{Change, Leg};
use crate::web::scorecard::Report;
use crate::web::scorecard::compute::{Ctx, DAY_MS, FlowRow, Raw, metrics};

const NOW: i64 = 1_760_000_000_000;
const DAYS: usize = 7;

/// A report over `raw` and `changes` for the 7-day window the digest reads.
fn computed(raw: &Raw, changes: &[Change]) -> Report {
    let ctx = Ctx::new(raw, changes, None, NOW, DAYS);
    Report {
        window: "7d".to_string(),
        generated_at: "2026-10-16T00:00:00Z".to_string(),
        family: None,
        repo: None,
        metrics: metrics(&ctx)
            .into_iter()
            .map(|(name, metric)| (name.to_string(), serde_json::to_value(metric).unwrap()))
            .collect(),
    }
}

/// A report whose tiles are written out: the shapes
/// `GET /api/v1/scorecard` answers with, without the ledgers behind them.
fn report_of(metrics: Value) -> Report {
    Report {
        window: "7d".to_string(),
        generated_at: "2026-10-16T00:00:00Z".to_string(),
        family: Some("acme".to_string()),
        repo: None,
        metrics: match metrics {
            Value::Object(map) => map,
            _ => Map::new(),
        },
    }
}

/// A change merged, shipped and, when `reverted`, reverted straight away.
fn change(lot: &str, at: i64, reverted: bool) -> Change {
    Change {
        lot_id: lot.to_string(),
        family: Some("acme".to_string()),
        todo_id: Some(lot.to_string()),
        first_seen_ms: NOW - 30 * DAY_MS,
        filed_ms: Some(at - DAY_MS),
        done_ms: Some(at),
        agent: Some("claude".to_string()),
        model: Some("opus".to_string()),
        reverted_ms: reverted.then_some(at + 1000),
        legs: vec![Leg {
            repo: "acme/widget".to_string(),
            pr: Some(lot.len() as i64),
            merged_ms: Some(at),
            deployed_ms: Some(at + 60_000),
            ..Leg::default()
        }],
        ..Change::default()
    }
}

fn line<'a>(digest: &'a Digest, metric: &str) -> &'a super::Line {
    digest
        .headline
        .iter()
        .find(|line| line.metric == metric)
        .unwrap_or_else(|| panic!("{metric} is not in the headline"))
}

/// Every headline number is the tile's own `value` and `previous`, and the
/// change between them is the difference the Scorecard already implies.
#[test]
fn the_headline_reads_the_tiles_value_and_previous() {
    // Two changes merged and reverted this week, one merged and reverted the
    // week before: the failure rate stays 100% and the merge rate rises.
    let changes = vec![
        change("a", NOW - 2 * DAY_MS, true),
        change("bb", NOW - 3 * DAY_MS, true),
        change("ccc", NOW - 9 * DAY_MS, true),
    ];
    let digest = build(&computed(&Raw::default(), &changes));
    let failure = line(&digest, "change_failure_rate");
    assert_eq!(failure.value, Some(100.0));
    assert_eq!(failure.previous, Some(100.0));
    assert_eq!(failure.change, Some(0.0));
    assert_eq!(failure.direction, "flat");
    assert_eq!(failure.sample_size, 2);
    // Nothing is traded: the guardrail did not get worse.
    assert!(digest.warnings.is_empty(), "{:?}", digest.warnings);
    // The window and the report's scope travel with the numbers.
    assert_eq!(digest.window, "7d");
    assert_eq!(digest.schema_version, super::DIGEST_SCHEMA);
}

/// A guardrail that got worse while the pull requests merged rose is called
/// out, with both weeks of both numbers.
#[test]
fn a_guardrail_worse_on_rising_volume_is_called_out() {
    // One change merged and reverted the week before; three merged this week,
    // two of them reverted. The failure rate falls, so the rate that gets
    // worse here is the false-done rate: three todos reported done, two of
    // them reverted, against one of one the week before.
    let mut changes = vec![
        change("a", NOW - 2 * DAY_MS, true),
        change("bb", NOW - 2 * DAY_MS, true),
        change("ccc", NOW - 3 * DAY_MS, false),
        change("dddd", NOW - 9 * DAY_MS, false),
    ];
    // A change with no failure is still observed once it was reworked.
    changes[2].reworked_by = vec!["later".to_string()];
    // The week before: one todo reported done and nothing wrong with it since,
    // which is only known for a change past the 14-day observation.
    changes[3].legs[0].deployed_ms = Some(NOW - 20 * DAY_MS);
    let digest = build(&computed(&Raw::default(), &changes));
    let worse = line(&digest, "false_done_rate");
    assert_eq!(worse.direction, "worse");
    let warning = digest
        .warnings
        .iter()
        .find(|warning| warning.guardrail == "false_done_rate")
        .expect("the false-done rate is called out");
    assert_eq!(warning.volume, "completed_work_items_per_day");
    assert!(
        warning.volume_value > warning.volume_previous,
        "{warning:?}"
    );
    assert_eq!(warning.value, worse.value.unwrap());
    assert_eq!(warning.previous, worse.previous.unwrap());
    // The arms behind that rate come with it.
    assert_eq!(digest.false_done_by_arm.len(), 1);
    assert_eq!(digest.false_done_by_arm[0].arm, "claude:opus");
    assert_eq!(digest.false_done_by_arm[0].denominator, 3);
    assert_eq!(digest.false_done_by_arm[0].rate_pct, 100.0);
    assert_eq!(digest.false_done_arms_omitted, 0);
}

/// A guardrail that got worse while the volume fell is not a trade-off, and
/// says nothing.
#[test]
fn a_guardrail_worse_on_falling_volume_is_not_called_out() {
    let digest = build(&report_of(json!({
        "change_failure_rate": {
            "value": 20.0, "previous": 10.0, "daily": [], "sample_size": 10,
            "source": "outcome_ledger", "detail": null, "unknown": null,
        },
        "prs_merged_per_day": {
            "value": 1.0, "previous": 4.0, "daily": [], "sample_size": 7,
            "source": "outcome_ledger", "detail": null, "unknown": null,
        },
    })));
    assert_eq!(line(&digest, "change_failure_rate").direction, "worse");
    assert!(digest.warnings.is_empty(), "{:?}", digest.warnings);
}

/// A rate where a higher number is better reads the other way round.
#[test]
fn a_rate_that_should_rise_is_worse_when_it_falls() {
    let digest = build(&report_of(json!({
        "review_catch_rate": {
            "value": 40.0, "previous": 80.0, "daily": [], "sample_size": 10,
            "source": "yield", "detail": null, "unknown": null,
        },
        "prs_merged_per_day": {
            "value": 5.0, "previous": 4.0, "daily": [], "sample_size": 7,
            "source": "outcome_ledger", "detail": null, "unknown": null,
        },
    })));
    let catch = line(&digest, "review_catch_rate");
    assert_eq!(catch.direction, "worse");
    assert_eq!(catch.change, Some(-40.0));
    assert_eq!(catch.change_pct, Some(-50.0));
    assert_eq!(digest.warnings.len(), 1);
    assert_eq!(digest.warnings[0].guardrail, "review_catch_rate");
}

/// An unknown tile stays unknown, with the Scorecard's own reason, and is
/// never filled in or read as a zero.
#[test]
fn an_unknown_tile_passes_its_reason_through() {
    let digest = build(&computed(&Raw::default(), &[]));
    for metric in ["review_catch_rate", "time_to_restore"] {
        let line = line(&digest, metric);
        assert_eq!(line.value, None);
        assert_eq!(line.change, None);
        assert_eq!(line.direction, "unknown");
        assert_eq!(
            line.unknown.as_deref(),
            Some("source_not_landed: review finding outcomes")
                .filter(|_| metric == "review_catch_rate")
                .or(Some("source_not_landed: incident ledger"))
        );
        assert!(phrase(line).contains("unknown"), "{}", phrase(line));
    }
    assert!(digest.warnings.is_empty());
    assert_eq!(digest.oldest_unowned_hold, None);
}

/// The oldest hold nobody has acted on, from the hold tile's age buckets.
#[test]
fn the_oldest_unowned_hold_is_the_oldest_bucket_with_one() {
    let hold = |lot: &str, days: i64| FlowRow {
        lot_id: lot.to_string(),
        repo: Some("acme/widget".to_string()),
        station: Station::Review,
        entered_ms: NOW - days * DAY_MS,
        started_ms: None,
        left_ms: None,
        direction: "hold".to_string(),
    };
    let raw = Raw {
        flow: vec![hold("a", 10), hold("bb", 2)],
        flow_from: Some(NOW - 30 * DAY_MS),
        ..Raw::default()
    };
    let digest = build(&computed(&raw, &[]));
    let oldest = digest.oldest_unowned_hold.expect("a standing hold");
    assert_eq!(oldest.age, "over_7d");
    assert_eq!(oldest.unowned, 1);
    assert_eq!(oldest.held, 1);
}

/// A bucket whose holds have all been acted on is not the oldest unowned one.
#[test]
fn a_hold_somebody_acted_on_is_not_unowned() {
    let digest = build(&report_of(json!({
        "holds_by_age_and_action": {
            "value": 2, "previous": null, "daily": [], "sample_size": 2,
            "source": "flow_ledger",
            "detail": {"acting": 1, "not_acting": 1, "by_age": [
                {"age": "under_1d", "held": 1, "acting": 0},
                {"age": "over_7d", "held": 1, "acting": 1},
            ]},
            "unknown": null,
        },
    })));
    let oldest = digest.oldest_unowned_hold.expect("the younger bucket");
    assert_eq!(oldest.age, "under_1d");
    assert_eq!(oldest.unowned, 1);
}

/// The busiest arms are named and the rest counted: the payload is stored as
/// one event detail, so the list cannot grow with the arms a deployment runs.
#[test]
fn the_arm_list_is_bounded_and_busiest_first() {
    let arms: Map<String, Value> = (0..super::MAX_ARMS + 3)
        .map(|index| {
            (
                format!("claude:model-{index:02}"),
                json!({"numerator": 1, "denominator": index + 1}),
            )
        })
        .collect();
    let digest = build(&report_of(json!({
        "false_done_rate": {
            "value": 10.0, "previous": 10.0, "daily": [], "sample_size": 100,
            "source": "outcome_ledger",
            "detail": {"numerator": 10, "denominator": 100, "pending": 0, "by_arm": arms},
            "unknown": null,
        },
    })));
    assert_eq!(digest.false_done_by_arm.len(), super::MAX_ARMS);
    assert_eq!(digest.false_done_arms_omitted, 3);
    assert_eq!(digest.false_done_by_arm[0].arm, "claude:model-14");
    assert!(
        digest.false_done_by_arm[0].denominator > digest.false_done_by_arm[1].denominator,
        "{:?}",
        digest.false_done_by_arm
    );
    assert!(serde_json::to_string(&digest).unwrap().len() < 8 * 1024);
}

/// The digest survives a round trip through the event detail it is stored as,
/// which is how the attention inbox reads it back.
#[test]
fn a_stored_digest_reads_back_the_same() {
    let digest = build(&computed(
        &Raw::default(),
        &[change("a", NOW - DAY_MS, true)],
    ));
    let stored = serde_json::to_value(&digest).unwrap();
    let read: Digest = serde_json::from_value(stored).unwrap();
    assert_eq!(read, digest);
}

/// A payload written before a field existed still reads: the inbox shows the
/// digest a server of the week before recorded.
#[test]
fn a_digest_without_the_newer_fields_still_reads() {
    let read: Digest = serde_json::from_value(json!({
        "schema_version": "jeryu.scorecard.digest/v1",
        "generated_at": "2026-10-09T00:00:00Z",
        "window": "7d",
    }))
    .unwrap();
    assert!(read.headline.is_empty());
    assert_eq!(read.false_done_arms_omitted, 0);
}

#[test]
fn an_amount_is_spelled_with_its_unit() {
    assert_eq!(amount(12.5, "percent"), "12.5%");
    assert_eq!(amount(3.5, "usd"), "$3.50");
    assert_eq!(amount(12_000.0, "seconds"), "3h 20m");
    assert_eq!(amount(90.0, "seconds"), "1m");
    assert_eq!(amount(9.0, "seconds"), "9s");
    assert_eq!(amount(2.0, "changes"), "2");
}
