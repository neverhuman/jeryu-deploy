//! The Scorecard arithmetic: ledger rows in, one [`Metric`] per tile out.
//!
//! Pure: nothing here reads the clock, a connection or a request. Every
//! metric is a function of a half-open interval `[a, b)` that yields a value
//! and a sample size, or `None` when its source has nothing to say about the
//! interval. The driver [`run`] evaluates it over the selected window, the
//! window before it and each day of the selected window, so `value`,
//! `previous` and `daily` can never disagree about a definition.
//!
//! A metric with no sample is unknown, never zero. A count or a rate is only
//! zero when its source was recording at the time: each source carries the
//! timestamp of its earliest record, and an interval that ended before it is
//! unknown. Contract: `docs/scorecard.md`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Value, json};

use crate::web::flow::Station;
use crate::web::outcomes::project::{Change, Leg};
use crate::web::outcomes::{outcome_of, shipped_ms};

pub(crate) const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A station visit as `flow_ledger` stores it, with the repository it names.
#[derive(Clone, Debug)]
pub(crate) struct FlowRow {
    pub lot_id: String,
    pub repo: Option<String>,
    pub station: Station,
    pub entered_ms: i64,
    pub started_ms: Option<i64>,
    pub left_ms: Option<i64>,
    pub direction: String,
}

/// One `cost_ledger` row.
#[derive(Clone, Debug)]
pub(crate) struct CostRow {
    pub ts_ms: i64,
    pub kind: String,
    pub repo: Option<String>,
    pub pr: Option<i64>,
    pub todo_id: Option<String>,
    pub cost_usd: f64,
    pub seconds: Option<i64>,
}

/// One retained `pipeline_events` row of a kind the Scorecard reads.
#[derive(Clone, Debug)]
pub(crate) struct EventRow {
    pub seq: i64,
    pub ts_ms: i64,
    pub kind: String,
    pub repo: Option<String>,
    pub pr: Option<i64>,
    pub sha: Option<String>,
    pub todo_id: Option<String>,
    pub outcome: Option<String>,
    pub detail: Option<Value>,
}

/// Everything the metrics read, as loaded by `read.rs`.
#[derive(Debug, Default)]
pub(crate) struct Raw {
    pub flow: Vec<FlowRow>,
    pub costs: Vec<CostRow>,
    pub events: Vec<EventRow>,
    pub flow_from: Option<i64>,
    pub events_from: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct Unknown {
    pub reason: &'static str,
}

#[derive(Serialize)]
pub(crate) struct Metric {
    pub value: Option<Value>,
    pub previous: Option<Value>,
    pub daily: Vec<Option<Value>>,
    pub sample_size: usize,
    pub source: &'static str,
    /// Counts behind the value (numerator, denominator, pending, per arm).
    pub detail: Option<Value>,
    pub unknown: Option<Unknown>,
}

struct Out {
    value: Value,
    n: usize,
    detail: Option<Value>,
}

fn out(value: Value, n: usize) -> Option<Out> {
    Some(Out {
        value,
        n,
        detail: None,
    })
}

/// An unknown tile whose source is not recorded at all.
pub(crate) fn unrecorded(source: &'static str, days: usize, reason: &'static str) -> Metric {
    Metric {
        value: None,
        previous: None,
        daily: vec![None; days],
        sample_size: 0,
        source,
        detail: None,
        unknown: Some(Unknown { reason }),
    }
}

/// What a report is computed for.
pub(crate) struct Ctx<'a> {
    pub raw: &'a Raw,
    pub changes: &'a [Change],
    pub repo: Option<&'a str>,
    pub now_ms: i64,
    pub days: usize,
    outcome_from: Option<i64>,
    lots: Vec<Lot>,
    attributed: BTreeMap<String, f64>,
    unattributed: Vec<(i64, f64)>,
}

/// A lot's visits and what they say about its journey.
struct Lot {
    visits: Vec<FlowRow>,
    filed_ms: Option<i64>,
    ship_ms: Option<i64>,
}

fn arm_of(change: &Change) -> String {
    match (&change.agent, &change.model) {
        (None, None) => "unknown".to_string(),
        (agent, model) => format!(
            "{}:{}",
            agent.as_deref().unwrap_or("claude"),
            model.as_deref().unwrap_or("unknown")
        ),
    }
}

fn completed_ms(change: &Change) -> Option<i64> {
    change.legs.iter().filter_map(|l| l.merged_ms).min()
}

