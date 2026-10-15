//! The arithmetic behind `GET /api/v1/flow`: `flow_ledger` visits in, the
//! stations, the line and the oldest lots out.
//!
//! Every number is read from the visits, and a number the visits cannot
//! support is `null` with its reason in the station's `unknown` list: a
//! station nothing has ever reached says `no_events`, a metric with fewer
//! than [`MIN_SAMPLES`] samples in the window says `too_few_samples`, and
//! capacity waits on the heartbeat half of Flow (`no_heartbeat`). An
//! all-zero station while visits exist is a bug, not a quiet line.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::stations::Station;
use crate::web::pipeline::estimate::MIN_SAMPLES;
use crate::web::shift::rfc3339_ms;

/// `schema_version` of every `/api/v1/flow` answer.
pub(crate) const FLOW_SCHEMA: &str = "jeryu.flow/v1";
/// How many lots a station lists.
pub(crate) const LOTS_PER_STATION: usize = 20;

/// One visit of one lot to one station, as `flow_ledger` stores it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Visit {
    pub lot_id: String,
    pub station: Station,
    pub entered_ms: i64,
    pub started_ms: Option<i64>,
    pub left_ms: Option<i64>,
    pub direction: String,
    pub rework_to: Option<Station>,
}

impl Visit {
    fn held(&self) -> bool {
        self.direction == "hold"
    }

    fn left_forward(&self) -> bool {
        self.direction == "forward"
    }

    fn process_s(&self) -> Option<i64> {
        match (self.started_ms, self.left_ms) {
            (Some(started), Some(left)) => Some(seconds(left - started)),
            _ => None,
        }
    }
}

/// A `{p50, p90}` pair of seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Pair {
    pub p50: i64,
    pub p90: i64,
}

/// A `{p50, p90}` pair of ratios (the X-factor).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) struct Ratios {
    pub p50: f64,
    pub p90: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Wip {
    pub waiting: u32,
    pub active: u32,
    pub held: u32,
}

/// Lots this station sent back, and where to.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) struct Rework {
    pub to: &'static str,
    pub count: u32,
    /// The share of the lots that left this station in the window.
    pub rate: f64,
}

