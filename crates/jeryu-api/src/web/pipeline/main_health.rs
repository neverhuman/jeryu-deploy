//! Main-branch health: what each repository's default branch has been doing
//! under its required gate.
//!
//! The gate on a pushed head reports through `main.status` events (see
//! [`super::emit::main_gate_status`]); this module folds them into the numbers
//! the Scorecard reads:
//!
//! * **red intervals** -- red from the first red head until the next green
//!   head, with the merged change that turned main red;
//! * **flake rate** -- a gate run that failed and then passed on the same
//!   commit with no change is a flaky run; the rate is flaky runs over gate
//!   runs, and the failing test names are kept when the runner reported them.
//!
//! The projection is a pure function of the events, so a restart loses nothing
//! and the tests need no forge.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::super::WebState;
use super::types::{Event, EventsQuery};

pub(crate) const STATUS_KIND: &str = "main.status";
pub(crate) const GREEN: &str = "green";
pub(crate) const RED: &str = "red";

/// One stretch during which main was red.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct RedInterval {
    /// The first red head.
    pub sha: String,
    pub since: String,
    /// When the next green head landed; `None` while main is still red.
    pub until: Option<String>,
    pub seconds: i64,
    /// The green head that closed it.
    pub fixed_by: Option<String>,
    /// The merged pull request whose merge commit is `sha`, when there is one.
    pub culprit_pr: Option<i64>,
    /// A rerun of the same commit went green: the interval was a flake, not a
    /// defect that escaped to main.
    pub flake: bool,
    pub tests: Vec<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct MainHealth {
    pub repo: String,
    pub red_now: bool,
    pub gate_runs: u64,
    pub flaky_runs: u64,
    /// `flaky_runs / gate_runs`, 0 when nothing ran.
    pub flake_rate: f64,
    /// Red seconds over the seconds from the first verdict to `now`.
    pub red_seconds: i64,
    pub observed_seconds: i64,
    pub red_share: f64,
    /// Escaped-to-main defects: red intervals that were not flakes.
    pub escaped_defects: u64,
    pub flaky_tests: Vec<String>,
    pub intervals: Vec<RedInterval>,
}

struct Verdict {
    sha: String,
    red: bool,
    at: DateTime<Utc>,
    tests: Vec<String>,
}