fn event_lot(repo: Option<&str>, pr: Option<i64>, todo: Option<&str>) -> Option<String> {
    match (todo, repo, pr) {
        (Some(todo), _, _) if !todo.is_empty() => Some(todo.to_string()),
        (_, Some(repo), Some(pr)) => Some(format!("{repo}#{pr}")),
        _ => None,
    }
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(
        raw: &'a Raw,
        changes: &'a [Change],
        repo: Option<&'a str>,
        now_ms: i64,
        days: usize,
    ) -> Self {
        let mut by_lot: BTreeMap<&str, Vec<&FlowRow>> = BTreeMap::new();
        for row in &raw.flow {
            by_lot.entry(&row.lot_id).or_default().push(row);
        }
        let lots = by_lot
            .into_values()
            .filter(|rows| {
                repo.is_none_or(|want| rows.iter().any(|r| r.repo.as_deref() == Some(want)))
            })
            .map(|rows| {
                let filed_ms = rows
                    .iter()
                    .filter(|r| r.station == Station::Queue)
                    .map(|r| r.entered_ms)
                    .min();
                let ship_ms = rows
                    .iter()
                    .filter(|r| r.station == Station::Deploy && r.direction == "forward")
                    .filter_map(|r| r.left_ms)
                    .max();
                Lot {
                    visits: rows.into_iter().cloned().collect(),
                    filed_ms,
                    ship_ms,
                }
            })
            .collect();
        let (attributed, unattributed) = attribute_costs(&raw.costs, changes, repo);
        Self {
            raw,
            changes,
            repo,
            now_ms,
            days,
            outcome_from: changes.iter().map(|c| c.first_seen_ms).min(),
            lots,
            attributed,
            unattributed,
        }
    }

    fn start(&self) -> i64 {
        self.now_ms - self.days as i64 * DAY_MS
    }
}

/// Spend per change, and the spend no change claims.
///
/// A row joins its todo (or the lot id); failing that its repository and pull
/// request, split equally when several changes carry that pull request.
fn attribute_costs(
    costs: &[CostRow],
    changes: &[Change],
    repo: Option<&str>,
) -> (BTreeMap<String, f64>, Vec<(i64, f64)>) {
    let mut by_todo: BTreeMap<&str, &str> = BTreeMap::new();
    let mut by_pr: BTreeMap<(&str, i64), BTreeSet<&str>> = BTreeMap::new();
    for change in changes {
        by_todo.insert(&change.lot_id, &change.lot_id);
        if let Some(todo) = &change.todo_id {
            by_todo.insert(todo, &change.lot_id);
        }
        for leg in &change.legs {
            if let Some(pr) = leg.pr {
                by_pr
                    .entry((leg.repo.as_str(), pr))
                    .or_default()
                    .insert(&change.lot_id);
            }
        }
    }
    let mut attributed: BTreeMap<String, f64> = BTreeMap::new();
    let mut unattributed = Vec::new();
    for row in costs {
        if let Some(lot) = row.todo_id.as_deref().and_then(|t| by_todo.get(t)) {
            *attributed.entry((*lot).to_string()).or_default() += row.cost_usd;
            continue;
        }
        let owners = match (row.repo.as_deref(), row.pr) {
            (Some(repo), Some(pr)) => by_pr.get(&(repo, pr)),
            _ => None,
        };
        match owners {
            Some(owners) if !owners.is_empty() => {
                let share = row.cost_usd / owners.len() as f64;
                for lot in owners {
                    *attributed.entry((*lot).to_string()).or_default() += share;
                }
            }
            _ => {
                if repo.is_none() || row.repo.as_deref() == repo {
                    unattributed.push((row.ts_ms, row.cost_usd));
                }
            }
        }
    }
    (attributed, unattributed)
}

/// Nearest-rank percentile of sorted samples.
fn rank(sorted: &[i64], percent: usize) -> i64 {
    let index = (sorted.len() * percent).div_ceil(100).max(1) - 1;
    sorted[index]
}

fn sorted(mut samples: Vec<i64>) -> Vec<i64> {
    samples.sort_unstable();
    samples
}

