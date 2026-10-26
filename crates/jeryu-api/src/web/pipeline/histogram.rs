//! `GET /api/v1/events/histogram`: pipeline volume over time, counted by the
//! server (contract: `docs/pipeline-events.md`).
//!
//! `GET /api/v1/events` answers at most 500 rows, so a chart drawn from it
//! counts the page it happens to hold rather than the window it claims. This
//! route counts the window itself: one `GROUP BY` over `pipeline_events`
//! ([`EventStore::histogram`]), zero-filled here into a dense series so a
//! quiet hour is a zero and not a gap in the line.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::super::WebState;
use super::super::shift::rfc3339_ms;
use super::store::{GroupRow, HistogramWindow};
use super::types::{EVENTS_SCHEMA, EventsQuery};

/// The buckets a caller may ask for, coarsest last.
const BUCKETS: &[(&str, i64)] = &[("5m", 300), ("15m", 900), ("1h", 3600), ("1d", 86_400)];
const DEFAULT_HOURS: i64 = 24;
/// A month, the log's retention.
const MAX_HOURS: i64 = 720;
/// Most buckets one answer holds. A fine bucket over a long window asks for a
/// series no chart can draw and no client wants to parse, so it is refused
/// with the bucket to use instead rather than silently coarsened.
const MAX_BUCKETS: i64 = 1500;

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct HistogramQuery {
    /// Existing Activity clients use range=6h|24h|7d.
    pub range: Option<String>,
    /// How far back the window reaches, 1 to 720 (default 24).
    pub hours: Option<i64>,
    /// `5m`, `15m`, `1h` or `1d`; the default comes from `hours`.
    pub bucket: Option<String>,
    /// Either spelling of a family key; the server canonicalises it.
    pub family: Option<String>,
    pub repo: Option<String>,
    pub source: Option<String>,
    pub kind: Option<String>,
    pub needs_human: Option<bool>,
}

impl crate::web::strict_query::StrictFields for HistogramQuery {
    const KEYS: &'static [&'static str] = &[
        "range",
        "hours",
        "bucket",
        "family",
        "repo",
        "source",
        "kind",
        "needs_human",
    ];

    /// The same code the event log answers a mistyped query with: one route
    /// reading the same filters, so one code to handle.
    const CODE: &'static str = "events_invalid_query";

    const REPAIR_HINT: &'static str =
        "hours is an integer from 1 to 720; bucket is 5m, 15m, 1h or 1d";

    fn check_values(&self) -> Result<(), String> {
        crate::web::strict_query::filter_one_of(
            "range",
            self.range.as_ref(),
            &["6h", "24h", "7d"],
        )?;
        if let Some(range) = self.range.as_deref() {
            let hours = match range {
                "6h" => 6,
                "24h" => 24,
                _ => 168,
            };
            if self.hours.is_some_and(|value| value != hours) {
                return Err("range and hours name different windows; send one".to_string());
            }
        }
        crate::web::strict_query::filter_one_of(
            "bucket",
            self.bucket.as_ref(),
            &BUCKETS.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
        )
    }
}

/// How many events of each Activity chip's kinds one bucket holds.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct ByGroup {
    pub todos: i64,
    pub gates: i64,
    pub reviews: i64,
    pub pulls: i64,
    pub merges: i64,
    pub releases: i64,
    pub deploys: i64,
    /// Everything no chip names (`worker.`, `pin.`, `shift.`, `repo.`).
    pub other: i64,
}