/// A number the visits cannot support, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Unknown {
    pub metric: &'static str,
    pub reason: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct StationReport {
    pub id: &'static str,
    pub label: &'static str,
    pub order: u8,
    pub wip: Wip,
    pub oldest_wait_s: Option<i64>,
    pub queue_s: Option<Pair>,
    pub process_s: Option<Pair>,
    pub throughput_per_hour: Option<f64>,
    pub first_pass_rate: Option<f64>,
    pub rework: Vec<Rework>,
    /// The tools serving the station; `GET /api/v1/flow/layout` reads the
    /// heartbeats this waits on, so it is null here.
    pub capacity: Option<serde_json::Value>,
    pub utilization: Option<f64>,
    pub bottleneck: bool,
    pub unknown: Vec<Unknown>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) struct Line {
    pub cycle_s: Option<Pair>,
    pub x_factor: Option<Ratios>,
    pub wip: u32,
    pub finished: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct LotReport {
    pub lot_id: String,
    pub station: &'static str,
    /// `waiting`, `active` or `held`.
    pub state: &'static str,
    pub since: String,
    /// The sum over the stations ahead that have an estimate; null when
    /// none has one (and for a held lot).
    pub eta_s: Option<Pair>,
    /// True when `eta_unknown_stations` is not empty: `eta_s` leaves out the
    /// stations named there and is a lower bound, not the whole remaining time.
    pub eta_partial: bool,
    /// The stations still ahead (the current one included) with no queue or
    /// process estimate, in route order. Their time is not in `eta_s`.
    pub eta_unknown_stations: Vec<&'static str>,
    pub slowest_ahead: Option<&'static str>,
    pub href: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct WindowReport {
    pub since: String,
    pub until: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct FlowReport {
    pub schema_version: &'static str,
    pub generated_at: String,
    pub family: Option<String>,
    pub days: i64,
    pub window: WindowReport,
    pub stations: Vec<StationReport>,
    pub line: Line,
    pub lots: Vec<LotReport>,
}

/// What a report is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub family: Option<String>,
    pub days: i64,
    pub now_ms: i64,
}

/// Whole seconds of a millisecond span, never negative.
fn seconds(ms: i64) -> i64 {
    (ms / 1000).max(0)
}

/// The p50 and p90 of `samples`, by the nearest-rank rule the gate estimate
/// uses, or `None` below [`MIN_SAMPLES`].
fn pair(mut samples: Vec<i64>) -> Option<Pair> {
    if samples.len() < MIN_SAMPLES {
        return None;
    }
    samples.sort_unstable();
    let rank = |percent: usize| -> i64 {
        let index = (samples.len() * percent).div_ceil(100).max(1) - 1;
        samples[index]
    };
    Some(Pair {
        p50: rank(50),
        p90: rank(90),
    })
}

/// The same for ratios: sorted to whole thousandths so the percentile is the
/// nearest rank of the samples, not an interpolation.
fn ratios(samples: &[f64]) -> Option<Ratios> {
    let thousandths: Vec<i64> = samples
        .iter()
        .map(|value| (value * 1000.0).round() as i64)
        .collect();
    pair(thousandths).map(|pair| Ratios {
        p50: pair.p50 as f64 / 1000.0,
        p90: pair.p90 as f64 / 1000.0,
    })
}

/// A rate rounded to whole hundredths, so a share reads as a share.
fn rate(numerator: u32, denominator: u32) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    (f64::from(numerator) / f64::from(denominator) * 100.0).round() / 100.0
}

/// The p50 of a station's queue plus process time: what one visit costs a lot
/// that still has to make it. `None` when either half has no estimate.
fn station_p50(station: &StationReport) -> Option<i64> {
    Some(station.queue_s?.p50 + station.process_s?.p50)
}

fn station_p90(station: &StationReport) -> Option<i64> {
    Some(station.queue_s?.p90 + station.process_s?.p90)
}

/// The first millisecond of a `days` window that ends at `now_ms`: the bound
/// `EventStore::flow_visits` reads from and the report counts from.
pub(crate) fn window_start(now_ms: i64, days: i64) -> i64 {
    now_ms - days * 24 * 60 * 60 * 1000
}

/// The Flow report for the visits of one family.
///
/// `visits` may be only the window's share of the ledger (see
/// `EventStore::flow_visits`); `reached` is every station the family's lots
/// have ever visited, so a station whose visits all predate the window still
/// reads as reached (its window metrics `no_events`) rather than as a station
/// nothing has ever reached.
pub(crate) fn build(
    request: &Request,
    visits: &[Visit],
    reached: &BTreeSet<Station>,
) -> FlowReport {
    let since_ms = window_start(request.now_ms, request.days);
    let in_window = |ms: i64| ms >= since_ms && ms <= request.now_ms;
    let hours = request.days as f64 * 24.0;

    let mut stations: Vec<StationReport> = Station::ALL
        .into_iter()
        .map(|station| {
            let own: Vec<&Visit> = visits
                .iter()
                .filter(|visit| visit.station == station)
                .collect();
            let ever = !own.is_empty() || reached.contains(&station);
            station_report(station, &own, ever, request.now_ms, &in_window, hours)
        })
        .collect();
    mark_bottleneck(&mut stations);

    FlowReport {
        schema_version: FLOW_SCHEMA,
        generated_at: rfc3339_ms(request.now_ms),
        family: request.family.clone(),
        days: request.days,
        window: WindowReport {
            since: rfc3339_ms(since_ms),
            until: rfc3339_ms(request.now_ms),
        },
        line: line(visits, request.now_ms, &in_window),
        lots: lots(visits, &stations, request.now_ms),
        stations,
    }
}

fn station_report(
    station: Station,
    own: &[&Visit],
    ever: bool,
    now_ms: i64,
    in_window: &impl Fn(i64) -> bool,
    hours: f64,
) -> StationReport {
    let mut report = StationReport {
        id: station.id(),
        label: station.label(),
        order: station.order(),
        wip: Wip::default(),
        oldest_wait_s: None,
        queue_s: None,
        process_s: None,
        throughput_per_hour: None,
        first_pass_rate: None,
        rework: Vec::new(),
        capacity: None,
        utilization: None,
        bottleneck: false,
        unknown: vec![Unknown {
            metric: "capacity",
            reason: "no_heartbeat",
        }],
    };
    if !ever {
        report.unknown.insert(
            0,
            Unknown {
                metric: "all",
                reason: "no_events",
            },
        );
        return report;
    }

    for visit in own.iter().filter(|visit| visit.left_ms.is_none()) {
        if visit.held() {
            report.wip.held += 1;
        } else if visit.started_ms.is_some() {
            report.wip.active += 1;
        } else {
            report.wip.waiting += 1;
        }
        let waited = seconds(now_ms - visit.entered_ms);
        report.oldest_wait_s = Some(report.oldest_wait_s.map_or(waited, |old| old.max(waited)));
    }

    let queue: Vec<i64> = own
        .iter()
        .filter_map(|visit| visit.started_ms.map(|started| (visit, started)))
        .filter(|(_, started)| in_window(*started))
        .map(|(visit, started)| seconds(started - visit.entered_ms))
        .collect();
    let queue_samples = queue.len();
    report.queue_s = pair(queue);

    let process: Vec<i64> = own
        .iter()
        .filter(|visit| visit.left_ms.is_some_and(in_window))
        .filter_map(|visit| visit.process_s())
        .collect();
    let process_samples = process.len();
    report.process_s = pair(process);

    let left: Vec<&Visit> = own
        .iter()
        .copied()
        .filter(|visit| visit.left_ms.is_some_and(in_window))
        .collect();
    let forward: Vec<&Visit> = left
        .iter()
        .copied()
        .filter(|visit| visit.left_forward())
        .collect();
    report.throughput_per_hour = Some((forward.len() as f64 / hours * 100.0).round() / 100.0);

    // A lot made it first time when this station never sent it back, over the
    // whole ledger: a loop a month ago is still a loop.
    let looped: BTreeSet<&str> = own
        .iter()
        .filter(|visit| visit.direction == "rework")
        .map(|visit| visit.lot_id.as_str())
        .collect();
    if forward.len() >= MIN_SAMPLES {
        let first_pass = forward
            .iter()
            .filter(|visit| !looped.contains(visit.lot_id.as_str()))
            .count();
        report.first_pass_rate = Some(rate(
            u32::try_from(first_pass).unwrap_or(u32::MAX),
            u32::try_from(forward.len()).unwrap_or(u32::MAX),
        ));
    }

    let mut back: BTreeMap<&'static str, u32> = BTreeMap::new();
    for visit in left.iter().filter(|visit| visit.direction == "rework") {
        let to = visit.rework_to.unwrap_or(station).id();
        *back.entry(to).or_default() += 1;
    }
    let left_count = u32::try_from(left.len()).unwrap_or(u32::MAX);
    report.rework = back
        .into_iter()
        .map(|(to, count)| Rework {
            to,
            count,
            rate: rate(count, left_count),
        })
        .collect();

    for (metric, samples, missing) in [
        ("queue_s", queue_samples, report.queue_s.is_none()),
        ("process_s", process_samples, report.process_s.is_none()),
        (
            "first_pass_rate",
            forward.len(),
            report.first_pass_rate.is_none(),
        ),
    ] {
        if missing {
            report.unknown.push(Unknown {
                metric,
                reason: if samples == 0 {
                    "no_events"
                } else {
                    "too_few_samples"
                },
            });
        }
    }
    report
}

/// The one station holding the most lot-hours of waiting.
fn mark_bottleneck(stations: &mut [StationReport]) {
    let mut worst: Option<(usize, f64)> = None;
    for (index, station) in stations.iter().enumerate() {
        let Some(queue) = station.queue_s else {
            continue;
        };
        let Some(throughput) = station.throughput_per_hour else {
            continue;
        };
        let load = queue.p50 as f64 * throughput;
        if load > 0.0 && worst.is_none_or(|(_, best)| load > best) {
            worst = Some((index, load));
        }
    }
    if let Some((index, _)) = worst {
        stations[index].bottleneck = true;
    }
}

/// Cycle time and X-factor over the lots that finished in the window, plus
/// what is on the line now.
fn line(visits: &[Visit], now_ms: i64, in_window: &impl Fn(i64) -> bool) -> Line {
    let mut by_lot: BTreeMap<&str, Vec<&Visit>> = BTreeMap::new();
    for visit in visits {
        by_lot.entry(visit.lot_id.as_str()).or_default().push(visit);
    }
    let open: u32 = u32::try_from(
        by_lot
            .values()
            .filter(|lot| lot.iter().any(|visit| visit.left_ms.is_none()))
            .count(),
    )
    .unwrap_or(u32::MAX);

    let mut cycles = Vec::new();
    let mut factors = Vec::new();
    let mut finished = 0u32;
    for lot in by_lot.values() {
        // Finished means the last station of the route let it go forward.
        let deployed = lot.iter().find(|visit| {
            visit.station == Station::Deploy
                && visit.left_forward()
                && visit.left_ms.is_some_and(in_window)
        });
        let Some(deployed) = deployed else {
            continue;
        };
        finished += 1;
        let filed = lot.iter().map(|visit| visit.entered_ms).min().unwrap_or(0);
        let left = deployed.left_ms.unwrap_or(now_ms);
        let cycle = seconds(left - filed);
        cycles.push(cycle);
        let worked: i64 = lot.iter().filter_map(|visit| visit.process_s()).sum();
        if worked > 0 {
            factors.push(cycle as f64 / worked as f64);
        }
    }
    Line {
        cycle_s: pair(cycles),
        x_factor: ratios(&factors),
        wip: open,
        finished,
    }
}

/// The oldest [`LOTS_PER_STATION`] lots at each station, with their ETA.
fn lots(visits: &[Visit], stations: &[StationReport], now_ms: i64) -> Vec<LotReport> {
    let mut lots = Vec::new();
    for station in Station::ALL {
        let mut open: Vec<&Visit> = visits
            .iter()
            .filter(|visit| visit.station == station && visit.left_ms.is_none())
            .collect();
        open.sort_by_key(|visit| (visit.entered_ms, visit.lot_id.clone()));
        for visit in open.into_iter().take(LOTS_PER_STATION) {
            let eta = if visit.held() {
                // A held lot's next step is a person, not a station.
                Eta::default()
            } else {
                eta(station, visit, stations, now_ms)
            };
            lots.push(LotReport {
                lot_id: visit.lot_id.clone(),
                station: station.id(),
                state: if visit.held() {
                    "held"
                } else if visit.started_ms.is_some() {
                    "active"
                } else {
                    "waiting"
                },
                since: rfc3339_ms(visit.entered_ms),
                eta_partial: !eta.unknown.is_empty(),
                eta_s: eta.known,
                eta_unknown_stations: eta.unknown,
                slowest_ahead: eta.slowest,
                href: lot_href(&visit.lot_id),
            });
        }
    }
    lots
}

/// Where a lot opens in the web app. A todo's lot is its Work page; a pull
/// request no todo carries (`<owner>/<repo>#<n>`) opens that pull request.
pub(crate) fn lot_href(lot_id: &str) -> String {
    if let Some((repo, number)) = lot_id.rsplit_once('#')
        && let Some((owner, name)) = repo.rsplit_once('/')
        && !owner.is_empty()
        && !name.is_empty()
        && number.parse::<u64>().is_ok()
    {
        return format!(
            "/{}/{}/pull/{number}",
            path_segment(owner),
            path_segment(name)
        );
    }
    format!("/work/{}", path_segment(lot_id))
}

/// One URL path segment: unreserved bytes kept, everything else escaped.
fn path_segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// What [`eta`] answers for one lot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Eta {
    /// The sum over the stations ahead that have an estimate.
    known: Option<Pair>,
    /// The stations ahead that have none, in route order.
    unknown: Vec<&'static str>,
    /// The slowest of the stations ahead that have an estimate.
    slowest: Option<&'static str>,
}