fn pct1(numerator: usize, denominator: usize) -> f64 {
    ((numerator as f64 / denominator as f64) * 1000.0).round() / 10.0
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

/// Elapsed days of `[a, b)` that the source was recording for; `None` when it
/// started after the interval ended.
fn covered_days(from: Option<i64>, a: i64, b: i64) -> Option<f64> {
    let begin = a.max(from?);
    (begin < b).then(|| (b - begin) as f64 / DAY_MS as f64)
}

fn per_day(count: usize, days: f64) -> f64 {
    round3(count as f64 / days)
}

fn within(at: i64, a: i64, b: i64) -> bool {
    at >= a && at < b
}

fn run(
    ctx: &Ctx,
    source: &'static str,
    why: &'static str,
    f: impl Fn(i64, i64) -> Option<Out>,
) -> Metric {
    let start = ctx.start();
    let span = ctx.days as i64 * DAY_MS;
    let current = f(start, ctx.now_ms);
    let previous = f(start - span, start).map(|o| o.value);
    let daily = (0..ctx.days as i64)
        .map(|i| f(start + i * DAY_MS, start + (i + 1) * DAY_MS).map(|o| o.value))
        .collect();
    match current {
        Some(o) => Metric {
            value: Some(o.value),
            previous,
            daily,
            sample_size: o.n,
            source,
            detail: o.detail,
            unknown: None,
        },
        None => Metric {
            value: None,
            previous,
            daily,
            sample_size: 0,
            source,
            detail: None,
            unknown: Some(Unknown { reason: why }),
        },
    }
}

const NO_SAMPLES: &str = "no_samples_in_window";
const NO_COVERAGE: &str = "source_not_recording_in_window";

impl Ctx<'_> {
    fn shipped_lots(&self, a: i64, b: i64) -> impl Iterator<Item = (&Lot, i64, i64)> {
        self.lots.iter().filter_map(move |lot| {
            let (filed, ship) = (lot.filed_ms?, lot.ship_ms?);
            (within(ship, a, b) && ship >= filed).then_some((lot, filed, ship))
        })
    }

    fn lead_samples(&self, a: i64, b: i64) -> Vec<i64> {
        sorted(
            self.shipped_lots(a, b)
                .map(|(_, filed, ship)| (ship - filed) / 1000)
                .collect(),
        )
    }

    fn lead_percentile(&self, a: i64, b: i64, percent: usize) -> Option<Out> {
        let s = self.lead_samples(a, b);
        (!s.is_empty()).then(|| Out {
            value: json!(rank(&s, percent)),
            n: s.len(),
            detail: None,
        })
    }

    fn lead_spread(&self, a: i64, b: i64) -> Option<Out> {
        let s = self.lead_samples(a, b);
        (!s.is_empty()).then(|| Out {
            value: json!(rank(&s, 85) - rank(&s, 50)),
            n: s.len(),
            detail: Some(json!({"p50_s": rank(&s, 50), "p85_s": rank(&s, 85)})),
        })
    }

    fn station_breakdown(&self, a: i64, b: i64) -> Option<Out> {
        let mut total: BTreeMap<Station, Vec<i64>> = BTreeMap::new();
        let mut queue: BTreeMap<Station, Vec<i64>> = BTreeMap::new();
        let mut process: BTreeMap<Station, Vec<i64>> = BTreeMap::new();
        let mut lots = 0;
        for (lot, _, _) in self.shipped_lots(a, b) {
            lots += 1;
            for v in &lot.visits {
                let (Some(left), "forward") = (v.left_ms, v.direction.as_str()) else {
                    continue;
                };
                if left < v.entered_ms {
                    continue;
                }
                total
                    .entry(v.station)
                    .or_default()
                    .push((left - v.entered_ms) / 1000);
                let started = if v.station.starts_on_arrival() {
                    Some(v.entered_ms)
                } else {
                    v.started_ms
                };
                if let Some(started) = started.filter(|s| *s >= v.entered_ms && *s <= left) {
                    queue
                        .entry(v.station)
                        .or_default()
                        .push((started - v.entered_ms) / 1000);
                    process
                        .entry(v.station)
                        .or_default()
                        .push((left - started) / 1000);
                }
            }
        }
        if lots == 0 {
            return None;
        }
        let mut stations = serde_json::Map::new();
        for station in Station::ALL {
            let Some(t) = total.remove(&station) else {
                continue;
            };
            let t = sorted(t);
            let median = |m: &mut BTreeMap<Station, Vec<i64>>| {
                m.remove(&station)
                    .map(sorted)
                    .filter(|s| !s.is_empty())
                    .map_or(Value::Null, |s| json!(rank(&s, 50)))
            };
            stations.insert(
                station.id().to_string(),
                json!({
                    "samples": t.len(),
                    "total_p50_s": rank(&t, 50),
                    "queue_p50_s": median(&mut queue),
                    "process_p50_s": median(&mut process),
                }),
            );
        }
        out(Value::Object(stations), lots)
    }

    fn flow_efficiency(&self, a: i64, b: i64) -> Option<Out> {
        let mut ratios: Vec<i64> = Vec::new();
        for (lot, filed, ship) in self.shipped_lots(a, b) {
            let lead = ship - filed;
            let mut active = 0;
            let mut classified = lead > 0;
            for v in lot.visits.iter().filter(|v| v.left_ms.is_some()) {
                let left = v.left_ms.unwrap_or(v.entered_ms);
                let started = if v.station.starts_on_arrival() {
                    Some(v.entered_ms)
                } else {
                    v.started_ms
                };
                match started {
                    Some(s) if s <= left => active += left - s,
                    _ => classified = false,
                }
            }
            if classified && active <= lead {
                ratios.push((active as f64 / lead as f64 * 1000.0).round() as i64);
            }
        }
        let ratios = sorted(ratios);
        (!ratios.is_empty()).then(|| Out {
            value: json!(rank(&ratios, 50) as f64 / 1000.0),
            n: ratios.len(),
            detail: None,
        })
    }

    fn wip_by_station(&self, _a: i64, at: i64) -> Option<Out> {
        covered_days(self.raw.flow_from, at - 1, at)?;
        let mut by: BTreeMap<Station, [usize; 3]> = BTreeMap::new();
        let mut open = BTreeSet::new();
        for lot in &self.lots {
            for v in &lot.visits {
                if v.entered_ms > at || v.left_ms.is_some_and(|l| l <= at) {
                    continue;
                }
                open.insert(v.lot_id.as_str());
                let slot = by.entry(v.station).or_default();
                if v.direction == "hold" && v.left_ms.is_none() && at >= self.now_ms {
                    slot[2] += 1;
                } else if v.started_ms.is_some_and(|s| s <= at) {
                    slot[1] += 1;
                } else {
                    slot[0] += 1;
                }
            }
        }
        let stations: serde_json::Map<String, Value> = Station::ALL
            .into_iter()
            .map(|s| {
                let c = by.get(&s).copied().unwrap_or_default();
                (
                    s.id().to_string(),
                    json!({"waiting": c[0], "active": c[1], "held": c[2]}),
                )
            })
            .collect();
        out(Value::Object(stations), open.len())
    }

    fn holds_by_age_and_action(&self, _a: i64, at: i64) -> Option<Out> {
        // Hold history is not kept: only the holds standing now are known.
        if at < self.now_ms {
            return None;
        }
        covered_days(self.raw.flow_from, at - 1, at)?;
        let mut acting_after: BTreeMap<String, i64> = BTreeMap::new();
        for e in &self.raw.events {
            if !ACTION_KINDS.contains(&e.kind.as_str()) {
                continue;
            }
            if let Some(lot) = event_lot(e.repo.as_deref(), e.pr, e.todo_id.as_deref()) {
                let latest = acting_after.entry(lot).or_insert(e.ts_ms);
                *latest = (*latest).max(e.ts_ms);
            }
        }
        let mut buckets = [
            ("under_1d", 0usize, 0usize),
            ("1d_to_3d", 0, 0),
            ("3d_to_7d", 0, 0),
            ("over_7d", 0, 0),
        ];
        let mut held = BTreeSet::new();
        for lot in &self.lots {
            for v in &lot.visits {
                if v.direction != "hold" || v.left_ms.is_some() || !held.insert(&v.lot_id) {
                    continue;
                }
                // The visit's arrival is the best age the ledger has: the hold
                // itself is not timestamped.
                let age = (self.now_ms - v.entered_ms).max(0);
                let slot = match age / DAY_MS {
                    0 => 0,
                    1..=2 => 1,
                    3..=6 => 2,
                    _ => 3,
                };
                buckets[slot].1 += 1;
                if acting_after
                    .get(&v.lot_id)
                    .is_some_and(|t| *t > v.entered_ms)
                {
                    buckets[slot].2 += 1;
                }
            }
        }
        let acting: usize = buckets.iter().map(|b| b.2).sum();
        let detail = json!({
            "acting": acting,
            "not_acting": held.len() - acting,
            "by_age": buckets
                .iter()
                .map(|(age, n, acted)| json!({"age": age, "held": n, "acting": acted}))
                .collect::<Vec<_>>(),
        });
        Some(Out {
            value: json!(held.len()),
            n: held.len(),
            detail: Some(detail),
        })
    }

    fn gate_queue_wait(&self, a: i64, b: i64) -> Option<Out> {
        let samples = sorted(
            self.lots
                .iter()
                .flat_map(|l| &l.visits)
                .filter(|v| v.station == Station::Gate)
                .filter_map(|v| {
                    let started = v.started_ms?;
                    (within(started, a, b) && started >= v.entered_ms)
                        .then(|| (started - v.entered_ms) / 1000)
                })
                .collect(),
        );
        (!samples.is_empty()).then(|| Out {
            value: json!({"p50_s": rank(&samples, 50), "p85_s": rank(&samples, 85)}),
            n: samples.len(),
            detail: None,
        })
    }
}

