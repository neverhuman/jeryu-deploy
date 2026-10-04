//! One work trace: `GET /api/v1/trace`, "where is this piece of work".
//!
//! A todo and the pull request that carries it are the same work, and until
//! now nothing said so in one answer: the Work page knew the queue file, the
//! pull request page knew its own events, and the event log filtered by
//! `todo_id` or by `repo` and `pr` without ever joining the two. This route
//! joins them — by the `Todo: <id>` trailer every landing carries — and
//! answers with the twelve stages the work passes through, in order:
//!
//! `filed`, `claimed`, `done`, `shift`, `pr`, `gate`, `review`, `queue`,
//! `merged`, `pinned`, `staged`, `deployed`.
//!
//! Every stage says the same four things: what [`StageState`] it is in, when
//! that happened (`at`), where the answer came from ([`Source`]: a stored
//! event's `seq`, a fact derived from the repositories, or a report from
//! outside the forge) and the in-app `href` that explains it. Each also
//! carries the ids of the open attention items that belong to it, so a reader
//! who sees a stage stuck has the next step without a second call.
//!
//! The last three stages are what no surface could answer before. A repository
//! that ships by being pinned into a consumer (jeryu-web into jeryu-deploy)
//! has no release and no deployment of its own, so `shift/truth.rs` leaves its
//! work `released = null` for ever. Here `pinned` comes from the consumer's
//! lock (the same pins `/api/v1/pins` reports) and `staged` and `deployed`
//! from the consumer's own release and production deployment, each decided by
//! reading the lock at that exact consumer commit and asking whether the pin
//! it names reaches the work. Contract: `docs/pipeline-events.md`.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::super::WebState;
use super::super::merge_queue::{QueueEntry, QueueState};
use super::super::shift::{ShiftTodo, TodoStatus};
use super::super::workcells_support::{TypedError, typed_error};
use super::attention::{self, Item};
use super::types::{Event, EventsQuery};

/// `schema_version` of every `/api/v1/trace` answer.
pub(crate) const TRACE_SCHEMA: &str = "jeryu.trace/v1";
/// Most events one trace reads per join key; a stage only needs the newest of
/// its kinds, and a piece of work never has more steps than this.
const EVENT_LIMIT: i64 = 200;
const DOCS: &str = "docs/pipeline-events.md";

/// The stages of one piece of work, in the order it passes through them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stage {
    Filed,
    Claimed,
    Done,
    Shift,
    Pr,
    Gate,
    Review,
    Queue,
    Merged,
    Pinned,
    Staged,
    Deployed,
}

impl Stage {
    /// Every stage, in trace order: what the answer lists and what the tests
    /// walk. Product code names a stage, never the list.
    pub(crate) const ALL: [Self; 12] = [
        Self::Filed,
        Self::Claimed,
        Self::Done,
        Self::Shift,
        Self::Pr,
        Self::Gate,
        Self::Review,
        Self::Queue,
        Self::Merged,
        Self::Pinned,
        Self::Staged,
        Self::Deployed,
    ];
}

/// Where one stage stands. `not_applicable` is a stage this work never passes
/// through (nothing pins a repository that ships its own release); `unknown`
/// is a stage nothing the forge can read says anything about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StageState {
    Done,
    Active,
    Waiting,
    Blocked,
    Failed,
    Skipped,
    NotApplicable,
    Unknown,
}

/// Where a stage's answer came from. `event` carries the `seq` of the stored
/// event that said so, so a reader can go straight to it in
/// `GET /api/v1/events`; `derived` is read from the repositories the forge
/// hosts right now; `reported` is a fact another system filed with the forge
/// (a deployment, a pull request review).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Origin {
    Event,
    Derived,
    Reported,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Source {
    pub from: Origin,
    /// The stored event's sequence; absent on every other source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
}

impl Source {
    fn derived() -> Self {
        Self {
            from: Origin::Derived,
            seq: None,
        }
    }

    fn reported() -> Self {
        Self {
            from: Origin::Reported,
            seq: None,
        }
    }

    fn event(event: &Event) -> Self {
        Self {
            from: Origin::Event,
            seq: Some(event.seq),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct TraceStage {
    pub stage: Stage,
    pub state: StageState,
    /// When the stage reached this state, RFC 3339, or null when nothing dates
    /// it.
    pub at: Option<String>,
    pub source: Source,
    /// One human line: what this stage says about the work.
    pub summary: String,
    /// The in-app page that explains the stage.
    pub href: String,
    /// The ids of the open `GET /api/v1/attention` items that belong to this
    /// stage, so a stuck stage carries its own next step.
    pub attention: Vec<String>,
}

/// What the trace is about: the work, and the keys every other surface joins
/// it by.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Subject {
    /// Every todo the trace follows, oldest id first. A pull request that
    /// carries several todos traces them together.
    pub todos: Vec<String>,
    pub family: Option<String>,
    pub family_label: Option<String>,
    /// The repository the work lands in, `owner/name`.
    pub repo: Option<String>,
    pub pr: Option<u64>,
    pub shift: Option<String>,
    /// The repository whose release carries the work, `owner/name`, when the
    /// work's own repository ships by being pinned into another. Null when the
    /// repository releases itself.
    pub released_by: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct TraceResponse {
    pub schema_version: &'static str,
    pub generated_at: String,
    pub subject: Subject,
    pub stages: Vec<TraceStage>,
}

/// `GET /api/v1/trace?todo=<id>`, or `?repo=<owner/name>&pr=<number>`.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct TraceQuery {
    pub todo: Option<String>,
    pub repo: Option<String>,
    pub pr: Option<u64>,
}

/// The pull request the work rides on.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PrFacts {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    pub head_sha: String,
    /// The branch the work is on, and the branch it lands on.
    pub head_ref: String,
    pub base_ref: String,
    pub draft: bool,
    pub merged: bool,
    pub closed: bool,
    pub opened_at: Option<String>,
    pub merged_at: Option<String>,
    pub href: String,
}

/// The combined commit status of the pull request's head, when anything posts
/// statuses there at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Checks {
    Green,
    Failing,
    Pending,
}