impl ByGroup {
    fn add(&mut self, group: &str, total: i64) {
        let counter = match group {
            "todos" => &mut self.todos,
            "gates" => &mut self.gates,
            "reviews" => &mut self.reviews,
            "pulls" => &mut self.pulls,
            "merges" => &mut self.merges,
            "releases" => &mut self.releases,
            "deploys" => &mut self.deploys,
            _ => &mut self.other,
        };
        *counter += total;
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Bucket {
    /// The bucket's start, inclusive; the next bucket's start is its end.
    pub start: String,
    pub total: i64,
    pub by_group: ByGroup,
    /// Events whose `outcome` says the step did not get through.
    pub failures: i64,
    pub needs_human: i64,
    /// Summed `cost_usd`, to the cent.
    pub cost_usd: f64,
    /// Median `seconds` of the events in the bucket that carry one (the upper
    /// of the two middles for an even count), `null` when none does.
    pub seconds_p50: Option<i64>,
}

/// The window's own counters: the tiles the Activity wall shows.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct Totals {
    pub todos_finished: i64,
    pub blocked: i64,
    pub prs_merged: i64,
    pub deploys: i64,
    pub spent_usd: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct HistogramResponse {
    pub schema_version: &'static str,
    /// Start of the first bucket, inclusive.
    pub from: String,
    /// End of the last bucket, exclusive; the bucket holding "now" is whole,
    /// so this is at or after the time of the request.
    pub to: String,
    pub bucket_seconds: i64,
    pub buckets: Vec<Bucket>,
    pub totals: Totals,
}

/// The bucket a window of `hours` is drawn with when the caller names none:
/// enough bars to read a shape, few enough to fit a page.
fn default_bucket_seconds(hours: i64) -> i64 {
    match hours {
        ..=2 => 300,
        3..=12 => 900,
        13..=168 => 3600,
        _ => 86_400,
    }
}

fn bucket_seconds(label: Option<&str>, hours: i64) -> i64 {
    match label.map(str::trim).filter(|label| !label.is_empty()) {
        None => default_bucket_seconds(hours),
        Some(label) => BUCKETS
            .iter()
            .find(|(name, _)| *name == label)
            .map(|(_, seconds)| *seconds)
            .unwrap_or_else(|| default_bucket_seconds(hours)),
    }
}

/// The finest bucket that keeps a window of `hours` under [`MAX_BUCKETS`]:
/// what a refusal tells a caller who asked for a finer one to use.
fn fitting_label(hours: i64) -> &'static str {
    BUCKETS
        .iter()
        .find(|(_, seconds)| hours * 3600 / seconds <= MAX_BUCKETS)
        .map_or("1d", |(label, _)| *label)
}

/// The window `hours` and `bucket_ms` name at `now_ms`: bucket starts are
/// whole multiples of the bucket since the epoch (so two calls seconds apart
/// draw the same bars, and a `1d` bucket is a UTC day), and the bucket holding
/// `now_ms` is included whole.
pub(super) fn window(now_ms: i64, hours: i64, bucket_ms: i64) -> (i64, i64) {
    let from_ms = (now_ms - hours * 3_600_000).div_euclid(bucket_ms) * bucket_ms;
    let buckets = (now_ms - from_ms).div_euclid(bucket_ms) + 1;
    (from_ms, from_ms + buckets * bucket_ms)
}

fn median(mut seconds: Vec<i64>) -> Option<i64> {
    if seconds.is_empty() {
        return None;
    }
    seconds.sort_unstable();
    Some(seconds[seconds.len() / 2])
}

fn cents(amount: f64) -> f64 {
    (amount * 100.0).round() / 100.0
}

/// The dense series `rows` describe, plus the window's own totals.
fn assemble(window: &HistogramWindow<'_>, rows: Vec<GroupRow>) -> HistogramResponse {
    let bucket_ms = window.bucket_ms;
    let count = usize::try_from((window.to_ms - window.from_ms) / bucket_ms).unwrap_or(0);
    let mut buckets: Vec<Bucket> = (0..count)
        .map(|index| Bucket {
            start: rfc3339_ms(window.from_ms + i64::try_from(index).unwrap_or(0) * bucket_ms),
            total: 0,
            by_group: ByGroup::default(),
            failures: 0,
            needs_human: 0,
            cost_usd: 0.0,
            seconds_p50: None,
        })
        .collect();
    let mut totals = Totals::default();
    let mut seconds: Vec<Vec<i64>> = vec![Vec::new(); buckets.len()];
    for row in rows {
        totals.todos_finished += row.todos_finished;
        totals.blocked += row.blocked;
        totals.prs_merged += row.prs_merged;
        totals.deploys += row.deploys;
        totals.spent_usd += row.cost_usd;
        let Some(index) = usize::try_from(row.bucket)
            .ok()
            .filter(|index| *index < buckets.len())
        else {
            continue;
        };
        let bucket = &mut buckets[index];
        bucket.total += row.total;
        bucket.by_group.add(&row.group, row.total);
        bucket.failures += row.failures;
        bucket.needs_human += row.needs_human;
        bucket.cost_usd += row.cost_usd;
        seconds[index].extend(row.seconds);
    }
    for (bucket, seconds) in buckets.iter_mut().zip(seconds) {
        bucket.cost_usd = cents(bucket.cost_usd);
        bucket.seconds_p50 = median(seconds);
    }
    totals.spent_usd = cents(totals.spent_usd);
    HistogramResponse {
        schema_version: EVENTS_SCHEMA,
        from: rfc3339_ms(window.from_ms),
        to: rfc3339_ms(window.to_ms),
        bucket_seconds: bucket_ms / 1000,
        buckets,
        totals,
    }
}

fn refused(reason: &str, hint: &str) -> AxumResponse {
    super::events_read_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "events_invalid_query",
        reason,
        hint,
    )
}

