//! The three numbers that say how the factory is doing, carried by
//! `GET /api/v1/yield` (the Flow and Yield pages do not render it yet):
//!
//! * **lead time** -- how long a todo takes, filed to production and claim to
//!   main. Production time is the todo's pull request's first successful
//!   production deployment (`pipeline::EventStore::first_pr_deployment`), not
//!   whatever production happens to run now, so a todo that shipped in
//!   September does not read as shipped today.
//! * **exception rate** -- the share of todos that needed a person: stopped
//!   `blocked`, `handoff` or `closed`, taken on by an assignee, or with an
//!   attempt that gave up (`blocked`, `incomplete`, `retarget`, `killed`). An
//!   automatic retry is not an exception; a person's time is.
//! * **bad-change rate** -- the share of merged changes that turned a
//!   repository red: a non-flaky red stretch of main whose culprit is a merged
//!   pull request (`pipeline::main_health`), or a production deployment of a
//!   pull request that failed. One pull request counts once however many ways
//!   it went red.
//!
//! Everything is a pure function of the todos and the events handed in, so the
//! rules are testable without a forge. A number with too few samples is
//! `null` with its reason beside it, as Flow's are: a blank beats a made-up
//! median.
//! Contract: `docs/pipeline-events.md`, "Yield".

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::pipeline::Event;
use super::pipeline::main_health;
use super::shift::{ShiftTodo, TodoStatus};

/// The event kinds [`report`] reads, for the handler that pages them in.
pub(crate) const KINDS: &[&str] = &[
    "pr.merged",
    main_health::STATUS_KIND,
    "gate.log",
    "deploy.status",
];
/// Attempt outcomes that ended with a person having to look.
const GAVE_UP: &[&str] = &["blocked", "incomplete", "retarget", "killed"];
/// Samples a percentile needs before it is reported.
const MIN_SAMPLES: usize = 5;
/// Why a number is `null`.
const TOO_FEW: &str = "too_few_samples";
const NO_MERGED_CHANGES: &str = "no_merged_changes";

/// A measured span, in seconds, over the todos that have one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Span {
    /// Todos this span could be measured on.
    pub n: usize,
    pub p50: Option<i64>,
    pub p90: Option<i64>,
    /// Set when the percentiles are `null`: why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown: Option<&'static str>,
}

