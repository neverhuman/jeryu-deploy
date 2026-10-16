//! The weekly Scorecard digest: the headline numbers of a 7-day Scorecard
//! report with the week-over-week change, built from the report itself.
//!
//! Every number here is a tile of `GET /api/v1/scorecard` read back out
//! ([`super::report`]): `value` is the week just gone, `previous` the equal
//! week before it, which the Scorecard already computes for every tile. The
//! only arithmetic is the difference between those two and, for the false-done
//! arms, the Scorecard's own [`super::compute::pct1`] over the counts its
//! `detail.by_arm` carries. A tile the Scorecard answers as unknown stays
//! unknown here, with its reason, rather than being filled in from elsewhere.
//!
//! [`Digest::warnings`] is the one judgement the digest makes: a guardrail
//! that got worse while the volume metric it is paired with rose, which is the
//! shape of trading quality for throughput and the thing a weekly read should
//! not have to spot by eye.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub(crate) const DIGEST_SCHEMA: &str = "jeryu.scorecard.digest/v1";

/// How many false-done arms the payload names. An arm is an agent, model and
/// experiment, so a deployment running many combinations has many of them,
/// and the payload is stored as one event `detail` with a byte limit: the
/// busiest arms are named and the rest are counted.
const MAX_ARMS: usize = 12;

/// Which way a metric moves when the forge is doing better.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Better {
    Lower,
    Higher,
    /// Volume and inventory: context for a guardrail, not good or bad by itself.
    Context,
}

/// One tile the digest reports, as a reader is shown it.
struct Spec {
    metric: &'static str,
    label: &'static str,
    /// What the number is in: `seconds`, `percent`, `usd`, `changes`,
    /// `pull_requests_per_day`, `todos_per_day`.
    unit: &'static str,
    better: Better,
}

const fn spec(
    metric: &'static str,
    label: &'static str,
    unit: &'static str,
    better: Better,
) -> Spec {
    Spec {
        metric,
        label,
        unit,
        better,
    }
}

/// The headline, in the order the digest reads it out.
const HEADLINES: &[Spec] = &[
    spec("lead_time_p50", "lead time p50", "seconds", Better::Lower),
    spec("lead_time_p85", "lead time p85", "seconds", Better::Lower),
    spec(
        "change_failure_rate",
        "change failure rate",
        "percent",
        Better::Lower,
    ),
    spec(
        "time_to_restore",
        "time to restore",
        "seconds",
        Better::Lower,
    ),
    spec(
        "review_catch_rate",
        "review catch rate",
        "percent",
        Better::Higher,
    ),
    spec(
        "false_done_rate",
        "false-done rate",
        "percent",
        Better::Lower,
    ),
    spec(
        "cost_per_successful_change",
        "cost per successful change",
        "usd",
        Better::Lower,
    ),
    spec(
        "merged_but_unreleased",
        "merged but unreleased",
        "changes",
        Better::Lower,
    ),
];

/// The volume tiles a guardrail is read against. They are not headline
/// numbers: each one appears only inside a warning, as the rise that makes a
/// worse guardrail worth saying out loud.
///
/// Both are counted inside the window itself. `successful_changes` is not one
/// of them on purpose: a change is successful once it has been clear of
/// failure and rework for 14 days, so in a 7-day window that tile cannot rise
/// and could never make a warning.
const VOLUMES: &[Spec] = &[
    spec(
        "prs_merged_per_day",
        "pull requests merged per day",
        "pull_requests_per_day",
        Better::Context,
    ),
    spec(
        "completed_work_items_per_day",
        "todos completed per day",
        "todos_per_day",
        Better::Context,
    ),
];

/// Each guardrail and the volume metric whose rise makes it worth calling
/// out: the rates and times a change carries against the pull requests
/// merged, and the false-done rate against the todos finished, which is what
/// it is counted over.
const PAIRS: &[(&str, &str)] = &[
    ("lead_time_p50", "prs_merged_per_day"),
    ("lead_time_p85", "prs_merged_per_day"),
    ("change_failure_rate", "prs_merged_per_day"),
    ("time_to_restore", "prs_merged_per_day"),
    ("review_catch_rate", "prs_merged_per_day"),
    ("false_done_rate", "completed_work_items_per_day"),
    ("cost_per_successful_change", "prs_merged_per_day"),
    ("merged_but_unreleased", "prs_merged_per_day"),
];

fn spec_of(metric: &str) -> Option<&'static Spec> {
    HEADLINES
        .iter()
        .chain(VOLUMES)
        .find(|spec| spec.metric == metric)
}