/// `GET /api/v1/events/histogram` (admin-only, like reading the log).
pub(crate) async fn histogram(
    State(state): State<Arc<WebState>>,
    query: Result<
        super::super::strict_query::StrictQuery<HistogramQuery>,
        super::super::strict_query::QueryRefused,
    >,
) -> AxumResponse {
    let query = match query {
        Ok(super::super::strict_query::StrictQuery(query)) => query,
        Err(refused) => return refused.into_response(),
    };
    let hours = query.hours.unwrap_or(match query.range.as_deref() {
        Some("6h") => 6,
        Some("24h") => 24,
        Some("7d") => 168,
        _ => DEFAULT_HOURS,
    });
    if !(1..=MAX_HOURS).contains(&hours) {
        return refused(
            &format!("hours must be from 1 to {MAX_HOURS}, got {hours}; it is not clamped"),
            "send hours from 1 to 720, or omit it for 24",
        );
    }
    if let Some(kind) = query.kind.as_deref().filter(|kind| !kind.is_empty())
        && !super::types::valid_kind_filter(kind)
    {
        return refused(
            &format!("kind: {kind:?} is neither a kind (todo.claimed) nor a prefix (todo.)"),
            "use a dotted lower-case kind, or a prefix that ends in a dot",
        );
    }
    let bucket_ms = if query.bucket.is_none() {
        match query.range.as_deref() {
            Some("6h") => 300,
            Some("24h") => 1800,
            Some("7d") => 21600,
            _ => bucket_seconds(None, hours),
        }
    } else {
        bucket_seconds(query.bucket.as_deref(), hours)
    } * 1000;
    let (from_ms, to_ms) = window(Utc::now().timestamp_millis(), hours, bucket_ms);
    let count = (to_ms - from_ms) / bucket_ms;
    if count > MAX_BUCKETS {
        return refused(
            &format!(
                "hours={hours} with bucket={} is {count} buckets, more than the {MAX_BUCKETS} one answer holds",
                bucket_ms / 1000
            ),
            &format!("ask for bucket={} over this window", fitting_label(hours)),
        );
    }
    let mut filter = EventsQuery {
        repo: query.repo,
        source: query.source,
        kind: query.kind,
        needs_human: query.needs_human,
        ..EventsQuery::default()
    };
    // A family the forge does not know is a typo in the request: answering it
    // with an empty chart reads as "nothing happened".
    match super::super::family::filter(&state, query.family.as_deref()) {
        Ok(family) => filter.family = family,
        Err(response) => return *response,
    }
    let window = HistogramWindow {
        from_ms,
        to_ms,
        bucket_ms,
        filter: &filter,
    };
    match state.events.histogram(&window) {
        Ok(rows) => Json(assemble(&window, rows)).into_response(),
        Err(reason) => super::events_store_error(&reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_bucket_keeps_a_window_readable() {
        for (hours, seconds) in [
            (1, 300),
            (2, 300),
            (3, 900),
            (12, 900),
            (13, 3600),
            (168, 3600),
            (169, 86_400),
            (720, 86_400),
        ] {
            assert_eq!(default_bucket_seconds(hours), seconds, "{hours}h");
            assert!(hours * 3600 / seconds <= MAX_BUCKETS, "{hours}h");
        }
        assert_eq!(bucket_seconds(Some("15m"), 720), 900, "a named bucket wins");
        assert_eq!(bucket_seconds(Some(" "), 24), 3600, "a blank one does not");
        assert_eq!(fitting_label(720), "1h");
        assert_eq!(fitting_label(24), "5m");
    }

    #[test]
    fn buckets_start_on_whole_multiples_and_include_the_current_one() {
        // 1970-01-02T05:30:30Z, an hour and a day into a bucket boundary.
        let now = (29 * 3600 + 30 * 60 + 30) * 1000;
        let (from, to) = window(now, 2, 3_600_000);
        assert_eq!(rfc3339_ms(from), "1970-01-02T03:00:00Z");
        assert_eq!(
            rfc3339_ms(to),
            "1970-01-02T06:00:00Z",
            "now's bucket is whole"
        );
        assert_eq!((to - from) / 3_600_000, 3, "2h over whole hours is 3 bars");
        let (from, to) = window(now, 24, 86_400_000);
        assert_eq!(rfc3339_ms(from), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_ms(to), "1970-01-03T00:00:00Z");
        // A window that starts before the epoch still starts on a boundary.
        let (from, _) = window(0, 1, 300_000);
        assert_eq!(rfc3339_ms(from), "1969-12-31T23:00:00Z");
    }

    #[test]
    fn the_median_is_the_upper_middle_and_money_is_cents() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![7]), Some(7));
        assert_eq!(median(vec![9, 1, 5]), Some(5));
        assert_eq!(median(vec![4, 1, 9, 5]), Some(5));
        assert_eq!(cents(0.1 + 0.2), 0.3);
    }

    /// The live 2026-10-26 read reported 0 todos finished, 0 blocked, 0 merged
    /// and 0 deploys over 24 hours while the log held hundreds of events. The
    /// events below are shaped like that log (sources, kinds, outcomes and
    /// spacing; names invented): the counted kinds all fell before the window,
    /// and what the window held was gates, hourly `todo.waiting_on_unmerged`
    /// notices (outcome `blocked`), an in-progress deploy status and a costed
    /// session. So the zeros were right, and this pins down why: the window
    /// is the last 24 hours of whole buckets, and a waiting notice or a deploy
    /// still in progress is no finished todo, block or deploy.
    #[test]
    fn real_shaped_events_count_only_what_finished_inside_the_window() {
        use crate::web::pipeline::{EventStore, NewEvent};
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(&dir.path().join("shift.sqlite")).unwrap();
        // 2026-10-26T13:07:43Z, when the read was taken.
        let now = 1_793_020_063_000;
        let hour = 3_600_000;
        let event = |source: &str, kind: &str, outcome: Option<&str>| NewEvent {
            source: source.to_string(),
            kind: kind.to_string(),
            summary: kind.to_string(),
            outcome: outcome.map(str::to_string),
            family: Some("acme".to_string()),
            repo: Some("acme/widget".to_string()),
            ..NewEvent::default()
        };
        let at = |new: NewEvent, ms: i64| {
            store.insert("operator", &new, ms).unwrap();
        };
        // Before the window: what the 7-day read counts.
        at(
            event("forge", "pr.merged", Some("success")),
            now - 34 * hour,
        );
        at(
            event("forge", "deploy.status", Some("success")),
            now - 35 * hour,
        );
        at(
            NewEvent {
                todo_id: Some("20260101-000001-aaaa01".into()),
                cost_usd: Some(1.5),
                ..event("todoq", "todo.attempt_finished", Some("blocked"))
            },
            now - 68 * hour,
        );
        at(
            NewEvent {
                todo_id: Some("20260101-000002-aaaa02".into()),
                cost_usd: Some(2.0),
                ..event("todoq", "todo.attempt_finished", Some("done"))
            },
            now - 70 * hour,
        );
        // Inside the window.
        for n in 0..20 {
            at(event("forge", "gate.started", None), now - n * hour);
            at(
                event("pr-gate", "gate.log", Some("inputs_changed")),
                now - n * hour + 60_000,
            );
            at(
                NewEvent {
                    todo_id: Some("20260101-000003-aaaa03".into()),
                    ..event("todoq", "todo.waiting_on_unmerged", Some("blocked"))
                },
                now - n * hour + 120_000,
            );
        }
        at(
            event("forge", "deploy.status", Some("in_progress")),
            now - 2 * hour,
        );
        at(
            NewEvent {
                source: "todoq-agents".into(),
                kind: "session.costed".into(),
                summary: "session".into(),
                cost_usd: Some(4.22),
                ..NewEvent::default()
            },
            now - hour,
        );

        let read = |hours: i64| {
            let bucket_ms = 1_800_000;
            let (from_ms, to_ms) = window(now, hours, bucket_ms);
            let filter = EventsQuery::default();
            let window = HistogramWindow {
                from_ms,
                to_ms,
                bucket_ms,
                filter: &filter,
            };
            assemble(&window, store.histogram(&window).unwrap())
        };
        let day = read(24);
        assert_eq!(
            day.totals,
            Totals {
                todos_finished: 0,
                blocked: 0,
                prs_merged: 0,
                deploys: 0,
                spent_usd: 4.22,
            }
        );
        let todos: i64 = day.buckets.iter().map(|b| b.by_group.todos).sum();
        assert_eq!(todos, 20, "the waiting notices are todo events, not blocks");
        let week = read(168);
        assert_eq!(
            week.totals,
            Totals {
                todos_finished: 1,
                blocked: 1,
                prs_merged: 1,
                deploys: 1,
                spent_usd: 7.72,
            }
        );
    }
}