/// The newest review verdict a person or an automation filed.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ReviewFact {
    pub approved: bool,
    pub changes_requested: bool,
    pub at: Option<String>,
}

/// One step of shipping, as the trace found it: the consumer commit (or
/// dependency pin) it looked at, when that happened, and whether what it
/// names reaches the work.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Shipped {
    pub sha: Option<String>,
    pub at: Option<String>,
    pub reaches: bool,
}

/// How the work reaches production. `consumer` is set only when the work's
/// repository ships by being pinned into another one.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ShipFacts {
    pub consumer: Option<String>,
    pub pinned: Option<Shipped>,
    pub staged: Option<Shipped>,
    pub deployed: Option<Shipped>,
    /// Set instead of `deployed` when the repository releases itself: the
    /// derived `released` of `GET /api/v1/shift/todos`.
    pub released: Option<bool>,
    pub released_at: Option<String>,
    /// The page that explains the release, whichever repository ships it.
    pub href: Option<String>,
}

/// Everything one trace is computed from. Collected once, impurely; the rules
/// below are pure functions of it, which is what the tests drive.
pub(super) struct Facts {
    pub todos: Vec<ShiftTodo>,
    pub family: Option<String>,
    /// `owner/name` of the repository the work lands in.
    pub repo: Option<String>,
    pub shift: Option<String>,
    pub pr: Option<PrFacts>,
    /// Every event for the todos or the pull request, newest first.
    pub events: Vec<Event>,
    pub queue: Vec<QueueEntry>,
    pub checks: Option<Checks>,
    pub review: Option<ReviewFact>,
    pub ship: ShipFacts,
    /// The open attention items that belong to this work.
    pub attention: Vec<Item>,
}

/// The newest event of any of `kinds`.
fn newest<'a>(events: &'a [Event], kinds: &[&str]) -> Option<&'a Event> {
    events
        .iter()
        .find(|event| kinds.contains(&event.kind.as_str()))
}

/// The oldest event of any of `kinds`: when a step first happened, not when it
/// last did.
fn oldest<'a>(events: &'a [Event], kinds: &[&str]) -> Option<&'a Event> {
    events
        .iter()
        .rev()
        .find(|event| kinds.contains(&event.kind.as_str()))
}

/// The attention kinds each stage owns. An item whose kind is in no stage
/// (a diverged mirror, a failing mirror push) is about the forge rather than
/// about one piece of work, and the trace leaves it to the inbox.
fn stage_of(kind: &str) -> Option<Stage> {
    Some(match kind {
        "todo_untriaged" | "todo_waiting_on_blocker" => Stage::Filed,
        "todo_stuck_claim" | "workers_down" | "shift_budget_spent" => Stage::Claimed,
        "todo_blocked" | "todo_handoff" | "todo_parked" => Stage::Done,
        "shift_without_pr" | "shift_stranded_work" => Stage::Shift,
        "pr_draft_waiting" => Stage::Pr,
        "pr_checks_failing" | "gate_runner_down" => Stage::Gate,
        "pr_changes_requested" | "pr_awaiting_approval" | "reviewer_stuck" => Stage::Review,
        "pr_ready_to_merge" | "queue_failed" | "queue_refused" | "queue_stuck" => Stage::Queue,
        "pin_behind" => Stage::Pinned,
        "release_staged" | "release_stage_failed" => Stage::Staged,
        "deploy_failed" => Stage::Deployed,
        _ => return None,
    })
}

/// The ids of this work's open attention items that belong to `stage`.
fn attention_ids(items: &[Item], stage: Stage) -> Vec<String> {
    items
        .iter()
        .filter(|item| stage_of(item.kind) == Some(stage))
        .map(|item| item.id.clone())
        .collect()
}

fn todo_href(facts: &Facts) -> String {
    match (facts.todos.first(), &facts.family) {
        (Some(todo), _) => format!("/work/{}?family={}", todo.id, todo.family),
        (None, Some(family)) => format!("/work?family={family}"),
        (None, None) => "/work".to_string(),
    }
}

fn pr_href(facts: &Facts) -> String {
    match &facts.pr {
        Some(pr) => pr.href.clone(),
        None => todo_href(facts),
    }
}

fn release_href(facts: &Facts) -> String {
    facts
        .ship
        .href
        .clone()
        .unwrap_or_else(|| "/releases".to_string())
}

struct Built {
    state: StageState,
    at: Option<String>,
    source: Source,
    summary: String,
}

fn built(
    state: StageState,
    at: Option<String>,
    source: Source,
    summary: impl Into<String>,
) -> Built {
    Built {
        state,
        at,
        source,
        summary: summary.into(),
    }
}

/// From one event: its timestamp and sequence, with a summary of its own.
fn from_event(state: StageState, event: &Event) -> Built {
    built(
        state,
        Some(event.ts.clone()),
        Source::event(event),
        event.summary.clone(),
    )
}