/// Events that show someone acting on a held lot.
const ACTION_KINDS: &[&str] = &[
    "todo.action",
    "pr.review",
    "pr.reviewed",
    "review.started",
    "review.finished",
    "gate.started",
    "gate.finished",
    "release.staged",
    "deploy.created",
    "deploy.status",
];

/// Outcome-ledger metrics.
impl Ctx<'_> {
    /// The share of `eligible` changes with `hit`, with the arms apart.
    fn rate(
        &self,
        a: i64,
        b: i64,
        at: impl Fn(&Change) -> Option<i64>,
        eligible: impl Fn(&Change) -> bool,
        hit: impl Fn(&Change) -> bool,
    ) -> Option<Out> {
        let mut total = 0;
        let mut hits = 0;
        let mut pending = 0;
        let mut arms: BTreeMap<String, [usize; 2]> = BTreeMap::new();
        for c in self.changes {
            if !at(c).is_some_and(|ts| within(ts, a, b)) {
                continue;
            }
            if !eligible(c) {
                continue;
            }
            if outcome_of(c, self.now_ms) == "pending" {
                pending += 1;
                continue;
            }
            total += 1;
            let arm = arms.entry(arm_of(c)).or_default();
            arm[1] += 1;
            if hit(c) {
                hits += 1;
                arm[0] += 1;
            }
        }
        (total > 0).then(|| Out {
            value: json!(pct1(hits, total)),
            n: total,
            detail: Some(json!({
                "numerator": hits,
                "denominator": total,
                "pending": pending,
                "by_arm": arms
                    .into_iter()
                    .map(|(arm, [n, d])| (arm, json!({"numerator": n, "denominator": d})))
                    .collect::<serde_json::Map<_, _>>(),
            })),
        })
    }

    fn successful_changes(&self, a: i64, b: i64) -> Option<Out> {
        covered_days(self.outcome_from, a, b)?;
        let n = self
            .changes
            .iter()
            .filter(|c| shipped_ms(&c.legs).is_some_and(|at| within(at, a, b)))
            .filter(|c| outcome_of(c, self.now_ms) == "successful")
            .count();
        out(json!(n), n)
    }

    fn work_mix(&self, a: i64, b: i64) -> Option<Out> {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        let mut total = 0;
        for c in self.changes {
            if completed_ms(c).is_some_and(|at| within(at, a, b)) {
                total += 1;
                *counts
                    .entry(c.work_kind.as_deref().unwrap_or("unclassified"))
                    .or_default() += 1;
            }
        }
        if total == 0 {
            return None;
        }
        let classified = total - counts.get("unclassified").copied().unwrap_or(0);
        let value: serde_json::Map<String, Value> =
            ["feature", "fix", "rework", "ops", "unclassified"]
                .into_iter()
                .map(|k| (k.to_string(), json!(counts.get(k).copied().unwrap_or(0))))
                .collect();
        let shares: serde_json::Map<String, Value> = ["feature", "fix", "rework", "ops"]
            .into_iter()
            .filter(|_| classified > 0)
            .map(|k| {
                let n = counts.get(k).copied().unwrap_or(0);
                (k.to_string(), json!(pct1(n, classified)))
            })
            .collect();
        Some(Out {
            value: Value::Object(value),
            n: total,
            detail: Some(json!({"share_of_classified_pct": shares})),
        })
    }

    fn changes_per_release(&self, a: i64, b: i64) -> Option<Out> {
        let mut releases: BTreeMap<(&str, &str), (i64, BTreeSet<&str>)> = BTreeMap::new();
        for c in self.changes {
            for leg in &c.legs {
                let (Some(release), Some(at)) =
                    (leg.release.as_deref(), leg.deployed_ms.or(leg.released_ms))
                else {
                    continue;
                };
                if within(at, a, b) {
                    releases
                        .entry((leg.repo.as_str(), release))
                        .or_insert((at, BTreeSet::new()))
                        .1
                        .insert(&c.lot_id);
                }
            }
        }
        let sizes = sorted(releases.values().map(|(_, s)| s.len() as i64).collect());
        (!sizes.is_empty()).then(|| Out {
            value: json!({"p50": rank(&sizes, 50), "p85": rank(&sizes, 85)}),
            n: sizes.len(),
            detail: None,
        })
    }

    fn completed_per_day(&self, a: i64, b: i64) -> Option<Out> {
        let days = covered_days(self.outcome_from, a, b)?;
        let n = self
            .changes
            .iter()
            .filter(|c| c.todo_id.is_some())
            .filter(|c| completed_ms(c).is_some_and(|at| within(at, a, b)))
            .count();
        out(json!(per_day(n, days)), n)
    }

    fn prs_merged_per_day(&self, a: i64, b: i64) -> Option<Out> {
        let days = covered_days(self.outcome_from, a, b)?;
        let prs: BTreeSet<(&str, i64)> = self
            .changes
            .iter()
            .flat_map(|c| &c.legs)
            .filter(|l: &&Leg| l.merged_ms.is_some_and(|at| within(at, a, b)))
            .filter_map(|l| Some((l.repo.as_str(), l.pr?)))
            .collect();
        out(json!(per_day(prs.len(), days)), prs.len())
    }

    fn arrival_vs_completion(&self, a: i64, b: i64) -> Option<Out> {
        let days = covered_days(self.outcome_from, a, b)?;
        let filed = self
            .changes
            .iter()
            .filter(|c| c.todo_id.is_some())
            .filter(|c| c.filed_ms.is_some_and(|at| within(at, a, b)))
            .count();
        let done = self
            .changes
            .iter()
            .filter(|c| c.todo_id.is_some())
            .filter(|c| completed_ms(c).is_some_and(|at| within(at, a, b)))
            .count();
        out(
            json!({
                "filed_per_day": per_day(filed, days),
                "completed_per_day": per_day(done, days),
                "gap_per_day": round3(filed as f64 / days - done as f64 / days),
            }),
            filed + done,
        )
    }

    fn merged_but_unreleased(&self, _a: i64, at: i64) -> Option<Out> {
        covered_days(self.outcome_from, at - 1, at)?;
        let ages: Vec<i64> = self
            .changes
            .iter()
            .filter_map(|c| {
                let merged = completed_ms(c).filter(|m| *m <= at)?;
                let shipped = shipped_ms(&c.legs).is_some_and(|s| s <= at);
                (!shipped).then_some((at - merged) / 1000)
            })
            .collect();
        let oldest = ages.iter().max().copied();
        Some(Out {
            value: json!(ages.len()),
            n: ages.len(),
            detail: Some(json!({"oldest_age_s": oldest})),
        })
    }
}

