//! Whether a live worker backs a claim at Work.
//!
//! A claim opens a Work visit (`todo.claimed`, then `worker.stage`), and only
//! `todo.attempt_finished` or a landing closes it. A worker that died, was
//! stopped or moved on without reporting leaves the visit open, so the
//! ledger alone cannot tell work in progress from an abandoned claim. Shift
//! heartbeats can: a slot that is working a todo names it on every beat.
//!
//! The answer for one unheld lot at Work, read only from the heartbeats:
//! - `live`: a slot whose newest beat is healthy (`shift::HEALTHY_MS`) names
//!   the todo.
//! - `no_live_worker`: no such slot, while slots of the lot's family have
//!   reported within the heartbeat retention. Those slots would name the
//!   todo if they were working it.
//! - `unknown`: nothing of the lot's family has reported within the
//!   retention, or the ledger recorded no family for the lot. A family whose
//!   workers report nowhere here says nothing about its claims.

use std::collections::BTreeSet;

use serde::Serialize;

use super::report::{FlowReport, Visit};
use super::stations::Station;
use crate::web::shift::{HEALTHY_MS, StoredHeartbeat};

pub(crate) const LIVE: &str = "live";
pub(crate) const NO_LIVE_WORKER: &str = "no_live_worker";
pub(crate) const UNKNOWN: &str = "unknown";

/// How the unheld claims at Work split by [`Evidence::claim`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ClaimCounts {
    pub live: u32,
    pub no_live_worker: u32,
    pub unknown: u32,
}

/// What the heartbeats say, read once per request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Evidence {
    /// Todos a slot whose newest beat is healthy names now.
    live_todos: BTreeSet<String>,
    /// Canonical families with at least one slot inside the retention.
    reporting: BTreeSet<String>,
}

impl Evidence {
    /// `latest` is the newest beat of every slot inside the heartbeat
    /// retention (`HeartbeatStore::latest`).
    pub(crate) fn from_heartbeats(latest: &[StoredHeartbeat], now_ms: i64) -> Self {
        let mut evidence = Self::default();
        for row in latest {
            let family = crate::web::family::canonical(&row.heartbeat.family);
            if !family.is_empty() {
                evidence.reporting.insert(family);
            }
            let healthy = now_ms.saturating_sub(row.received_ms) <= HEALTHY_MS;
            if let Some(todo) = row.heartbeat.todo_id.as_ref().filter(|_| healthy) {
                evidence.live_todos.insert(todo.clone());
            }
        }
        evidence
    }

    fn claim(&self, visit: &Visit) -> &'static str {
        if self.live_todos.contains(&visit.lot_id) {
            return LIVE;
        }
        let family = visit
            .family
            .as_deref()
            .map(crate::web::family::canonical)
            .filter(|family| !family.is_empty());
        match family {
            Some(family) if self.reporting.contains(&family) => NO_LIVE_WORKER,
            _ => UNKNOWN,
        }
    }
}

/// An open, current, unheld Work visit: the claims this module judges.
fn is_claim(visit: &Visit, is_current: &impl Fn(&Visit) -> bool) -> bool {
    visit.station == Station::Work
        && visit.left_ms.is_none()
        && visit.direction != "hold"
        && is_current(visit)
}