fn filed(facts: &Facts) -> Built {
    let Some(todo) = facts.todos.first() else {
        return built(
            StageState::Unknown,
            None,
            Source::derived(),
            "no todo carries this work: no commit of the pull request has a `Todo:` trailer",
        );
    };
    let at = facts
        .todos
        .iter()
        .map(|todo| todo.filed_at.clone())
        .min()
        .filter(|at| !at.is_empty());
    let summary = match facts.todos.len() {
        1 => format!("{} filed {}", todo.id, todo.title),
        count => format!("{count} todos were filed for this pull request"),
    };
    match oldest(&facts.events, &["todo.filed"]) {
        Some(event) => built(
            StageState::Done,
            Some(event.ts.clone()),
            Source::event(event),
            summary,
        ),
        None => built(StageState::Done, at, Source::derived(), summary),
    }
}

fn claimed(facts: &Facts) -> Built {
    let Some(todo) = facts.todos.first() else {
        return built(
            StageState::Unknown,
            None,
            Source::derived(),
            "no todo, so nothing claimed it",
        );
    };
    if let Some(event) = oldest(&facts.events, &["todo.claimed"]) {
        return from_event(StageState::Done, event);
    }
    let first_attempt = todo
        .worked_by
        .iter()
        .map(|attempt| attempt.started.clone())
        .filter(|started| !started.is_empty())
        .min();
    match todo.status {
        TodoStatus::Claimed => built(
            StageState::Active,
            first_attempt,
            Source::derived(),
            format!("{} is working on it", todo.claim_by),
        ),
        _ if first_attempt.is_some() => built(
            StageState::Done,
            first_attempt,
            Source::derived(),
            format!("worked by {}", todo.worked_by[0].by),
        ),
        TodoStatus::Open | TodoStatus::Parked | TodoStatus::Closed => built(
            StageState::Waiting,
            None,
            Source::derived(),
            "no worker slot has claimed it",
        ),
        _ => built(
            StageState::Done,
            None,
            Source::derived(),
            "it was claimed; the queue file kept no attempt",
        ),
    }
}

fn done(facts: &Facts) -> Built {
    let Some(todo) = facts.todos.first() else {
        return built(
            StageState::Unknown,
            None,
            Source::derived(),
            "no todo, so nothing finished it",
        );
    };
    let last_attempt = todo
        .worked_by
        .iter()
        .map(|attempt| attempt.ended.clone())
        .filter(|ended| !ended.is_empty())
        .max();
    let unfinished = facts
        .todos
        .iter()
        .find(|todo| todo.status != TodoStatus::Done);
    let Some(todo) = unfinished else {
        let count = facts.todos.len();
        let summary = match count {
            1 => "the worker finished the work".to_string(),
            _ => format!("all {count} todos are done"),
        };
        return match newest(&facts.events, &["todo.attempt_finished"]) {
            Some(event) if event.outcome.as_deref() == Some("done") => {
                from_event(StageState::Done, event)
            }
            _ => built(StageState::Done, last_attempt, Source::derived(), summary),
        };
    };
    let note = |fallback: &str| {
        let note = todo.note.trim();
        if note.is_empty() {
            fallback.to_string()
        } else {
            note.to_string()
        }
    };
    match todo.status {
        TodoStatus::Blocked | TodoStatus::Handoff => built(
            StageState::Blocked,
            last_attempt,
            Source::derived(),
            note("the todo stopped and needs a person"),
        ),
        TodoStatus::Claimed => built(
            StageState::Active,
            None,
            Source::derived(),
            format!("{} is working on it", todo.claim_by),
        ),
        TodoStatus::Closed => built(
            StageState::Skipped,
            None,
            Source::derived(),
            note("the todo was closed: it will not be done"),
        ),
        TodoStatus::Parked => built(
            StageState::Waiting,
            None,
            Source::derived(),
            note("the todo is parked"),
        ),
        _ => built(
            StageState::Waiting,
            None,
            Source::derived(),
            "the todo is waiting in the queue",
        ),
    }
}

fn shift(facts: &Facts) -> Built {
    match &facts.shift {
        Some(branch) => built(
            StageState::Done,
            None,
            Source::derived(),
            format!("the work is on {branch}"),
        ),
        None if facts
            .todos
            .iter()
            .any(|todo| todo.status == TodoStatus::Done) =>
        {
            built(
                StageState::Unknown,
                None,
                Source::derived(),
                "the todo is done and names no shift branch",
            )
        }
        None => built(
            StageState::Waiting,
            None,
            Source::derived(),
            "no shift branch carries it yet",
        ),
    }
}

fn pull(facts: &Facts) -> Built {
    let Some(pr) = &facts.pr else {
        return built(
            StageState::Waiting,
            None,
            Source::derived(),
            "no pull request carries the work yet",
        );
    };
    let opened = oldest(&facts.events, &["pr.opened"]);
    let at = opened
        .map(|event| event.ts.clone())
        .or_else(|| pr.opened_at.clone());
    let source = opened.map_or_else(Source::reported, Source::event);
    let where_ = format!("{}#{}", pr.repo, pr.number);
    if pr.merged {
        return built(StageState::Done, at, source, format!("{where_} is merged"));
    }
    if pr.closed {
        return built(
            StageState::Failed,
            at,
            source,
            format!("{where_} was closed without merging"),
        );
    }
    if pr.draft {
        return built(
            StageState::Waiting,
            at,
            source,
            format!("{where_} is still a draft"),
        );
    }
    built(StageState::Active, at, source, format!("{where_} is open"))
}