impl Span {
    fn of(mut samples: Vec<i64>) -> Self {
        let n = samples.len();
        if n < MIN_SAMPLES {
            return Self {
                n,
                p50: None,
                p90: None,
                unknown: Some(TOO_FEW),
            };
        }
        samples.sort_unstable();
        let rank = |percent: usize| samples[(n * percent).div_ceil(100).max(1) - 1];
        Self {
            n,
            p50: Some(rank(50)),
            p90: Some(rank(90)),
            unknown: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct LeadTime {
    /// Filed until production runs it.
    pub filed_to_prod_s: Span,
    /// First claim until the work is on the base branch.
    pub claim_to_main_s: Span,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct ExceptionRate {
    pub todos: usize,
    pub needed_human: usize,
    /// `needed_human / todos`, `null` with no todos in the window.
    pub rate: Option<f64>,
    /// How many todos each reason accounts for; a todo may have several.
    pub reasons: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct BadChangeRate {
    /// Merged pull requests in the window: the changes that could go bad.
    pub changes: usize,
    /// Those that turned main red or failed to deploy to production.
    pub bad: usize,
    /// `bad / changes`, `null` when nothing merged in the window.
    pub rate: Option<f64>,
    /// Set when `rate` is `null`: why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown: Option<&'static str>,
    /// Red stretches of main that a rerun of the same commit cleared: not
    /// counted as bad changes, and worth seeing next to those that were.
    pub flaky_reds: usize,
    /// `{repo: bad changes}`, for the repositories that had one.
    pub by_repo: BTreeMap<String, usize>,
}

/// The three headline numbers of one window.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct Headline {
    pub lead_time: LeadTime,
    pub exception_rate: ExceptionRate,
    pub bad_change_rate: BadChangeRate,
}

fn at(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|ts| ts.with_timezone(&Utc))
}

/// Whole seconds from `from` to `to`, or None when that is backwards or either
/// end is unreadable: a negative lead time is a clock, not a measurement.
fn span_s(from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>) -> Option<i64> {
    let seconds = (to? - from?).num_seconds();
    (seconds >= 0).then_some(seconds)
}

/// Why one todo needed a person, or None when none did.
fn exception_reasons(todo: &ShiftTodo) -> BTreeSet<&'static str> {
    let mut reasons = BTreeSet::new();
    match todo.status {
        TodoStatus::Blocked => {
            reasons.insert("blocked");
        }
        TodoStatus::Handoff => {
            reasons.insert("handoff");
        }
        TodoStatus::Closed => {
            reasons.insert("closed_by_hand");
        }
        _ => {}
    }
    if todo.assignee.is_some() {
        reasons.insert("taken_on_by_a_person");
    }
    for attempt in &todo.worked_by {
        if GAVE_UP.contains(&attempt.outcome.trim()) {
            reasons.insert(match attempt.outcome.trim() {
                "retarget" => "retargeted",
                "killed" => "killed",
                "incomplete" => "attempt_left_it_unmet",
                _ => "attempt_gave_up",
            });
        }
    }
    reasons
}

/// When each pull request merged, by `(repo, number)`, newest merge winning
/// whatever order the events arrive in.
fn merged_at(events: &[Event]) -> BTreeMap<(String, i64), DateTime<Utc>> {
    let mut out: BTreeMap<(String, i64), DateTime<Utc>> = BTreeMap::new();
    for event in events.iter().filter(|event| event.kind == "pr.merged") {
        if let (Some(repo), Some(pr), Some(ts)) = (event.repo.clone(), event.pr, at(&event.ts)) {
            let kept = out.entry((repo, pr)).or_insert(ts);
            *kept = (*kept).max(ts);
        }
    }
    out
}

/// Pull requests whose production deployment failed, by `(repo, number)`.
fn failed_deploys(
    events: &[Event],
    window: (DateTime<Utc>, DateTime<Utc>),
) -> BTreeSet<(String, i64)> {
    events
        .iter()
        .filter(|event| event.kind == "deploy.status")
        .filter(|event| matches!(event.outcome.as_deref(), Some("failure" | "error")))
        .filter_map(|event| {
            let ts = at(&event.ts)?;
            (window.0 <= ts && ts < window.1).then_some((event.repo.clone()?, event.pr?))
        })
        .collect()
}

/// `(repo, number)` of the merged pull requests that turned main red, and how
/// many red stretches a rerun cleared instead.
fn red_culprits(
    events: &[Event],
    now: DateTime<Utc>,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> (BTreeSet<(String, i64)>, usize) {
    let mut culprits = BTreeSet::new();
    let mut flaky = 0;
    for health in main_health::project(events, now) {
        for interval in &health.intervals {
            let Some(since) = at(&interval.since) else {
                continue;
            };
            if since < window.0 || since >= window.1 {
                continue;
            }
            if interval.flake {
                flaky += 1;
            } else if let Some(pr) = interval.culprit_pr {
                culprits.insert((health.repo.clone(), pr));
            }
        }
    }
    (culprits, flaky)
}

/// The three numbers for `todos` (already cut to the window and the family) and
/// `events`.
///
/// `released_at` answers, for one of a todo's pull requests, when production
/// first ran it; the handler reads the delivery receipts for that, which
/// outlive the raw events.
pub(crate) fn report(
    todos: &[ShiftTodo],
    events: &[Event],
    window: (DateTime<Utc>, DateTime<Utc>),
    now: DateTime<Utc>,
    released_at: &dyn Fn(&str, u64) -> Option<String>,
) -> Headline {
    let merges = merged_at(events);
    let mut filed_to_prod = Vec::new();
    let mut claim_to_main = Vec::new();
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut needed_human = 0;
    for todo in todos {
        let found = exception_reasons(todo);
        if !found.is_empty() {
            needed_human += 1;
            for reason in found {
                *reasons.entry(reason.to_string()).or_default() += 1;
            }
        }
        let filed = at(&todo.filed_at);
        // A todo's work is on main when its last repository's change is: the
        // latest merge over its pull requests.
        let on_main = todo
            .prs
            .iter()
            .filter_map(|pr| merges.get(&(pr.repo.clone(), i64::try_from(pr.number).ok()?)))
            .max()
            .copied();
        let first_claim = todo
            .worked_by
            .iter()
            .filter_map(|attempt| at(&attempt.started))
            .min();
        if let Some(seconds) = span_s(first_claim, on_main).filter(|_| todo.merged) {
            claim_to_main.push(seconds);
        }
        if todo.released == Some(true) {
            let shipped = todo
                .prs
                .iter()
                .map(|pr| released_at(&pr.repo, pr.number).and_then(|ts| at(&ts)))
                .try_fold(None, |newest: Option<DateTime<Utc>>, ts| {
                    // One repository with no deployment time leaves the todo
                    // unmeasured rather than dating it from the others.
                    ts.map(|ts| Some(newest.map_or(ts, |newest| newest.max(ts))))
                })
                .flatten();
            if let Some(seconds) = span_s(filed, shipped) {
                filed_to_prod.push(seconds);
            }
        }
    }
    let (culprits, flaky_reds) = red_culprits(events, now, window);
    let mut bad: BTreeSet<(String, i64)> = culprits;
    bad.extend(failed_deploys(events, window));
    let changes: BTreeSet<&(String, i64)> = merges
        .iter()
        .filter(|(_, ts)| window.0 <= **ts && **ts < window.1)
        .map(|(key, _)| key)
        .collect();
    // Only a change that merged in the window is in the denominator, so only
    // one that did can count against it.
    bad.retain(|key| changes.contains(key));
    let mut by_repo: BTreeMap<String, usize> = BTreeMap::new();
    for (repo, _) in &bad {
        *by_repo.entry(repo.clone()).or_default() += 1;
    }
    let todo_count = todos.len();
    Headline {
        lead_time: LeadTime {
            filed_to_prod_s: Span::of(filed_to_prod),
            claim_to_main_s: Span::of(claim_to_main),
        },
        exception_rate: ExceptionRate {
            todos: todo_count,
            needed_human,
            rate: (todo_count > 0).then(|| share(needed_human, todo_count)),
            reasons,
        },
        bad_change_rate: BadChangeRate {
            changes: changes.len(),
            bad: bad.len(),
            rate: (!changes.is_empty()).then(|| share(bad.len(), changes.len())),
            unknown: changes.is_empty().then_some(NO_MERGED_CHANGES),
            flaky_reds,
            by_repo,
        },
    }
}

/// A share rounded to whole ten-thousandths, so a rate reads as a rate.
fn share(part: usize, whole: usize) -> f64 {
    ((part as f64 / whole as f64) * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests;