/// Event-log metrics.
impl Ctx<'_> {
    fn events<'s>(&'s self, kind: &'s str, a: i64, b: i64) -> impl Iterator<Item = &'s EventRow> {
        self.raw
            .events
            .iter()
            .filter(move |e| e.kind == kind && within(e.ts_ms, a, b))
            .filter(move |e| self.repo.is_none_or(|r| e.repo.as_deref() == Some(r)))
    }

    fn releases_per_day(&self, a: i64, b: i64) -> Option<Out> {
        let days = covered_days(self.raw.events_from, a, b)?;
        type Seen = BTreeMap<String, BTreeSet<String>>;
        let mut seen: BTreeMap<&str, Seen> = BTreeMap::new();
        let mut n = 0;
        for (label, kind, want) in [
            ("releases", "release.staged", None),
            ("deploys", "deploy.status", Some("success")),
        ] {
            for e in self.events(kind, a, b) {
                if want.is_some_and(|w| e.outcome.as_deref() != Some(w)) {
                    continue;
                }
                let Some(service) = e.repo.clone() else {
                    continue;
                };
                let release = e
                    .detail
                    .as_ref()
                    .and_then(|d| d.get("release"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .or_else(|| e.sha.clone())
                    .unwrap_or_else(|| format!("seq-{}", e.seq));
                if seen
                    .entry(label)
                    .or_default()
                    .entry(service)
                    .or_default()
                    .insert(release)
                {
                    n += 1;
                }
            }
        }
        if n == 0 {
            return None;
        }
        let rates = |label: &str| -> Value {
            let mut per_service = serde_json::Map::new();
            if let Some(m) = seen.get(label) {
                for (s, set) in m {
                    per_service.insert(s.clone(), json!(per_day(set.len(), days)));
                }
            }
            Value::Object(per_service)
        };
        out(
            json!({"releases": rates("releases"), "deploys": rates("deploys")}),
            n,
        )
    }

    fn gate_flake_rate(&self, a: i64, b: i64) -> Option<Out> {
        covered_days(self.raw.events_from, a, b)?;
        type Attempts = Vec<(i64, bool, i64)>;
        let mut groups: BTreeMap<(&str, i64, &str), Attempts> = BTreeMap::new();
        for e in self.raw.events.iter().filter(|e| e.kind == "gate.finished") {
            let pass = match e.outcome.as_deref() {
                Some("success") => true,
                Some("failure" | "error") => false,
                _ => continue,
            };
            if self.repo.is_some_and(|r| e.repo.as_deref() != Some(r)) {
                continue;
            }
            if let (Some(repo), Some(pr), Some(sha)) = (&e.repo, e.pr, &e.sha) {
                groups
                    .entry((repo.as_str(), pr, sha.as_str()))
                    .or_default()
                    .push((e.seq, pass, e.ts_ms));
            }
        }
        let mut total = 0;
        let mut flaked = 0;
        for (_, mut attempts) in groups {
            attempts.sort_unstable();
            if !attempts.last().is_some_and(|(_, _, ts)| within(*ts, a, b)) {
                continue;
            }
            total += 1;
            let first_fail = attempts.iter().position(|(_, pass, _)| !pass);
            if first_fail.is_some_and(|i| attempts[i..].iter().any(|(_, pass, _)| *pass)) {
                flaked += 1;
            }
        }
        (total > 0).then(|| Out {
            value: json!(pct1(flaked, total)),
            n: total,
            detail: Some(json!({"flaked": flaked, "commits": total})),
        })
    }
}

/// Cost-ledger metrics.
impl Ctx<'_> {
    fn cost_per_change(
        &self,
        a: i64,
        b: i64,
        cohort: impl Fn(&Change) -> Option<i64>,
    ) -> Option<Out> {
        let mut total = 0.0;
        let mut costed = 0;
        let mut uncosted = 0;
        for c in self.changes {
            if !cohort(c).is_some_and(|at| within(at, a, b)) {
                continue;
            }
            match self.attributed.get(&c.lot_id) {
                Some(usd) => {
                    total += usd;
                    costed += 1;
                }
                None => uncosted += 1,
            }
        }
        if costed == 0 {
            return None;
        }
        let stray: f64 = self
            .unattributed
            .iter()
            .filter(|(ts, _)| within(*ts, a, b))
            .map(|(_, usd)| usd)
            .sum();
        Some(Out {
            value: json!(round4(total / costed as f64)),
            n: costed,
            detail: Some(json!({
                "total_usd": round4(total),
                "changes": costed,
                "changes_without_recorded_cost": uncosted,
                "unattributed_usd": round4(stray),
            })),
        })
    }

    fn review_cost(&self, a: i64, b: i64) -> Option<Out> {
        let rows: Vec<&CostRow> = self
            .raw
            .costs
            .iter()
            .filter(|r| r.kind == "pr.reviewed" && within(r.ts_ms, a, b))
            .filter(|r| self.repo.is_none_or(|w| r.repo.as_deref() == Some(w)))
            .collect();
        if rows.is_empty() {
            return None;
        }
        let changes: BTreeSet<(Option<&str>, Option<i64>)> =
            rows.iter().map(|r| (r.repo.as_deref(), r.pr)).collect();
        let total: f64 = rows.iter().map(|r| r.cost_usd).sum();
        let seconds: Option<i64> = rows.iter().map(|r| r.seconds).sum();
        Some(Out {
            value: json!(round4(total / changes.len() as f64)),
            n: rows.len(),
            detail: Some(json!({
                "total_usd": round4(total),
                "reviews": rows.len(),
                "reviewed_changes": changes.len(),
                "total_seconds": seconds,
            })),
        })
    }
}