fn gate(facts: &Facts) -> Built {
    if let Some(event) = newest(
        &facts.events,
        &["gate.finished", "gate.started", "gate.log"],
    ) {
        let state = match (event.kind.as_str(), event.outcome.as_deref()) {
            ("gate.started", _) => StageState::Active,
            (_, Some("success")) => StageState::Done,
            (_, None) => StageState::Active,
            _ => StageState::Failed,
        };
        return from_event(state, event);
    }
    match facts.checks {
        Some(Checks::Green) => built(
            StageState::Done,
            None,
            Source::reported(),
            "every check on the head commit passed",
        ),
        Some(Checks::Failing) => built(
            StageState::Failed,
            None,
            Source::reported(),
            "a check on the head commit is failing",
        ),
        Some(Checks::Pending) => built(
            StageState::Active,
            None,
            Source::reported(),
            "the checks on the head commit are still running",
        ),
        None => built(
            StageState::Unknown,
            None,
            Source::derived(),
            "no gate reported on this work",
        ),
    }
}

fn review(facts: &Facts) -> Built {
    if let Some(event) = newest(
        &facts.events,
        &[
            "pr.approved",
            "pr.review",
            "review.finished",
            "review.started",
        ],
    ) {
        let state = match (event.kind.as_str(), event.outcome.as_deref()) {
            ("pr.approved", _) => StageState::Done,
            ("review.started", _) => StageState::Active,
            (_, Some("request_changes")) => StageState::Blocked,
            (_, Some("comment")) => StageState::Active,
            _ if event.needs_human => StageState::Blocked,
            (_, Some("approve")) | (_, Some("success")) => StageState::Done,
            _ => StageState::Active,
        };
        return from_event(state, event);
    }
    match &facts.review {
        Some(review) if review.changes_requested => built(
            StageState::Blocked,
            review.at.clone(),
            Source::reported(),
            "a reviewer asked for changes",
        ),
        Some(review) if review.approved => built(
            StageState::Done,
            review.at.clone(),
            Source::reported(),
            "the pull request is approved",
        ),
        Some(review) => built(
            StageState::Active,
            review.at.clone(),
            Source::reported(),
            "a reviewer has commented, with no verdict yet",
        ),
        None if facts.pr.is_some() => built(
            StageState::Waiting,
            None,
            Source::derived(),
            "nobody has reviewed it yet",
        ),
        None => built(
            StageState::Unknown,
            None,
            Source::derived(),
            "no pull request, so nothing to review",
        ),
    }
}

fn queue(facts: &Facts) -> Built {
    if let Some(event) = newest(
        &facts.events,
        &[
            "queue.landed",
            "queue.failed",
            "queue.refused",
            "queue.dequeued",
            "queue.building",
            "queue.enqueued",
        ],
    ) {
        let state = match event.kind.as_str() {
            "queue.landed" => StageState::Done,
            "queue.failed" | "queue.refused" => StageState::Failed,
            "queue.dequeued" => StageState::Waiting,
            _ => StageState::Active,
        };
        return from_event(state, event);
    }
    let entry = facts
        .queue
        .iter()
        .max_by(|a, b| a.enqueued_at.cmp(&b.enqueued_at));
    match entry {
        Some(entry) => {
            let (state, said) = match entry.state {
                QueueState::Landed => (StageState::Done, "the merge queue landed it".to_string()),
                QueueState::Building => (
                    StageState::Active,
                    "the merge queue is building it".to_string(),
                ),
                QueueState::Failed | QueueState::Dequeued => (
                    StageState::Failed,
                    entry
                        .reason
                        .clone()
                        .unwrap_or_else(|| "the merge queue dropped it".to_string()),
                ),
            };
            built(
                state,
                Some(entry.enqueued_at.clone()),
                Source::derived(),
                said,
            )
        }
        None if facts.pr.as_ref().is_some_and(|pr| pr.merged) => built(
            StageState::Skipped,
            None,
            Source::derived(),
            "it merged without going through the merge queue",
        ),
        None => built(
            StageState::Waiting,
            None,
            Source::derived(),
            "it is not in the merge queue",
        ),
    }
}

fn merged(facts: &Facts) -> Built {
    let derived = !facts.todos.is_empty() && facts.todos.iter().all(|todo| todo.merged);
    if let Some(event) = newest(&facts.events, &["pr.merged", "todo.merged"]) {
        return from_event(StageState::Done, event);
    }
    if derived {
        return built(
            StageState::Done,
            facts.pr.as_ref().and_then(|pr| pr.merged_at.clone()),
            Source::derived(),
            "every commit of the work is on the base branch",
        );
    }
    if facts.pr.as_ref().is_some_and(|pr| pr.merged) {
        return built(
            StageState::Done,
            facts.pr.as_ref().and_then(|pr| pr.merged_at.clone()),
            Source::reported(),
            "the pull request merged",
        );
    }
    built(
        StageState::Waiting,
        None,
        Source::derived(),
        "no commit of the work is on the base branch",
    )
}