/// Count every claim at Work (not only the listed ones) on the Work
/// station, and mark each listed unheld Work lot with its own answer.
pub(crate) fn mark(
    report: &mut FlowReport,
    visits: &[Visit],
    is_current: &impl Fn(&Visit) -> bool,
    evidence: &Evidence,
) {
    let mut counts = ClaimCounts::default();
    for visit in visits.iter().filter(|visit| is_claim(visit, is_current)) {
        match evidence.claim(visit) {
            LIVE => counts.live += 1,
            NO_LIVE_WORKER => counts.no_live_worker += 1,
            _ => counts.unknown += 1,
        }
    }
    if let Some(work) = report
        .stations
        .iter_mut()
        .find(|station| station.id == Station::Work.id())
    {
        work.claims = Some(counts);
    }
    for lot in report
        .lots
        .iter_mut()
        .filter(|lot| lot.station == Station::Work.id() && lot.state != "held")
    {
        lot.claim = visits
            .iter()
            .find(|visit| visit.lot_id == lot.lot_id && is_claim(visit, is_current))
            .map(|visit| evidence.claim(visit));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::web::flow::report::{LotsPage, Request, build_paged};
    use crate::web::shift::Heartbeat;

    const NOW: i64 = 1_793_000_000_000;
    const MINUTE: i64 = 60_000;

    fn beat(family: &str, slot: &str, todo: Option<&str>, age_ms: i64) -> StoredHeartbeat {
        StoredHeartbeat {
            received_ms: NOW - age_ms,
            heartbeat: Heartbeat {
                operator: "dana".into(),
                host: "build-1".into(),
                slot: slot.into(),
                family: family.into(),
                state: if todo.is_some() { "working" } else { "idle" }.into(),
                todo_id: todo.map(str::to_string),
                stage: None,
                lease_until: None,
                shift: None,
                planned_slots: None,
                schedule: None,
                version: None,
                kind: "todoq".into(),
                detail: None,
                capacity: None,
                active: None,
                usage: None,
            },
        }
    }

    /// A claimed lot a worker staged: open at Work, started.
    fn staged(lot: &str, family: Option<&str>, days_ago: i64) -> Visit {
        let entered = NOW - days_ago * 24 * 60 * MINUTE;
        Visit {
            lot_id: lot.into(),
            station: Station::Work,
            entered_ms: entered,
            started_ms: Some(entered + MINUTE),
            left_ms: None,
            direction: "open".into(),
            rework_to: None,
            repo: None,
            pr: None,
            hold: None,
            family: family.map(str::to_string),
        }
    }

    /// The shape behind the live 2026-10-26 answer: hundreds of "active" Work
    /// lots while `/api/v1/shift/workers` lists every slot idle or stopping.
    #[test]
    fn a_claim_no_healthy_beat_names_is_not_live_and_a_silent_family_is_unknown() {
        let visits = vec![
            staged("20260101-000001-aaaa01", Some("acme"), 3),
            staged("20260101-000002-aaaa02", Some("acme"), 7),
            staged("20261020-120000-cccccc", Some("Acme-split"), 6),
            staged("20261021-090000-dddddd", Some("globex"), 5),
            staged("20261022-090000-eeeeee", None, 4),
            Visit {
                direction: "hold".into(),
                ..staged("20261018-090000-ffffff", Some("acme"), 8)
            },
        ];
        let latest = vec![
            // Working aaaa01 now.
            beat("acme", "w1", Some("20260101-000001-aaaa01"), 20_000),
            // Named aaaa02, then went quiet ten minutes ago: not live.
            beat("acme", "w2", Some("20260101-000002-aaaa02"), 10 * MINUTE),
            beat("acme", "supervisor", None, 15_000),
        ];
        let evidence = Evidence::from_heartbeats(&latest, NOW);
        let request = Request {
            family: None,
            days: 28,
            now_ms: NOW,
        };
        let reached = BTreeSet::from([Station::Work]);
        let mut report = build_paged(&request, &visits, &reached, &|_| true, LotsPage::default());
        mark(&mut report, &visits, &|_| true, &evidence);

        let work = report
            .stations
            .iter()
            .find(|station| station.id == "work")
            .unwrap();
        assert_eq!(work.wip.active, 5, "the ledger still says five are active");
        assert_eq!(work.wip.held, 1);
        assert_eq!(
            work.claims,
            Some(ClaimCounts {
                live: 1,
                no_live_worker: 2,
                unknown: 2,
            }),
            "the held lot is no claim; either family spelling is acme"
        );
        let claim = |lot: &str| {
            report
                .lots
                .iter()
                .find(|row| row.lot_id == lot)
                .map(|row| row.claim)
                .unwrap()
        };
        assert_eq!(claim("20260101-000001-aaaa01"), Some(LIVE));
        assert_eq!(claim("20260101-000002-aaaa02"), Some(NO_LIVE_WORKER));
        assert_eq!(claim("20261020-120000-cccccc"), Some(NO_LIVE_WORKER));
        assert_eq!(claim("20261021-090000-dddddd"), Some(UNKNOWN));
        assert_eq!(claim("20261022-090000-eeeeee"), Some(UNKNOWN));
        assert_eq!(claim("20261018-090000-ffffff"), None, "held: not a claim");
        for other in report
            .stations
            .iter()
            .filter(|station| station.id != "work")
        {
            assert_eq!(other.claims, None, "{}", other.id);
        }
    }

    #[test]
    fn no_heartbeats_at_all_leaves_every_claim_unknown() {
        let visits = vec![staged("20260101-000001-aaaa01", Some("acme"), 3)];
        let request = Request {
            family: None,
            days: 28,
            now_ms: NOW,
        };
        let reached = BTreeSet::from([Station::Work]);
        let mut report = build_paged(&request, &visits, &reached, &|_| true, LotsPage::default());
        mark(&mut report, &visits, &|_| true, &Evidence::default());
        assert_eq!(report.lots[0].claim, Some(UNKNOWN));
    }
}