/// One headline number, this week against the week before.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Line {
    pub metric: String,
    pub label: String,
    pub unit: String,
    /// The week just gone; absent when the Scorecard answers unknown.
    pub value: Option<f64>,
    /// The equal week before it, as the Scorecard's own `previous`.
    pub previous: Option<f64>,
    /// `value - previous`, absent unless both are known.
    pub change: Option<f64>,
    /// The same change as a percentage of `previous`, absent when `previous`
    /// is zero or either week is unknown.
    pub change_pct: Option<f64>,
    /// `better`, `worse`, `flat`, `context` (a volume metric, which is neither)
    /// or `unknown`.
    pub direction: String,
    /// Why the Scorecard could not put a number on this week.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown: Option<String>,
    /// How many samples the week's number stands on.
    pub sample_size: u64,
}

/// The false-done rate of one experiment arm, from the tile's own
/// `detail.by_arm` counts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ArmLine {
    pub arm: String,
    pub rate_pct: f64,
    pub numerator: u64,
    pub denominator: u64,
}

/// The oldest age bucket of `holds_by_age_and_action` still holding a lot that
/// nothing has acted on since: the oldest hold nobody has taken on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct OldestUnownedHold {
    /// The bucket, as the Scorecard spells it (`under_1d` … `over_7d`).
    pub age: String,
    pub unowned: u64,
    pub held: u64,
}

/// A guardrail that got worse while its paired volume metric rose.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Warning {
    pub guardrail: String,
    pub guardrail_label: String,
    pub unit: String,
    pub previous: f64,
    pub value: f64,
    pub volume: String,
    pub volume_label: String,
    pub volume_previous: f64,
    pub volume_value: f64,
}

/// The whole digest, as the webhook payload and as the attention row's
/// `digest` field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Digest {
    pub schema_version: String,
    pub generated_at: String,
    /// The Scorecard window the numbers come from (`7d`).
    pub window: String,
    /// The family the report was scoped to; `null` is every family.
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub headline: Vec<Line>,
    /// The false-done rate per experiment arm, busiest first and at most
    /// [`MAX_ARMS`] of them; empty when the tile is unknown or carries no arms.
    #[serde(default)]
    pub false_done_by_arm: Vec<ArmLine>,
    /// Arms left out of `false_done_by_arm` by that cap.
    #[serde(default)]
    pub false_done_arms_omitted: u64,
    #[serde(default)]
    pub oldest_unowned_hold: Option<OldestUnownedHold>,
    #[serde(default)]
    pub warnings: Vec<Warning>,
}

fn tile<'a>(metrics: &'a Map<String, Value>, metric: &str) -> Option<&'a Map<String, Value>> {
    metrics.get(metric)?.as_object()
}

fn number(tile: Option<&Map<String, Value>>, field: &str) -> Option<f64> {
    tile?.get(field)?.as_f64()
}

