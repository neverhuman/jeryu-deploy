//! `adhoc` of `GET /api/v1/yield`: what agent sessions run by hand spent,
//! read off the cost ledger's `session.costed` rows. This spend did no todo,
//! so it stands beside the todo economics and never enters them.
//!
//! Each row is one increase of one session's spend on one repository (or on
//! a family, or on nothing the producer could name). `detail` carries the
//! `session`, `agent` and `model`.

use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;
use serde::Serialize;
use serde_json::Value;

use super::report::{Window, monday, money};
use crate::web::family::canonical;
use crate::web::pipeline::SpendRow;

/// The ledger kind ad-hoc session spend is recorded under.
pub(crate) const SESSION_KIND: &str = "session.costed";
const UNKNOWN: &str = "(unknown)";

/// One group of ad-hoc spend.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct AdhocGroup {
    pub group: String,
    pub usd: f64,
    pub sessions: u64,
}

/// One repository's ad-hoc spend, with the family it belongs to.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct AdhocRepo {
    pub group: String,
    pub family: Option<String>,
    pub usd: f64,
    pub sessions: u64,
}

/// Ad-hoc spend in one week, keyed like the report's `weekly`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct AdhocWeek {
    pub week: String,
    pub usd: f64,
}

/// The `adhoc` block. Always present; zeros and empty lists when no session
/// spent anything in the window.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct Adhoc {
    pub total_usd: f64,
    pub sessions: u64,
    pub unattributed_usd: f64,
    pub by_family: Vec<AdhocGroup>,
    pub by_repo: Vec<AdhocRepo>,
    pub by_agent: Vec<AdhocGroup>,
    pub by_model: Vec<AdhocGroup>,
    pub weekly: Vec<AdhocWeek>,
}

/// Running sums for one group.
#[derive(Default)]
struct Sum<'a> {
    usd: f64,
    sessions: BTreeSet<&'a str>,
    family: Option<String>,
}

impl<'a> Sum<'a> {
    fn add(&mut self, usd: f64, session: Option<&'a str>) {
        self.usd += usd;
        if let Some(session) = session {
            self.sessions.insert(session);
        }
    }
}

fn text<'a>(detail: Option<&'a Value>, field: &str) -> Option<&'a str> {
    detail
        .and_then(|detail| detail.get(field))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Highest spend first; ties by group name.
fn ranked(sums: BTreeMap<String, Sum<'_>>) -> Vec<(String, Sum<'_>)> {
    let mut rows: Vec<_> = sums.into_iter().collect();
    rows.sort_by(|a, b| b.1.usd.total_cmp(&a.1.usd).then_with(|| a.0.cmp(&b.0)));
    rows
}

fn groups(sums: BTreeMap<String, Sum<'_>>) -> Vec<AdhocGroup> {
    ranked(sums)
        .into_iter()
        .map(|(group, sum)| AdhocGroup {
            group,
            usd: money(sum.usd),
            sessions: sum.sessions.len() as u64,
        })
        .collect()
}

/// Sum the `session.costed` rows recorded on a day of `window` and, when
/// `family` is given, whose family is that family (either spelling). A row's
/// family is the one the ledger recorded; the caller fills it from the
/// repository when the ledger has none.
pub(crate) fn summarise(window: Window, family: Option<&str>, rows: &[SpendRow]) -> Adhoc {
    let wanted = family.map(canonical);
    let mut total = 0.0;
    let mut unattributed = 0.0;
    let mut sessions = BTreeSet::new();
    let mut by_family: BTreeMap<String, Sum<'_>> = BTreeMap::new();
    let mut by_repo: BTreeMap<String, Sum<'_>> = BTreeMap::new();
    let mut by_agent: BTreeMap<String, Sum<'_>> = BTreeMap::new();
    let mut by_model: BTreeMap<String, Sum<'_>> = BTreeMap::new();
    let mut weekly: BTreeMap<String, f64> = BTreeMap::new();
    for row in rows {
        let Some(day) = DateTime::from_timestamp_millis(row.ts_ms).map(|at| at.date_naive()) else {
            continue;
        };
        if !window.contains(day) {
            continue;
        }
        let row_family = row
            .family
            .as_deref()
            .map(canonical)
            .filter(|key| !key.is_empty());
        if wanted.is_some() && row_family != wanted {
            continue;
        }
        let detail = row.detail.as_ref();
        let session = text(detail, "session");
        let usd = row.cost_usd;
        total += usd;
        if let Some(session) = session {
            sessions.insert(session);
        }
        let repo = row
            .repo
            .as_deref()
            .map(str::trim)
            .filter(|repo| !repo.is_empty());
        if repo.is_none() && row_family.is_none() {
            unattributed += usd;
        }
        if let Some(key) = &row_family {
            by_family.entry(key.clone()).or_default().add(usd, session);
        }
        if let Some(repo) = repo {
            let sum = by_repo.entry(repo.to_string()).or_default();
            sum.add(usd, session);
            if sum.family.is_none() {
                sum.family = row_family.clone();
            }
        }
        let agent = text(detail, "agent").unwrap_or(UNKNOWN);
        by_agent
            .entry(agent.to_string())
            .or_default()
            .add(usd, session);
        let model = text(detail, "model").unwrap_or(UNKNOWN);
        by_model
            .entry(model.to_string())
            .or_default()
            .add(usd, session);
        *weekly.entry(monday(day).to_string()).or_default() += usd;
    }
    Adhoc {
        total_usd: money(total),
        sessions: sessions.len() as u64,
        unattributed_usd: money(unattributed),
        by_family: groups(by_family),
        by_repo: ranked(by_repo)
            .into_iter()
            .map(|(group, sum)| AdhocRepo {
                group,
                family: sum.family,
                usd: money(sum.usd),
                sessions: sum.sessions.len() as u64,
            })
            .collect(),
        by_agent: groups(by_agent),
        by_model: groups(by_model),
        weekly: weekly
            .into_iter()
            .map(|(week, usd)| AdhocWeek {
                week,
                usd: money(usd),
            })
            .collect(),
    }
}
