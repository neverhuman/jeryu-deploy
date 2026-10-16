//! The weekly Scorecard digest in the inbox: one row a week carrying the
//! headline numbers and their week-over-week change.
//!
//! The digest is not state the forge can recompute from what is true now: it
//! is what the numbers were at the end of a week. So, like
//! `shift_budget_spent`, this rule reads the event the weekly job recorded
//! (`scorecard.digest`, see [`crate::web::scorecard::weekly`]) and carries its
//! payload on the row's `digest` field, so a surface shows the numbers without
//! parsing `reason`.
//!
//! The row is `action` when a guardrail got worse while the volume metric it
//! is paired with rose -- somebody has to decide whether that trade is wanted
//! -- and `watch` otherwise, because a week's numbers are to be read, not
//! fixed. It leaves the inbox when the digest is older than a week and a day,
//! by when the next one has replaced it.

use chrono::{DateTime, Duration, Utc};

use super::super::types::Event;
use super::{Draft, Item, Severity, parse_time};
use crate::web::scorecard::digest::{Digest, phrase};

/// Where a reader goes to read the week's numbers themselves.
const HREF: &str = "/scorecard";

/// A digest older than this is last week's: the next one has been recorded,
/// or the job has stopped, and either way the row is no longer this week's
/// reading.
fn stale_after() -> Duration {
    Duration::days(8)
}

/// The inbox row for the newest recorded digest, if there is a recent one.
/// `events` is the newest `scorecard.digest` first, as the event log returns
/// it; an event whose `detail` is not a digest payload is skipped rather than
/// shown as an empty row.
pub(crate) fn digest_items(events: &[Event], now: DateTime<Utc>) -> Vec<Item> {
    let Some((event, digest)) = events.iter().find_map(|event| {
        let digest: Digest =
            serde_json::from_value(event.detail.clone().unwrap_or(serde_json::Value::Null)).ok()?;
        Some((event, digest))
    }) else {
        return Vec::new();
    };
    let Some(at) = parse_time(&event.ts) else {
        return Vec::new();
    };
    if now - at > stale_after() {
        return Vec::new();
    }
    let worse = digest.warnings.len();
    let headline = digest
        .headline
        .iter()
        .map(phrase)
        .collect::<Vec<_>>()
        .join("; ");
    let traded = digest
        .warnings
        .iter()
        .map(|warning| {
            format!(
                "{} went from {} to {} while {} rose from {} to {}",
                warning.guardrail_label,
                crate::web::scorecard::digest::amount(warning.previous, &warning.unit),
                crate::web::scorecard::digest::amount(warning.value, &warning.unit),
                warning.volume_label,
                warning.volume_previous,
                warning.volume_value,
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let mut reason = format!(
        "The {} window against the week before it: {headline}.",
        digest.window
    );
    if let Some(hold) = &digest.oldest_unowned_hold {
        reason.push_str(&format!(
            " Oldest hold nobody has acted on since: {} ({} of {} holds in that bucket).",
            hold.age, hold.unowned, hold.held
        ));
    }
    if !digest.false_done_by_arm.is_empty() {
        let arms = digest
            .false_done_by_arm
            .iter()
            .map(|arm| format!("{} {}% of {}", arm.arm, arm.rate_pct, arm.denominator))
            .collect::<Vec<_>>()
            .join("; ");
        reason.push_str(&format!(" False-done rate by arm: {arms}."));
    }
    if worse > 0 {
        reason.push_str(&format!(
            " Worse on rising volume: {traded}. That is quality traded for throughput, so it is \
             a decision rather than a reading."
        ));
    }
    let mut item = Draft {
        id: format!("scorecard-digest:{}", digest.generated_at),
        kind: "scorecard_digest",
        severity: if worse > 0 {
            Severity::Action
        } else {
            Severity::Watch
        },
        title: match worse {
            0 => format!("Weekly scorecard for the {} window", digest.window),
            1 => "Weekly scorecard: 1 guardrail worse while volume rose".to_string(),
            _ => format!("Weekly scorecard: {worse} guardrails worse while volume rose"),
        },
        reason,
        href: HREF.to_string(),
        label: if worse > 0 {
            "Decide whether the trade-off is wanted"
        } else {
            "Read the week's numbers"
        },
        api: None,
        command: None,
    }
    .build();
    item.since = Some(event.ts.clone());
    item.family = digest.family.clone();
    item.digest = Some(digest);
    vec![item]
}
