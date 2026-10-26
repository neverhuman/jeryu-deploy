//! The arithmetic behind `GET /api/v1/yield`: todos and review spend in, the
//! report out. Nothing here reads a store, so every rule is testable on
//! invented todos.
//!
//! The unit of work is a todo. Author spend goes to each attempt's group; a
//! todo's outcome (merged, first pass) and its review and triage spend go to
//! the group of its landing attempt: the last `done` attempt, else the last.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::Serialize;
use serde_json::Value;

use super::adhoc::{self, Adhoc};
use crate::web::pipeline::{LedgerRow, SpendRow};
use crate::web::shift::{Attempt, ShiftTodo, TodoStatus};

pub(crate) const SCHEMA: &str = "jeryu.yield/v1";
/// Merged todos an experiment arm needs before its row is worth comparing.
pub(crate) const ENOUGH_MERGED: u64 = 20;
const TOP: usize = 10;
/// Outcomes that say nothing about the work: the attempt never got to try.
const NOT_A_TRY: &[&str] = &["ratelimit", "retarget", "killed"];
const DEFAULT_AGENT: &str = "claude";

/// How the report groups spend (`?by=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum By {
    Model,
    Agent,
    Experiment,
    Family,
    Repo,
    Requester,
    Week,
}

impl By {
    pub(crate) const NAMES: &'static [&'static str] = &[
        "model",
        "agent",
        "experiment",
        "family",
        "repo",
        "requester",
        "week",
    ];

    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "model" => Self::Model,
            "agent" => Self::Agent,
            "experiment" => Self::Experiment,
            "family" => Self::Family,
            "repo" => Self::Repo,
            "requester" => Self::Requester,
            "week" => Self::Week,
            _ => return None,
        })
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Agent => "agent",
            Self::Experiment => "experiment",
            Self::Family => "family",
            Self::Repo => "repo",
            Self::Requester => "requester",
            Self::Week => "week",
        }
    }

    /// The group one attempt of `todo` belongs to.
    fn key(self, todo: &ShiftTodo, attempt: &Attempt) -> String {
        let or = |value: &str, none: &str| {
            if value.trim().is_empty() {
                none.to_string()
            } else {
                value.to_string()
            }
        };
        match self {
            Self::Model => or(&attempt.model, "(unknown)"),
            Self::Agent => agent_of(attempt).to_string(),
            Self::Experiment => match &attempt.experiment {
                Some(experiment) => format!(
                    "{experiment}/{}:{}",
                    agent_of(attempt),
                    or(&attempt.model, "(unknown)")
                ),
                None => "(not routed)".to_string(),
            },
            Self::Family => or(&todo.family, "(unknown)"),
            Self::Repo => match todo.repos.first() {
                Some(repo) => or(repo, "(none)"),
                None => "(none)".to_string(),
            },
            Self::Requester => or(&todo.requested_by, "(unknown)"),
            Self::Week => week_label(attempt),
        }
    }
}

/// The days a report covers, both ends included (UTC).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Window {
    pub since: NaiveDate,
    pub until: NaiveDate,
}

impl Window {
    pub(super) fn contains(self, day: NaiveDate) -> bool {
        self.since <= day && day <= self.until
    }
}