fn pinned(facts: &Facts) -> Built {
    let Some(consumer) = &facts.ship.consumer else {
        return built(
            StageState::NotApplicable,
            None,
            Source::derived(),
            "nothing pins this repository: it ships its own release",
        );
    };
    match &facts.ship.pinned {
        Some(pin) if pin.reaches => built(
            StageState::Done,
            pin.at.clone(),
            Source::derived(),
            format!(
                "{consumer} pins it at {}, which reaches the work",
                short(pin.sha.as_deref())
            ),
        ),
        Some(pin) => built(
            StageState::Waiting,
            pin.at.clone(),
            Source::derived(),
            format!(
                "{consumer} still pins {}, which is behind the work",
                short(pin.sha.as_deref())
            ),
        ),
        None => built(
            StageState::Unknown,
            None,
            Source::derived(),
            format!("the pin {consumer} holds could not be read"),
        ),
    }
}

fn staged(facts: &Facts) -> Built {
    let shipper = facts
        .ship
        .consumer
        .clone()
        .or_else(|| facts.repo.clone())
        .unwrap_or_else(|| "the release".to_string());
    match &facts.ship.staged {
        Some(staged) if staged.reaches => built(
            StageState::Done,
            staged.at.clone(),
            Source::reported(),
            format!(
                "{shipper} {} is staged and carries the work",
                short(staged.sha.as_deref())
            ),
        ),
        Some(staged) => built(
            StageState::Waiting,
            staged.at.clone(),
            Source::reported(),
            format!(
                "the newest staged {shipper} release ({}) does not carry the work",
                short(staged.sha.as_deref())
            ),
        ),
        // A release production runs was staged before it was deployed, so a
        // deployment that carries the work settles staging too.
        None if facts.ship.deployed.as_ref().is_some_and(|d| d.reaches)
            || facts.ship.released == Some(true) =>
        {
            built(
                StageState::Done,
                facts.ship.released_at.clone(),
                Source::derived(),
                format!("the {shipper} release production runs was staged first"),
            )
        }
        None => built(
            StageState::Unknown,
            None,
            Source::derived(),
            format!("no release of {shipper} has been reported staged"),
        ),
    }
}

fn deployed(facts: &Facts) -> Built {
    let shipper = facts
        .ship
        .consumer
        .clone()
        .or_else(|| facts.repo.clone())
        .unwrap_or_else(|| "the release".to_string());
    if let Some(deployed) = &facts.ship.deployed {
        let state = if deployed.reaches {
            StageState::Done
        } else {
            StageState::Waiting
        };
        let summary = if deployed.reaches {
            format!(
                "production runs {shipper} {}, which carries the work",
                short(deployed.sha.as_deref())
            )
        } else {
            format!(
                "production runs {shipper} {}, which is older than the work",
                short(deployed.sha.as_deref())
            )
        };
        return built(state, deployed.at.clone(), Source::reported(), summary);
    }
    match facts.ship.released {
        Some(true) => built(
            StageState::Done,
            facts.ship.released_at.clone(),
            Source::reported(),
            format!("production runs a {shipper} release that carries the work"),
        ),
        Some(false) => built(
            StageState::Waiting,
            facts.ship.released_at.clone(),
            Source::reported(),
            format!("the {shipper} release production runs is older than the work"),
        ),
        None => built(
            StageState::Unknown,
            None,
            Source::derived(),
            format!("no production deployment of {shipper} is known"),
        ),
    }
}

/// A commit as a reader reads it, and a phrase when there is none.
fn short(sha: Option<&str>) -> String {
    match sha {
        Some(sha) if sha.len() >= 7 => sha[..7].to_string(),
        Some(sha) if !sha.is_empty() => sha.to_string(),
        _ => "an unreadable commit".to_string(),
    }
}

/// The twelve stages of one piece of work, in order: pure, so the rules above
/// are what the tests drive.
pub(super) fn stages(facts: &Facts) -> Vec<TraceStage> {
    Stage::ALL
        .into_iter()
        .map(|stage| {
            let rule = match stage {
                Stage::Filed => filed,
                Stage::Claimed => claimed,
                Stage::Done => done,
                Stage::Shift => shift,
                Stage::Pr => pull,
                Stage::Gate => gate,
                Stage::Review => review,
                Stage::Queue => queue,
                Stage::Merged => merged,
                Stage::Pinned => pinned,
                Stage::Staged => staged,
                Stage::Deployed => deployed,
            };
            let href = match stage {
                Stage::Filed | Stage::Claimed | Stage::Done => todo_href(facts),
                Stage::Shift => match &facts.pr {
                    Some(_) => pr_href(facts),
                    None => todo_href(facts),
                },
                Stage::Pr | Stage::Gate | Stage::Review | Stage::Queue | Stage::Merged => {
                    pr_href(facts)
                }
                Stage::Pinned | Stage::Staged | Stage::Deployed => release_href(facts),
            };
            let Built {
                state,
                at,
                source,
                summary,
            } = rule(facts);
            TraceStage {
                stage,
                state,
                at,
                source,
                summary,
                href,
                attention: attention_ids(&facts.attention, stage),
            }
        })
        .collect()
}

pub(super) fn response(facts: &Facts, now: DateTime<Utc>) -> TraceResponse {
    let family = facts.family.as_deref().map(super::super::family::canonical);
    TraceResponse {
        schema_version: TRACE_SCHEMA,
        generated_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        subject: Subject {
            todos: facts.todos.iter().map(|todo| todo.id.clone()).collect(),
            family_label: family.as_deref().map(super::super::family::label),
            family,
            repo: facts.repo.clone(),
            pr: facts.pr.as_ref().map(|pr| pr.number),
            shift: facts.shift.clone(),
            released_by: facts.ship.consumer.clone(),
        },
        stages: stages(facts),
    }
}