/// How long one lot still has, and which station ahead of it is the slowest.
///
/// The remaining time is this station's own p50 less the time already spent
/// here (never below zero) plus the p50 of every station still ahead, with
/// the p90s as the pessimistic bound. A station with no estimate is never
/// guessed at: it is named in `unknown` and left out of the sums, so the
/// answer is a partial ETA that says which stations it does not cover, rather
/// than either an optimistic total or nothing at all. With no estimate
/// anywhere ahead there is no ETA.
fn eta(at: Station, visit: &Visit, stations: &[StationReport], now_ms: i64) -> Eta {
    let mut answer = Eta::default();
    let mut p50 = 0;
    let mut p90 = 0;
    let mut known = false;
    let mut worst = 0;
    for station in stations
        .iter()
        .filter(|station| station.order >= at.order())
    {
        let (Some(station_p50), Some(station_p90)) = (station_p50(station), station_p90(station))
        else {
            answer.unknown.push(station.id);
            continue;
        };
        if !known || station_p50 > worst {
            answer.slowest = Some(station.id);
            worst = station_p50;
        }
        known = true;
        if station.id == at.id() {
            let spent = seconds(now_ms - visit.entered_ms);
            p50 += (station_p50 - spent).max(0);
            p90 += (station_p90 - spent).max(0);
        } else {
            p50 += station_p50;
            p90 += station_p90;
        }
    }
    if known {
        answer.known = Some(Pair { p50, p90 });
    }
    answer
}