fn count(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or_default()
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn line(metrics: &Map<String, Value>, spec: &Spec) -> Line {
    let tile = tile(metrics, spec.metric);
    let value = number(tile, "value");
    let previous = number(tile, "previous");
    let change = match (value, previous) {
        (Some(value), Some(previous)) => Some(round3(value - previous)),
        _ => None,
    };
    let change_pct = match (change, previous) {
        (Some(change), Some(previous)) if previous != 0.0 => {
            Some(round3(change / previous.abs() * 100.0))
        }
        _ => None,
    };
    let direction = match (spec.better, change) {
        (Better::Context, _) => "context",
        (_, None) => "unknown",
        (_, Some(0.0)) => "flat",
        (Better::Lower, Some(change)) if change > 0.0 => "worse",
        (Better::Lower, _) => "better",
        (Better::Higher, Some(change)) if change > 0.0 => "better",
        (Better::Higher, _) => "worse",
    };
    Line {
        metric: spec.metric.to_string(),
        label: spec.label.to_string(),
        unit: spec.unit.to_string(),
        value,
        previous,
        change,
        change_pct,
        direction: direction.to_string(),
        unknown: tile
            .and_then(|tile| tile.get("unknown"))
            .and_then(|unknown| unknown.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string),
        sample_size: count(tile.and_then(|tile| tile.get("sample_size"))),
    }
}

/// The false-done rate per arm, as the arm counts the tile already carries.
fn by_arm(metrics: &Map<String, Value>) -> (Vec<ArmLine>, u64) {
    let Some(arms) = tile(metrics, "false_done_rate")
        .and_then(|tile| tile.get("detail"))
        .and_then(|detail| detail.get("by_arm"))
        .and_then(Value::as_object)
    else {
        return (Vec::new(), 0);
    };
    let mut lines: Vec<ArmLine> = arms
        .iter()
        .filter_map(|(arm, counts)| {
            let numerator = count(counts.get("numerator"));
            let denominator = count(counts.get("denominator"));
            // An arm with no observed change has no rate; the Scorecard
            // leaves no such arm in `by_arm`, and a payload is not the place
            // to invent one.
            (denominator > 0).then(|| ArmLine {
                arm: arm.clone(),
                rate_pct: super::compute::pct1(numerator as usize, denominator as usize),
                numerator,
                denominator,
            })
        })
        .collect();
    // Busiest first, so the cap keeps the arms carrying the rate.
    lines.sort_by(|a, b| {
        b.denominator
            .cmp(&a.denominator)
            .then_with(|| a.arm.cmp(&b.arm))
    });
    let omitted = lines.len().saturating_sub(MAX_ARMS) as u64;
    lines.truncate(MAX_ARMS);
    (lines, omitted)
}

/// The oldest bucket of standing holds with a lot nothing has acted on. The
/// buckets come in age order, so the last such bucket is the oldest.
fn oldest_unowned_hold(metrics: &Map<String, Value>) -> Option<OldestUnownedHold> {
    let buckets = tile(metrics, "holds_by_age_and_action")?
        .get("detail")?
        .get("by_age")?
        .as_array()?;
    buckets
        .iter()
        .filter_map(|bucket| {
            let age = bucket.get("age")?.as_str()?.to_string();
            let held = count(bucket.get("held"));
            let unowned = held.saturating_sub(count(bucket.get("acting")));
            (unowned > 0).then_some(OldestUnownedHold { age, unowned, held })
        })
        .next_back()
}

/// Every guardrail that got worse while its paired volume metric rose. Both
/// weeks of both tiles have to be known: a tile the Scorecard cannot put a
/// number on says nothing about a trade-off.
fn warnings(metrics: &Map<String, Value>, headline: &[Line]) -> Vec<Warning> {
    let mut warnings = Vec::new();
    for (guardrail, volume) in PAIRS {
        let Some(line) = headline.iter().find(|line| line.metric == *guardrail) else {
            continue;
        };
        if line.direction != "worse" {
            continue;
        }
        let (Some(value), Some(previous)) = (line.value, line.previous) else {
            continue;
        };
        let tile = tile(metrics, volume);
        let (Some(volume_value), Some(volume_previous)) =
            (number(tile, "value"), number(tile, "previous"))
        else {
            continue;
        };
        if volume_value <= volume_previous {
            continue;
        }
        let Some(spec) = spec_of(volume) else {
            continue;
        };
        warnings.push(Warning {
            guardrail: line.metric.clone(),
            guardrail_label: line.label.clone(),
            unit: line.unit.clone(),
            previous,
            value,
            volume: spec.metric.to_string(),
            volume_label: spec.label.to_string(),
            volume_previous,
            volume_value,
        });
    }
    warnings
}

/// Build the digest from a Scorecard report. Pure: every number is read out
/// of `report.metrics`.
pub(crate) fn build(report: &super::Report) -> Digest {
    let metrics = &report.metrics;
    let headline: Vec<Line> = HEADLINES.iter().map(|spec| line(metrics, spec)).collect();
    let (false_done_by_arm, false_done_arms_omitted) = by_arm(metrics);
    Digest {
        schema_version: DIGEST_SCHEMA.to_string(),
        generated_at: report.generated_at.clone(),
        window: report.window.clone(),
        family: report.family.clone(),
        warnings: warnings(metrics, &headline),
        headline,
        false_done_by_arm,
        false_done_arms_omitted,
        oldest_unowned_hold: oldest_unowned_hold(metrics),
    }
}

/// A number as a reader is shown it, with its unit: seconds as hours and
/// minutes, a rate as a percentage, money as dollars.
pub(crate) fn amount(value: f64, unit: &str) -> String {
    match unit {
        "percent" => format!("{value}%"),
        "usd" => format!("${value:.2}"),
        "seconds" => duration(value),
        _ => format!("{}", round3(value)),
    }
}

/// Seconds as the longest two units that say something: `3h 20m`, `45m`, `12s`.
fn duration(seconds: f64) -> String {
    let total = seconds.round().max(0.0) as i64;
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m"),
        _ => format!("{hours}h {minutes}m"),
    }
}

/// One headline number as a sentence fragment: what it is now, what it was,
/// or why it is unknown.
pub(crate) fn phrase(line: &Line) -> String {
    let Some(value) = line.value else {
        return format!(
            "{} unknown ({})",
            line.label,
            line.unknown.as_deref().unwrap_or("no number")
        );
    };
    let now = amount(value, &line.unit);
    match (line.previous, line.change_pct) {
        (Some(previous), Some(pct)) => format!(
            "{} {now} ({} from {}, {}{pct}%)",
            line.label,
            line.direction,
            amount(previous, &line.unit),
            if pct > 0.0 { "+" } else { "" }
        ),
        (Some(previous), None) => format!(
            "{} {now} ({} from {})",
            line.label,
            line.direction,
            amount(previous, &line.unit)
        ),
        (None, _) => format!("{} {now} (no week before it)", line.label),
    }
}

#[cfg(test)]
mod tests;