// Everything below collects the facts above from the forge: the pure rules
// stay pure, and only this half runs git or reads the core.

fn trace_error(status: StatusCode, code: &str, reason: &str, hint: &str) -> Box<AxumResponse> {
    Box::new(typed_error(TypedError {
        status,
        code,
        purpose: "trace one piece of work from its todo to production",
        reason,
        common_fixes: &[
            "ask for one todo (?todo=<id>) or one pull request (?repo=owner/name&pr=<number>)",
            "list todos with GET /api/v1/shift/todos",
        ],
        docs_url: DOCS,
        repair_hint: hint,
        message: reason,
    }))
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn pr_facts(state: &WebState, owner: &str, name: &str, number: u64) -> Option<PrFacts> {
    let pr = state.core.get_pull_request(owner, name, number).ok()?;
    Some(PrFacts {
        repo: format!("{}/{}", pr.owner, pr.repo),
        number: pr.number,
        head_sha: pr.head.sha.clone(),
        head_ref: pr.head.ref_name.clone(),
        base_ref: pr.base.ref_name.clone(),
        draft: pr.draft,
        merged: pr.merged,
        closed: pr.state == jeryu_core::PullRequestState::Closed,
        opened_at: Some(rfc3339(pr.created_at)),
        merged_at: pr.merged_at.map(rfc3339),
        href: crate::github::pulls::pull_request_web_path(&pr.owner, &pr.repo, pr.number),
    })
}

fn checks(state: &WebState, repo: &str, sha: &str) -> Option<Checks> {
    let (owner, name) = repo.split_once('/')?;
    let combined = state.core.combined_status(owner, name, sha).ok()?;
    if combined.total_count == 0 {
        return None;
    }
    Some(match combined.state {
        jeryu_core::CommitStatusState::Success => Checks::Green,
        jeryu_core::CommitStatusState::Pending => Checks::Pending,
        _ => Checks::Failing,
    })
}

/// The newest verdict a reviewer filed, dismissed reviews aside.
fn review_fact(state: &WebState, repo: &str, number: u64) -> Option<ReviewFact> {
    use jeryu_core::ReviewState;
    let (owner, name) = repo.split_once('/')?;
    let mut reviews = state.core.list_reviews(owner, name, number).ok()?;
    reviews.retain(|review| review.state != ReviewState::Dismissed);
    reviews.sort_by_key(|review| review.submitted_at);
    let last = reviews.last()?;
    let verdict = reviews.iter().rev().find(|review| {
        matches!(
            review.state,
            ReviewState::Approved | ReviewState::ChangesRequested
        )
    });
    Some(ReviewFact {
        approved: verdict.is_some_and(|review| review.state == ReviewState::Approved),
        changes_requested: verdict
            .is_some_and(|review| review.state == ReviewState::ChangesRequested),
        at: Some(rfc3339(verdict.unwrap_or(last).submitted_at)),
    })
}

/// The commit of `dependency` that `consumer_sha` pins, read from the lock
/// file as that exact consumer commit wrote it. This is what makes a release
/// of the consumer answerable for work in the dependency: the lock at the
/// staged or deployed commit, not the one on its branch today.
fn pin_at(
    state: &WebState,
    consumer: &str,
    consumer_sha: &str,
    lock_path: &str,
    dependency: &str,
) -> Option<String> {
    let (owner, name) = consumer.split_once('/')?;
    let opened = state.repo_manager.open_parts(owner, name).ok()?;
    let git_bin = &state.repo_manager.config().git_bin;
    let spec = format!("{consumer_sha}:{lock_path}");
    let blob = super::super::shift::run_git(
        git_bin,
        &opened.path,
        &["cat-file", "blob", &spec],
        &[],
        None,
    )
    .ok()?;
    super::pins::lock_pins(&String::from_utf8_lossy(&blob))
        .into_iter()
        .find(|pin| pin.name == dependency)
        .map(|pin| pin.pinned_ref)
}

/// Whether `commit` reaches any of the work's commits: the pin, the staged
/// release or the deployment is at `commit`, so the work is in it when one of
/// its commits is that commit or an ancestor of it.
fn reaches(state: &WebState, repo: &str, commit: &str, work: &[String]) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    let Ok(opened) = state.repo_manager.open_parts(owner, name) else {
        return false;
    };
    let git_bin = &state.repo_manager.config().git_bin;
    work.iter().any(|sha| {
        sha == commit
            || super::super::shift::run_git(
                git_bin,
                &opened.path,
                &["merge-base", "--is-ancestor", sha, commit],
                &[],
                None,
            )
            .is_ok()
    })
}

/// When the lock that holds a pin last changed on the consumer's branch: the
/// bump that pinned this work, as far as the consumer's history records it.
fn pinned_at(state: &WebState, consumer: &str, branch: &str, lock_path: &str) -> Option<String> {
    let (owner, name) = consumer.split_once('/')?;
    let opened = state.repo_manager.open_parts(owner, name).ok()?;
    let git_bin = &state.repo_manager.config().git_bin;
    let at = super::super::shift::run_git(
        git_bin,
        &opened.path,
        &[
            "log",
            "-1",
            "--format=%cI",
            &format!("refs/heads/{branch}"),
            "--",
            lock_path,
        ],
        &[],
        None,
    )
    .ok()?;
    let at = String::from_utf8_lossy(&at).trim().to_string();
    (!at.is_empty()).then_some(at)
}