fn tests_of(event: &Event) -> Vec<String> {
    event
        .detail
        .as_ref()
        .and_then(|detail| detail.get("tests"))
        .and_then(|tests| tests.as_array())
        .map(|tests| {
            tests
                .iter()
                .filter_map(|name| name.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The merged change for each merge commit: `pr.merged` events carry the merge
/// commit as `sha`.
pub(crate) fn merged_by_sha(events: &[Event]) -> BTreeMap<String, i64> {
    events
        .iter()
        .filter(|event| event.kind == "pr.merged")
        .filter_map(|event| Some((event.sha.clone()?, event.pr?)))
        .collect()
}

/// The failing test names a runner's `gate.log` event reported, per
/// `(repo, sha)`. A `main.status` event rides the heartbeat and may be stored
/// before the log event arrives, so names are joined here rather than there.
fn logged_tests(events: &[Event]) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut out: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for event in events.iter().filter(|event| event.kind == "gate.log") {
        if let (Some(repo), Some(sha)) = (&event.repo, &event.sha) {
            out.entry((repo.clone(), sha.clone()))
                .or_default()
                .extend(tests_of(event));
        }
    }
    out
}

/// Fold `events` (any order; `main.status`, `gate.log` and `pr.merged` are
/// read) into one [`MainHealth`] per repository, sorted by repository.
pub(crate) fn project(events: &[Event], now: DateTime<Utc>) -> Vec<MainHealth> {
    let merged = merged_by_sha(events);
    let logged = logged_tests(events);
    let mut by_repo: BTreeMap<&str, Vec<&Event>> = BTreeMap::new();
    for event in events.iter().filter(|event| event.kind == STATUS_KIND) {
        if let Some(repo) = event.repo.as_deref() {
            by_repo.entry(repo).or_default().push(event);
        }
    }
    by_repo
        .into_iter()
        .map(|(repo, mut events)| {
            events.sort_by_key(|event| event.seq);
            let verdicts: Vec<Verdict> = events
                .iter()
                .filter(|event| matches!(event.outcome.as_deref(), Some(GREEN | RED)))
                .filter_map(|event| {
                    let sha = event.sha.clone()?;
                    let red = event.outcome.as_deref() == Some(RED);
                    let mut tests = tests_of(event);
                    if red && tests.is_empty() {
                        tests = logged
                            .get(&(repo.to_string(), sha.clone()))
                            .map(|names| names.iter().cloned().collect())
                            .unwrap_or_default();
                    }
                    Some(Verdict {
                        sha,
                        red,
                        at: DateTime::parse_from_rfc3339(&event.ts)
                            .ok()?
                            .with_timezone(&Utc),
                        tests,
                    })
                })
                .collect();
            health_of(repo, &verdicts, &merged, now)
        })
        .collect()
}

fn health_of(
    repo: &str,
    verdicts: &[Verdict],
    merged: &BTreeMap<String, i64>,
    now: DateTime<Utc>,
) -> MainHealth {
    // A red run is flaky when the same commit later went green.
    let mut flaky_runs = 0u64;
    let mut flaky_tests: BTreeSet<String> = BTreeSet::new();
    for verdict in verdicts.iter().filter(|v| v.red) {
        let later_green = verdicts
            .iter()
            .any(|other| !other.red && other.sha == verdict.sha && other.at >= verdict.at);
        if later_green {
            flaky_runs += 1;
            flaky_tests.extend(verdict.tests.iter().cloned());
        }
    }
    let mut intervals: Vec<RedInterval> = Vec::new();
    let mut open: Option<RedInterval> = None;
    let mut tests: BTreeSet<String> = BTreeSet::new();
    for verdict in verdicts {
        if verdict.red {
            tests.extend(verdict.tests.iter().cloned());
            if open.is_none() {
                open = Some(RedInterval {
                    sha: verdict.sha.clone(),
                    since: verdict.at.to_rfc3339(),
                    until: None,
                    seconds: 0,
                    fixed_by: None,
                    culprit_pr: merged.get(&verdict.sha).copied(),
                    flake: false,
                    tests: Vec::new(),
                });
            }
        } else if let Some(mut interval) = open.take() {
            let since = DateTime::parse_from_rfc3339(&interval.since)
                .map(|at| at.with_timezone(&Utc))
                .unwrap_or(verdict.at);
            interval.seconds = (verdict.at - since).num_seconds().max(0);
            interval.until = Some(verdict.at.to_rfc3339());
            interval.fixed_by = Some(verdict.sha.clone());
            interval.flake = interval.sha == verdict.sha;
            interval.tests = std::mem::take(&mut tests).into_iter().collect();
            intervals.push(interval);
        }
    }
    let red_now = open.is_some();
    if let Some(mut interval) = open {
        let since = DateTime::parse_from_rfc3339(&interval.since)
            .map(|at| at.with_timezone(&Utc))
            .unwrap_or(now);
        interval.seconds = (now - since).num_seconds().max(0);
        interval.tests = tests.into_iter().collect();
        intervals.push(interval);
    }
    let gate_runs = verdicts.len() as u64;
    let red_seconds: i64 = intervals.iter().map(|i| i.seconds).sum();
    let observed_seconds = verdicts
        .first()
        .map_or(0, |first| (now - first.at).num_seconds().max(0));
    let ratio = |part: i64, whole: i64| {
        if whole > 0 {
            (part as f64 / whole as f64).clamp(0.0, 1.0)
        } else {
            0.0
        }
    };
    MainHealth {
        repo: repo.to_string(),
        red_now,
        gate_runs,
        flaky_runs,
        flake_rate: if gate_runs > 0 {
            flaky_runs as f64 / gate_runs as f64
        } else {
            0.0
        },
        red_seconds,
        observed_seconds,
        red_share: ratio(red_seconds, observed_seconds),
        escaped_defects: intervals.iter().filter(|i| !i.flake).count() as u64,
        flaky_tests: flaky_tests.into_iter().collect(),
        intervals,
    }
}

/// What the store holds for `repo` (or every repository): the three kinds the
/// projection reads, newest first.
fn read_events(state: &WebState, repo: Option<&str>) -> Result<Vec<Event>, String> {
    let mut events = Vec::new();
    for (kind, limit) in [(STATUS_KIND, 1000), ("gate.log", 1000), ("pr.merged", 1000)] {
        events.extend(state.events.query(&EventsQuery {
            repo: repo.map(str::to_string),
            kind: Some(kind.to_string()),
            limit: Some(limit),
            ..EventsQuery::default()
        })?);
    }
    Ok(events)
}

/// The projection for `repo`, or every repository with a verdict.
pub(crate) fn for_repo(
    state: &WebState,
    repo: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Vec<MainHealth>, String> {
    Ok(project(&read_events(state, repo)?, now))
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct MainHealthQuery {
    /// `owner/name`; omitted for every repository.
    pub repo: Option<String>,
}

impl crate::web::strict_query::StrictFields for MainHealthQuery {
    const KEYS: &'static [&'static str] = &["repo"];
}

/// `GET /api/v1/main-health[?repo=owner/name]`.
pub(crate) async fn list(
    State(state): State<Arc<WebState>>,
    crate::web::strict_query::StrictQuery(query): crate::web::strict_query::StrictQuery<
        MainHealthQuery,
    >,
) -> AxumResponse {
    let repo = query
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .map(str::to_string);
    let worker = state.clone();
    let result =
        tokio::task::spawn_blocking(move || for_repo(&worker, repo.as_deref(), Utc::now())).await;
    match result {
        Ok(Ok(repos)) => Json(json!({
            "schema_version": "jeryu.main_health/v1",
            "repos": repos,
        }))
        .into_response(),
        _ => super::events_store_error("the event log could not be read"),
    }
}

/// Whether `repo`'s default branch is red right now: its newest `main.status`
/// verdict is `red`. One query, so a repository listing can ask it per
/// repository; the full projection is for the numbers.
pub(crate) fn red_now(state: &WebState, repo: &str) -> bool {
    state
        .events
        .query(&EventsQuery {
            repo: Some(repo.to_string()),
            kind: Some(STATUS_KIND.to_string()),
            limit: Some(50),
            ..EventsQuery::default()
        })
        .ok()
        .and_then(|events| {
            // The store answers newest first; a `started` event is no verdict.
            events
                .into_iter()
                .find(|event| matches!(event.outcome.as_deref(), Some(GREEN | RED)))
        })
        .is_some_and(|event| event.outcome.as_deref() == Some(RED))
}