/// `(name, source note)` of every tile, in contract order, with the reason a
/// tile whose source todo has not landed stays unknown.
pub(crate) const UNRECORDED: &[(&str, &str, &str)] = &[
    (
        "surviving_net_lines_per_day",
        "line survival ledger",
        "source_not_landed: line survival ledger",
    ),
    (
        "raw_net_lines_per_day",
        "durable pull request diff statistics",
        "source_not_landed: pull request diff statistics",
    ),
    (
        "escaped_defects_to_main",
        "outcome ledger and review findings",
        "source_not_landed: defect identities and review findings",
    ),
    (
        "escaped_defects_to_prod",
        "outcome and incident ledgers",
        "source_not_landed: defect identities and incident ledger",
    ),
    (
        "review_catch_rate",
        "review finding outcomes and outcome ledger",
        "source_not_landed: review finding outcomes",
    ),
    (
        "main_red_time_per_repo",
        "main health transitions",
        "source_not_landed: main health transitions",
    ),
    (
        "time_to_detect",
        "incident ledger",
        "source_not_landed: incident open and detect timestamps",
    ),
    (
        "time_to_restore",
        "incident ledger and successful deploy events",
        "source_not_landed: incident ledger",
    ),
    (
        "human_minutes_per_successful_change",
        "human time and outcome ledgers",
        "source_not_landed: human time ledger",
    ),
    (
        "eta_error",
        "immutable Flow ETA snapshots",
        "source_not_recorded: Flow computes ETAs at read time and stores no snapshot",
    ),
    (
        "worker_slot_utilisation",
        "worker slot state history",
        "source_not_landed: worker slot history",
    ),
    (
        "runner_utilisation",
        "runner heartbeat history",
        "source_not_landed: runner heartbeat history",
    ),
    (
        "hypotheses_checked_and_met",
        "hypothesis records and release/deploy events",
        "source_not_landed: hypothesis records",
    ),
];