/// The production deployment a repository runs: its commit and when it was
/// recorded.
fn production(state: &WebState, repo: &str) -> Option<(String, String)> {
    let (owner, name) = repo.split_once('/')?;
    let current = state
        .core
        .deployment_environments(owner, name)
        .ok()?
        .into_iter()
        .find(|environment| environment.environment == "production")?
        .current?
        .deployment;
    Some((current.sha, rfc3339(current.created_at)))
}

/// How the work reaches production: through a consumer that pins the work's
/// repository, or through that repository's own release.
fn ship_facts(
    state: &WebState,
    repo: &str,
    work: &[String],
    released: Option<bool>,
    events: &[Event],
) -> ShipFacts {
    let staged_for = |shipper: &str| {
        newest(events, &["release.staged"]).filter(|event| event.repo.as_deref() == Some(shipper))
    };
    // The pins of every consumer, the same answer `/api/v1/pins` serves. A
    // repository nothing pins ships its own release.
    let snapshot = super::pins::snapshot(state);
    let found = snapshot.consumers.iter().find_map(|consumer| {
        let pin = consumer
            .pins
            .iter()
            .find(|pin| pin.kind == "commit" && pin.dependency == repo)?;
        Some((consumer, pin))
    });
    let Some((consumer, pin)) = found else {
        let staged = staged_for(repo).and_then(|event| {
            let sha = event.sha.clone()?;
            Some(Shipped {
                reaches: reaches(state, repo, &sha, work),
                sha: Some(sha),
                at: Some(event.ts.clone()),
            })
        });
        return ShipFacts {
            consumer: None,
            pinned: None,
            staged,
            deployed: None,
            released,
            released_at: production(state, repo).map(|(_, at)| at),
            href: Some(format!("/releases?repo={repo}")),
        };
    };
    let dependency = repo.rsplit('/').next().unwrap_or(repo).to_string();
    // What a consumer commit carries: the pin its own lock named there.
    let carried = |consumer_sha: &str| {
        pin_at(
            state,
            &consumer.repo,
            consumer_sha,
            &pin.source,
            &dependency,
        )
        .is_some_and(|pinned| reaches(state, repo, &pinned, work))
    };
    let staged = staged_for(&consumer.repo).and_then(|event| {
        let sha = event.sha.clone()?;
        Some(Shipped {
            reaches: carried(&sha),
            sha: Some(sha),
            at: Some(event.ts.clone()),
        })
    });
    let deployed = production(state, &consumer.repo).map(|(sha, at)| Shipped {
        reaches: carried(&sha),
        sha: Some(sha),
        at: Some(at),
    });
    ShipFacts {
        consumer: Some(consumer.repo.clone()),
        pinned: Some(Shipped {
            reaches: pin
                .pinned_sha
                .as_deref()
                .is_some_and(|pinned| reaches(state, repo, pinned, work)),
            sha: pin
                .pinned_sha
                .clone()
                .or_else(|| Some(pin.pinned_ref.clone())),
            at: pinned_at(state, &consumer.repo, &consumer.branch, &pin.source),
        }),
        staged,
        deployed,
        released: None,
        released_at: None,
        href: Some(format!("/releases?repo={}", consumer.repo)),
    }
}

/// The events of this work, newest first: the ones tagged with one of its
/// todos **and** the ones tagged with its pull request. Filtering by one key
/// or the other is what kept a todo's trace and its pull request's trace
/// apart; the trace is the join.
fn events_of(state: &WebState, todos: &[String], pr: Option<&PrFacts>) -> Vec<Event> {
    let mut events: Vec<Event> = Vec::new();
    let mut queries: Vec<EventsQuery> = todos
        .iter()
        .map(|id| EventsQuery {
            todo_id: Some(id.clone()),
            limit: Some(EVENT_LIMIT),
            ..EventsQuery::default()
        })
        .collect();
    if let Some(pr) = pr {
        queries.push(EventsQuery {
            repo: Some(pr.repo.clone()),
            pr: Some(pr.number as i64),
            limit: Some(EVENT_LIMIT),
            ..EventsQuery::default()
        });
    }
    for query in &queries {
        events.extend(state.events.query(query).unwrap_or_default());
    }
    let mut seen = BTreeSet::new();
    events.retain(|event| seen.insert(event.seq));
    events.sort_by_key(|event| std::cmp::Reverse(event.seq));
    events
}

/// Whether one attention item is about this work: it names one of the todos,
/// or the pull request, or (for an item about shipping) one of the
/// repositories the work travels through, or only the family.
fn about(item: &Item, facts: &Facts) -> bool {
    if let Some(todo) = &item.todo_id {
        return facts.todos.iter().any(|known| &known.id == todo);
    }
    match (&item.repo, item.pr) {
        (Some(repo), Some(pr)) => facts
            .pr
            .as_ref()
            .is_some_and(|known| &known.repo == repo && known.number == pr),
        (Some(repo), None) => {
            facts.repo.as_deref() == Some(repo) || facts.ship.consumer.as_deref() == Some(repo)
        }
        (None, _) => item.family.is_some() && item.family == facts.family,
    }
}