/// `Row` of the contract: one group's spend against what reached main.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct Row {
    pub todos: u64,
    pub attempts: u64,
    pub merged: u64,
    pub first_pass: u64,
    pub reworked: u64,
    pub rework_usd: f64,
    pub spend_usd: f64,
    pub author_usd: f64,
    pub review_usd: f64,
    pub triage_usd: f64,
    pub main_usd: f64,
    pub scrap_usd: f64,
    pub pending_usd: f64,
    pub killed_usd: f64,
    pub estimated_usd: f64,
    pub r#yield: Option<f64>,
    pub usd_per_merged: Option<f64>,
    pub usd_per_merged_ci95: Option<[f64; 2]>,
    pub first_pass_rate: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct GroupRow {
    pub group: String,
    #[serde(flatten)]
    pub row: Row,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct WeekRow {
    pub week: String,
    pub group: String,
    pub merged: u64,
    pub main_usd: f64,
    pub usd_per_merged: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Experiment {
    pub experiment: String,
    pub arms: Vec<Arm>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Arm {
    pub agent: String,
    pub model: String,
    pub arm_p: Option<f64>,
    pub enough_data: bool,
    pub row: Row,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct TodoRef {
    pub id: String,
    pub family: String,
    pub title: String,
    pub status: TodoStatus,
    pub merged: bool,
    pub rework: bool,
    pub spend_usd: f64,
    pub attempts: u64,
    pub agent: String,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct YieldReport {
    pub schema_version: &'static str,
    pub generated_at: String,
    pub window: Window,
    pub family: Option<String>,
    pub by: &'static str,
    pub price_sources: Vec<String>,
    pub totals: Row,
    pub groups: Vec<GroupRow>,
    pub weekly: Vec<WeekRow>,
    pub experiments: Vec<Experiment>,
    pub costliest: Vec<TodoRef>,
    pub scrap: Vec<TodoRef>,
    /// Spend of agent sessions run by hand, outside the todo economics:
    /// never part of `totals`, `groups`, `weekly` or `experiments`.
    pub adhoc: Adhoc,
    /// What review caught against what escaped it, by reviewer kind; the
    /// handler fills it from the event log (`review_catch`).
    pub review_catch: crate::web::review_catch::ReviewCatch,
    /// Lead time, exception rate and bad-change rate over the same window;
    /// the handler fills it from the event log (`headline`).
    pub headline: crate::web::headline::Headline,
}

/// Where a todo ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Main,
    Scrap,
    Pending,
}

impl End {
    fn of(todo: &ShiftTodo) -> Self {
        if todo.merged {
            Self::Main
        } else if matches!(
            todo.status,
            TodoStatus::Blocked | TodoStatus::Closed | TodoStatus::Handoff
        ) {
            Self::Scrap
        } else {
            Self::Pending
        }
    }
}

/// One in-window todo and what the report needs to know about it.
struct Facts<'a> {
    todo: &'a ShiftTodo,
    end: End,
    landing: &'a Attempt,
    review: f64,
    triage: f64,
    first_pass: bool,
    rework: bool,
}

impl Facts<'_> {
    fn author(&self) -> f64 {
        self.todo.worked_by.iter().map(cost).sum()
    }

    fn spend(&self) -> f64 {
        self.author() + self.review + self.triage
    }
}

/// Running sums for one group.
#[derive(Default)]
struct Acc {
    todos: u64,
    attempts: u64,
    merged: u64,
    first_pass: u64,
    reworked: u64,
    rework: f64,
    author: f64,
    review: f64,
    triage: f64,
    main: f64,
    scrap: f64,
    pending: f64,
    killed: f64,
    estimated: f64,
    /// Whole spend of each merged todo that landed in this group.
    merged_spend: Vec<(String, f64)>,
}

impl Acc {
    fn add_end(&mut self, end: End, usd: f64) {
        match end {
            End::Main => self.main += usd,
            End::Scrap => self.scrap += usd,
            End::Pending => self.pending += usd,
        }
    }

    fn row(&self) -> Row {
        let spend = self.author + self.review + self.triage;
        let kept = self.main + self.scrap;
        let merged = self.merged as f64;
        Row {
            todos: self.todos,
            attempts: self.attempts,
            merged: self.merged,
            first_pass: self.first_pass,
            reworked: self.reworked,
            rework_usd: money(self.rework),
            spend_usd: money(spend),
            author_usd: money(self.author),
            review_usd: money(self.review),
            triage_usd: money(self.triage),
            main_usd: money(self.main),
            scrap_usd: money(self.scrap),
            pending_usd: money(self.pending),
            killed_usd: money(self.killed),
            estimated_usd: money(self.estimated),
            r#yield: (kept > 0.0).then(|| rate(self.main / kept)),
            usd_per_merged: (self.merged > 0).then(|| money((self.main + self.rework) / merged)),
            usd_per_merged_ci95: ci95(
                &self
                    .merged_spend
                    .iter()
                    .map(|(_, spend)| *spend)
                    .collect::<Vec<_>>(),
            ),
            first_pass_rate: (self.merged > 0).then(|| rate(self.first_pass as f64 / merged)),
        }
    }
}

/// Mean ± 1.96·sd/√n (sample standard deviation); none below two samples.
fn ci95(samples: &[f64]) -> Option<[f64; 2]> {
    if samples.len() < 2 {
        return None;
    }
    let n = samples.len() as f64;
    let mean = samples.iter().sum::<f64>() / n;
    let variance = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let half = 1.96 * variance.sqrt() / n.sqrt();
    Some([money(mean - half), money(mean + half)])
}

pub(super) fn money(usd: f64) -> f64 {
    (usd * 1e6).round() / 1e6
}

fn rate(value: f64) -> f64 {
    (value * 1e6).round() / 1e6
}

fn cost(attempt: &Attempt) -> f64 {
    attempt.cost_usd.unwrap_or(0.0)
}

fn agent_of(attempt: &Attempt) -> &str {
    match attempt.agent.as_deref() {
        Some(agent) if !agent.trim().is_empty() => agent,
        _ => DEFAULT_AGENT,
    }
}

/// The UTC day an attempt started, read off its RFC 3339 `started`.
fn started_day(attempt: &Attempt) -> Option<NaiveDate> {
    match DateTime::parse_from_rfc3339(attempt.started.trim()) {
        Ok(at) => Some(at.with_timezone(&Utc).date_naive()),
        Err(_) => None,
    }
}

/// The Monday of the week `day` is in.
pub(super) fn monday(day: NaiveDate) -> NaiveDate {
    day - Duration::days(i64::from(day.weekday().num_days_from_monday()))
}

/// The Monday (UTC) of the week an attempt started.
fn week_of(attempt: &Attempt) -> Option<NaiveDate> {
    started_day(attempt).map(monday)
}

/// [`week_of`] as a group name; an attempt without a readable start is
/// `(unknown)`.
fn week_label(attempt: &Attempt) -> String {
    match week_of(attempt) {
        Some(week) => week.to_string(),
        None => "(unknown)".to_string(),
    }
}

/// The attempt a todo's outcome is credited to: its last `done`, else its last.
fn landing(todo: &ShiftTodo) -> Option<&Attempt> {
    let last_done = todo
        .worked_by
        .iter()
        .rev()
        .find(|attempt| attempt.outcome == "done");
    match last_done {
        Some(attempt) => Some(attempt),
        None => todo.worked_by.last(),
    }
}

/// The repository name a ledger row's `owner/name` names, which is how a
/// todo's shift pull request spells its repo.
fn repo_name(repo: &str) -> &str {
    match repo.rsplit_once('/') {
        Some((_, name)) => name,
        None => repo,
    }
}

/// Every `cost_basis` under a `usage` object (`{model: {cost_basis, ...}}`).
fn price_sources_of(usage: Option<&Value>, into: &mut BTreeSet<String>) {
    let Some(Value::Object(models)) = usage else {
        return;
    };
    for entry in models.values() {
        if let Some(basis) = entry.get("cost_basis").and_then(Value::as_str) {
            let basis = basis.trim();
            if !basis.is_empty() {
                into.insert(basis.to_string());
            }
        }
    }
}

/// Group every attempt and todo-level amount by `key`. Author spend goes to
/// each attempt's key, everything else to the landing attempt's key.
fn aggregate<K: Ord>(
    facts: &[Facts<'_>],
    key: impl Fn(&ShiftTodo, &Attempt) -> K,
) -> BTreeMap<K, Acc> {
    let mut groups: BTreeMap<K, Acc> = BTreeMap::new();
    for fact in facts {
        for attempt in &fact.todo.worked_by {
            let acc = groups.entry(key(fact.todo, attempt)).or_default();
            let usd = cost(attempt);
            acc.attempts += 1;
            acc.author += usd;
            acc.add_end(fact.end, usd);
            if attempt.outcome == "killed" {
                acc.killed += usd;
            }
            if attempt.cost_estimated == Some(true) {
                acc.estimated += usd;
            }
        }
        let acc = groups.entry(key(fact.todo, fact.landing)).or_default();
        acc.todos += 1;
        acc.review += fact.review;
        acc.triage += fact.triage;
        acc.add_end(fact.end, fact.review + fact.triage);
        if fact.end == End::Main {
            acc.merged += 1;
            acc.first_pass += u64::from(fact.first_pass);
            acc.merged_spend.push((fact.todo.id.clone(), fact.spend()));
        }
    }
    let by_id: BTreeMap<_, _> = facts
        .iter()
        .map(|fact| ((fact.todo.family.as_str(), fact.todo.id.as_str()), fact))
        .collect();
    for fact in facts.iter().filter(|fact| !fact.todo.reworks.is_empty()) {
        for target_id in &fact.todo.reworks {
            if let Some(target) = by_id.get(&(fact.todo.family.as_str(), target_id.as_str())) {
                let acc = groups.entry(key(target.todo, target.landing)).or_default();
                acc.reworked += 1;
                acc.rework += fact.spend();
                if target.end == End::Main
                    && let Some((_, spend)) = acc
                        .merged_spend
                        .iter_mut()
                        .find(|(id, _)| id == target.todo.id.as_str())
                {
                    *spend += fact.spend();
                }
            }
        }
    }
    groups
}

fn todo_ref(fact: &Facts<'_>) -> TodoRef {
    TodoRef {
        id: fact.todo.id.clone(),
        family: fact.todo.family.clone(),
        title: fact.todo.title.clone(),
        status: fact.todo.status,
        merged: fact.todo.merged,
        rework: fact.rework,
        spend_usd: money(fact.spend()),
        attempts: fact.todo.worked_by.len() as u64,
        agent: agent_of(fact.landing).to_string(),
        model: fact.landing.model.clone(),
    }
}

fn top(facts: &[&Facts<'_>]) -> Vec<TodoRef> {
    let mut refs: Vec<TodoRef> = facts.iter().map(|fact| todo_ref(fact)).collect();
    refs.sort_by(|a, b| {
        b.spend_usd
            .total_cmp(&a.spend_usd)
            .then_with(|| a.id.cmp(&b.id))
    });
    refs.truncate(TOP);
    refs
}

/// What the report is asked for.
pub(crate) struct Request {
    pub window: Window,
    pub family: Option<String>,
    pub by: By,
    pub now: DateTime<Utc>,
}

/// Whether a todo was worked in `window`.
fn worked_in(window: Window, todo: &ShiftTodo) -> bool {
    todo.worked_by
        .iter()
        .filter_map(started_day)
        .any(|day| window.contains(day))
}

/// The whole report. `todos` are every todo of the family (or of every
/// family) with the merged truth already derived; `reviews` are the ledger's
/// `pr.reviewed` rows. Review spend is split over every todo a pull request
/// carries, inside the window or not, so a long-running shift is not charged
/// twice to the todos that happen to be in view. `sessions` are the ledger's
/// `session.costed` rows, summed apart into `adhoc`.
/// Whether the report covers a todo: it has a landing attempt and was worked
/// in `window`. `build` and the headline numbers share this cut.
pub(crate) fn in_report(window: Window, todo: &ShiftTodo) -> bool {
    landing(todo).is_some() && worked_in(window, todo)
}

pub(crate) fn build(
    request: &Request,
    todos: &[ShiftTodo],
    reviews: &[LedgerRow],
    sessions: &[SpendRow],
) -> YieldReport {
    let mut review_by_pr: BTreeMap<(String, u64), (f64, Vec<&LedgerRow>)> = BTreeMap::new();
    for row in reviews {
        let (Some(repo), Some(pr)) = (&row.repo, row.pr) else {
            continue;
        };
        let Ok(pr) = u64::try_from(pr) else {
            continue;
        };
        let entry = review_by_pr
            .entry((repo_name(repo).to_string(), pr))
            .or_default();
        entry.0 += row.cost_usd;
        entry.1.push(row);
    }
    let mut carriers: BTreeMap<(String, u64), u64> = BTreeMap::new();
    for todo in todos {
        let mut seen = BTreeSet::new();
        for pr in &todo.prs {
            let key = (pr.repo.clone(), pr.number);
            if review_by_pr.contains_key(&key) && seen.insert(key.clone()) {
                *carriers.entry(key).or_default() += 1;
            }
        }
    }

    let reworked: BTreeSet<(&str, &str)> = todos
        .iter()
        .flat_map(|todo| {
            todo.reworks
                .iter()
                .map(|id| (todo.family.as_str(), id.as_str()))
        })
        .collect();
    let mut sources = BTreeSet::new();
    let mut facts = Vec::new();
    for todo in todos {
        let Some(landing) = landing(todo).filter(|_| worked_in(request.window, todo)) else {
            continue;
        };
        let mut review = 0.0;
        let mut seen = BTreeSet::new();
        for pr in &todo.prs {
            let key = (pr.repo.clone(), pr.number);
            if !seen.insert(key.clone()) {
                continue;
            }
            if let (Some((usd, rows)), Some(share)) = (review_by_pr.get(&key), carriers.get(&key)) {
                review += usd / *share as f64;
                for row in rows {
                    price_sources_of(
                        row.detail.as_ref().and_then(|detail| detail.get("usage")),
                        &mut sources,
                    );
                }
            }
        }
        for attempt in &todo.worked_by {
            price_sources_of(attempt.usage.as_ref(), &mut sources);
        }
        let tries = todo
            .worked_by
            .iter()
            .filter(|attempt| !NOT_A_TRY.contains(&attempt.outcome.as_str()))
            .count();
        facts.push(Facts {
            todo,
            end: End::of(todo),
            landing,
            review,
            triage: todo.triage_cost_usd.unwrap_or(0.0),
            first_pass: tries == 1 && !reworked.contains(&(todo.family.as_str(), todo.id.as_str())),
            rework: !todo.reworks.is_empty(),
        });
    }

    let totals = match aggregate(&facts, |_, _| ()).remove(&()) {
        Some(acc) => acc.row(),
        None => Acc::default().row(),
    };
    let by = request.by;
    let mut groups: Vec<GroupRow> = aggregate(&facts, |todo, attempt| by.key(todo, attempt))
        .into_iter()
        .map(|(group, acc)| GroupRow {
            group,
            row: acc.row(),
        })
        .collect();
    groups.sort_by(|a, b| {
        b.row
            .spend_usd
            .total_cmp(&a.row.spend_usd)
            .then_with(|| a.group.cmp(&b.group))
    });

    let weekly = aggregate(&facts, |todo, attempt| {
        let week = week_label(attempt);
        let group = if by == By::Week {
            "all".to_string()
        } else {
            by.key(todo, attempt)
        };
        (week, group)
    })
    .into_iter()
    .map(|((week, group), acc)| {
        let row = acc.row();
        WeekRow {
            week,
            group,
            merged: row.merged,
            main_usd: row.main_usd,
            usd_per_merged: row.usd_per_merged,
        }
    })
    .collect();

    let mut arm_p: BTreeMap<(String, String, String), f64> = BTreeMap::new();
    for fact in &facts {
        for attempt in &fact.todo.worked_by {
            if let (Some(experiment), Some(p)) = (&attempt.experiment, attempt.arm_p) {
                arm_p.insert(
                    (
                        experiment.clone(),
                        agent_of(attempt).to_string(),
                        attempt.model.clone(),
                    ),
                    p,
                );
            }
        }
    }
    let mut experiments: Vec<Experiment> = Vec::new();
    for (key, acc) in aggregate(&facts, |_, attempt| {
        attempt.experiment.as_ref().map(|experiment| {
            (
                experiment.clone(),
                agent_of(attempt).to_string(),
                attempt.model.clone(),
            )
        })
    }) {
        let Some((experiment, agent, model)) = key else {
            continue;
        };
        let row = acc.row();
        let arm = Arm {
            arm_p: arm_p
                .get(&(experiment.clone(), agent.clone(), model.clone()))
                .copied(),
            agent,
            model,
            enough_data: row.merged >= ENOUGH_MERGED,
            row,
        };
        match experiments.last_mut() {
            Some(last) if last.experiment == experiment => last.arms.push(arm),
            _ => experiments.push(Experiment {
                experiment,
                arms: vec![arm],
            }),
        }
    }

    let all: Vec<&Facts<'_>> = facts.iter().collect();
    let scrapped: Vec<&Facts<'_>> = facts.iter().filter(|f| f.end == End::Scrap).collect();
    YieldReport {
        schema_version: SCHEMA,
        generated_at: request
            .now
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        window: request.window,
        family: request.family.clone(),
        by: by.as_str(),
        price_sources: sources.into_iter().collect(),
        totals,
        groups,
        weekly,
        experiments,
        costliest: top(&all),
        scrap: top(&scrapped),
        adhoc: adhoc::summarise(request.window, request.family.as_deref(), sessions),
        review_catch: Default::default(),
        headline: Default::default(),
    }
}