/// Every metric of the report, in a stable order.
pub(crate) fn metrics(ctx: &Ctx) -> Vec<(&'static str, Metric)> {
    let mut all: Vec<(&'static str, Metric)> = vec![
        (
            "lead_time_p50",
            run(
                ctx,
                "flow_ledger: filed to deployed, nearest-rank p50 seconds",
                NO_SAMPLES,
                |a, b| ctx.lead_percentile(a, b, 50),
            ),
        ),
        (
            "lead_time_p85",
            run(
                ctx,
                "flow_ledger: filed to deployed, nearest-rank p85 seconds",
                NO_SAMPLES,
                |a, b| ctx.lead_percentile(a, b, 85),
            ),
        ),
        (
            "lead_time_spread",
            run(
                ctx,
                "flow_ledger: lead time p85 minus p50 seconds",
                NO_SAMPLES,
                |a, b| ctx.lead_spread(a, b),
            ),
        ),
        (
            "station_breakdown",
            run(
                ctx,
                "flow_ledger station visits of shipped lots, median seconds",
                NO_SAMPLES,
                |a, b| ctx.station_breakdown(a, b),
            ),
        ),
        (
            "flow_efficiency",
            run(
                ctx,
                "flow_ledger: median of active time over lead time, 0 to 1",
                NO_SAMPLES,
                |a, b| ctx.flow_efficiency(a, b),
            ),
        ),
        (
            "wip_by_station",
            run(
                ctx,
                "flow_ledger open visits at the end of each interval; held is only known for now",
                NO_COVERAGE,
                |a, b| ctx.wip_by_station(a, b),
            ),
        ),
        (
            "holds_by_age_and_action",
            run(
                ctx,
                "flow_ledger holds standing now, aged from station arrival, with recorded actions after it",
                NO_COVERAGE,
                |a, b| ctx.holds_by_age_and_action(a, b),
            ),
        ),
        (
            "gate_queue_wait",
            run(
                ctx,
                "flow_ledger gate visits, arrival to start (arrival is when Flow puts the lot at the gate), seconds",
                NO_SAMPLES,
                |a, b| ctx.gate_queue_wait(a, b),
            ),
        ),
        (
            "completed_work_items_per_day",
            run(
                ctx,
                "outcome_ledger: todos whose change merged, per day",
                NO_COVERAGE,
                |a, b| ctx.completed_per_day(a, b),
            ),
        ),
        (
            "prs_merged_per_day",
            run(
                ctx,
                "outcome_ledger: distinct pull requests merged, per day (context)",
                NO_COVERAGE,
                |a, b| ctx.prs_merged_per_day(a, b),
            ),
        ),
        (
            "arrival_vs_completion",
            run(
                ctx,
                "outcome_ledger: todos filed and completed per day",
                NO_COVERAGE,
                |a, b| ctx.arrival_vs_completion(a, b),
            ),
        ),
        (
            "merged_but_unreleased",
            run(
                ctx,
                "outcome_ledger: merged changes not yet shipped, count at the end of each interval",
                NO_COVERAGE,
                |a, b| ctx.merged_but_unreleased(a, b),
            ),
        ),
        (
            "releases_per_day",
            run(
                ctx,
                "pipeline_events release.staged and deploy.status success, per service per day",
                NO_COVERAGE,
                |a, b| ctx.releases_per_day(a, b),
            ),
        ),
        (
            "changes_per_release",
            run(
                ctx,
                "outcome_ledger: distinct changes first shipped in each release",
                NO_SAMPLES,
                |a, b| ctx.changes_per_release(a, b),
            ),
        ),
        (
            "change_failure_rate",
            run(
                ctx,
                "outcome_ledger: failed over shipped changes past the 14-day observation, percent",
                NO_SAMPLES,
                |a, b| {
                    ctx.rate(
                        a,
                        b,
                        |c| shipped_ms(&c.legs),
                        |_| true,
                        |c| outcome_of(c, ctx.now_ms) == "failed",
                    )
                },
            ),
        ),
        (
            "rework_rate",
            run(
                ctx,
                "outcome_ledger: merged changes later reworked over observed ones, percent",
                NO_SAMPLES,
                |a, b| ctx.rate(a, b, completed_ms, |_| true, |c| !c.reworked_by.is_empty()),
            ),
        ),
        (
            "false_done_rate",
            run(
                ctx,
                "outcome_ledger: reported-done todos later reworked or reverted over observed ones, percent",
                NO_SAMPLES,
                |a, b| {
                    ctx.rate(
                        a,
                        b,
                        |c| c.done_ms,
                        |_| true,
                        |c| !c.reworked_by.is_empty() || c.reverted_ms.is_some(),
                    )
                },
            ),
        ),
        (
            "successful_changes",
            run(
                ctx,
                "outcome_ledger: changes shipped and clear of failure and rework for 14 days",
                NO_COVERAGE,
                |a, b| ctx.successful_changes(a, b),
            ),
        ),
        (
            "completed_work_mix",
            run(
                ctx,
                "outcome_ledger: merged changes by work kind",
                NO_SAMPLES,
                |a, b| ctx.work_mix(a, b),
            ),
        ),
        (
            "cost_per_merged_change",
            run(
                ctx,
                "cost_ledger spend of each change merged in the interval, USD per change with recorded cost",
                NO_SAMPLES,
                |a, b| ctx.cost_per_change(a, b, completed_ms),
            ),
        ),
        (
            "cost_per_successful_change",
            run(
                ctx,
                "cost_ledger spend over outcome_ledger successful changes, USD per change with recorded cost",
                NO_SAMPLES,
                |a, b| {
                    ctx.cost_per_change(a, b, |c| {
                        (outcome_of(c, ctx.now_ms) == "successful")
                            .then(|| shipped_ms(&c.legs))
                            .flatten()
                    })
                },
            ),
        ),
        (
            "review_cost",
            run(
                ctx,
                "cost_ledger pr.reviewed spend, USD per reviewed change",
                NO_SAMPLES,
                |a, b| ctx.review_cost(a, b),
            ),
        ),
        (
            "gate_flake_rate",
            run(
                ctx,
                "pipeline_events gate.finished: commits that failed then passed over commits with a terminal gate, percent",
                NO_SAMPLES,
                |a, b| ctx.gate_flake_rate(a, b),
            ),
        ),
    ];
    if let Some((_, metric)) = all.iter_mut().find(|(name, _)| *name == "flow_efficiency") {
        let shipped = ctx.shipped_lots(ctx.start(), ctx.now_ms).count();
        let unmeasured = ctx
            .shipped_lots(ctx.start(), ctx.now_ms)
            .filter(|(lot, _, _)| {
                lot.visits
                    .iter()
                    .any(|v| v.left_ms.is_some() && v.started_ms.is_none())
            })
            .count();
        metric.detail = Some(
            json!({"shipped_lots":shipped,"unmeasured_execution_lots":unmeasured,"fully_measured_samples":metric.sample_size}),
        );
        if metric.value.is_none() && unmeasured > 0 {
            metric.unknown = Some(Unknown {
                reason: "execution_starts_not_recorded",
            });
        }
    }
    for (name, source, reason) in UNRECORDED {
        all.push((name, unrecorded(source, ctx.days, reason)));
    }
    all
}