/// The facts of one todo, or of one pull request and every todo it carries.
fn collect(
    state: &WebState,
    query: &TraceQuery,
    now: DateTime<Utc>,
) -> Result<Facts, Box<AxumResponse>> {
    let mut pr = None;
    let mut repo = None;
    let mut family = None;
    let mut shift = None;
    let traces = match (query.todo.as_deref(), query.repo.as_deref(), query.pr) {
        (Some(todo), None, None) => {
            let traces = super::super::shift::todo_traces(state, &[todo.to_string()], now);
            if traces.is_empty() {
                return Err(trace_error(
                    StatusCode::NOT_FOUND,
                    "trace_todo_not_found",
                    &format!("no family queue holds a todo {todo:?}"),
                    "list the todos with GET /api/v1/shift/todos",
                ));
            }
            traces
        }
        (None, Some(full_name), Some(number)) => {
            let Some((owner, name)) = full_name.split_once('/') else {
                return Err(trace_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "trace_invalid_query",
                    &format!("repo: expected owner/name, got {full_name:?}"),
                    "send repo=<owner>/<name>",
                ));
            };
            let Some(facts) = pr_facts(state, owner, name, number) else {
                return Err(trace_error(
                    StatusCode::NOT_FOUND,
                    "trace_pull_not_found",
                    &format!("this forge hosts no {full_name}#{number}"),
                    "check the repository and the pull request number",
                ));
            };
            let range = format!("refs/heads/{}..{}", facts.base_ref, facts.head_sha);
            let ids = super::super::shift::trailer_todos(state, owner, name, &range);
            let (found_family, found_shift) =
                super::super::shift::shift_context(state, owner, name, &facts.head_ref);
            family = found_family;
            shift = found_shift;
            repo = Some(facts.repo.clone());
            pr = Some(facts);
            super::super::shift::todo_traces(state, &ids, now)
        }
        _ => {
            return Err(trace_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "trace_invalid_query",
                "send either todo=<id> or both repo=<owner>/<name> and pr=<number>",
                "GET /api/v1/trace?todo=<id>",
            ));
        }
    };
    let todos: Vec<ShiftTodo> = traces.iter().map(|trace| trace.todo.clone()).collect();
    if let Some(first) = traces.first() {
        family = family.or_else(|| Some(first.todo.family.clone()));
        shift = shift.or_else(|| Some(first.todo.shift.clone()).filter(|s| !s.is_empty()));
    }
    // The repository the work lands in, and the commits of it that count as
    // the work: the shas the todo recorded, and (a shift branch is replayed
    // onto the base, so the sha changes) the base commit carrying its
    // `Todo:` trailer.
    let mut work: Vec<String> = Vec::new();
    for trace in &traces {
        for (name, owner) in &trace.owners {
            let full_name = format!("{owner}/{name}");
            if repo.is_none() {
                repo = Some(full_name.clone());
            }
            if repo.as_deref() != Some(full_name.as_str()) {
                continue;
            }
            if let Some(sha) = trace.todo.commits.get(name) {
                work.push(sha.clone());
            }
            if let Some(landed) = super::super::shift::trailer_commit(
                state,
                owner,
                name,
                &trace.base_branch,
                &trace.todo.id,
            ) {
                work.push(landed);
            }
            if pr.is_none()
                && let Some(todo_pr) = trace.todo.prs.iter().find(|todo_pr| &todo_pr.repo == name)
            {
                pr = pr_facts(state, owner, name, todo_pr.number);
            }
        }
    }
    work.sort();
    work.dedup();
    // Released is unknown unless it is known for every todo: one todo nobody
    // can answer for is not the whole work released.
    let released = (!todos.is_empty())
        .then(|| {
            todos.iter().try_fold(true, |all, todo| {
                todo.released.map(|released| all && released)
            })
        })
        .flatten();
    let events = events_of(
        state,
        &traces.iter().map(|t| t.todo.id.clone()).collect::<Vec<_>>(),
        pr.as_ref(),
    );
    let ship = match &repo {
        Some(repo) => ship_facts(state, repo, &work, released, &events),
        None => ShipFacts::default(),
    };
    let queue = match &pr {
        Some(pr) => state.merge_queue.entries(state, |entry| {
            entry.repo == pr.repo && entry.number == pr.number
        }),
        None => Vec::new(),
    };
    let checks = pr
        .as_ref()
        .and_then(|pr| checks(state, &pr.repo, &pr.head_sha));
    let review = pr
        .as_ref()
        .and_then(|pr| review_fact(state, &pr.repo, pr.number));
    let mut facts = Facts {
        todos,
        family,
        repo,
        shift,
        pr,
        events,
        queue,
        checks,
        review,
        ship,
        attention: Vec::new(),
    };
    let inbox = attention::cached(state, now);
    facts.attention = inbox
        .items
        .into_iter()
        .filter(|item| about(item, &facts))
        .collect();
    Ok(facts)
}

/// `GET /api/v1/trace` (admin-only by path, see `auth::admin_only_request`).
pub(crate) async fn trace(
    State(state): State<Arc<WebState>>,
    Query(query): Query<TraceQuery>,
) -> AxumResponse {
    // The collectors run git, read every open pull request and compute the
    // inbox: keep them off the async workers.
    let worker_state = state.clone();
    let answer = tokio::task::spawn_blocking(move || {
        let now = Utc::now();
        collect(&worker_state, &query, now).map(|facts| response(&facts, now))
    })
    .await;
    match answer {
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(refusal)) => *refusal,
        Err(error) => *trace_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "trace_collect_failed",
            &error.to_string(),
            "retry; if it persists check the server log for a collector panic",
        ),
    }
}

#[cfg(test)]
mod tests;
