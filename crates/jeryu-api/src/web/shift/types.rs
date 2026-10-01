//! Wire types for `/api/v1/shift/*` (see ~/shiftwork-plan.md, "Contract").

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::web::paging::{PageInfo, PageParams};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FamiliesResponse {
    pub families: Vec<FamilySummary>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FamilySummary {
    pub name: String,
    pub queue_repo: String,
    pub repos: Vec<FamilyRepoSummary>,
    pub shift_tz: String,
    pub landing: String,
}

/// A family repo as `GET /api/v1/shift/families` reports it: the entry from
/// `family.toml` plus where the repo is hosted, so a client can link to it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct FamilyRepoSummary {
    pub name: String,
    pub order: i64,
    /// The owner the repo is hosted under, which is not always the queue's
    /// (the jain queue is `jain-split/jain-todo`, its code is `veox/*`).
    /// `null` when this forge does not host the repo at all.
    pub owner: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct FamilyRepo {
    pub name: String,
    #[serde(default)]
    pub order: i64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TodosResponse {
    pub generated_at: String,
    pub todos: Vec<ShiftTodo>,
    pub page: PageInfo,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct ShiftTodo {
    pub id: String,
    pub family: String,
    pub title: String,
    pub body: String,
    pub repos: Vec<String>,
    pub mode: String,
    pub priority: i64,
    pub blocked_by: Vec<String>,
    pub status: TodoStatus,
    pub attempts: i64,
    pub requested_by: String,
    pub filed_at: String,
    pub claim_by: String,
    pub lease_until: String,
    pub lease_live: bool,
    pub shift: String,
    pub change_set: String,
    pub commits: BTreeMap<String, String>,
    /// True when the file says so or, on `GET /api/v1/shift/todos`, when the
    /// server finds every commit on its base branch (see `truth.rs`).
    pub merged: bool,
    /// Whether production runs the merged work; `null` when no production
    /// deployment is known for any of the todo's repos.
    pub released: Option<bool>,
    /// The shift pull request that carries the todo (the first of `prs`).
    pub pr: Option<TodoPr>,
    /// One shift pull request per repo the todo committed to.
    pub prs: Vec<TodoPr>,
    /// Total spend over every attempt, when any attempt recorded a cost.
    pub cost_usd: Option<f64>,
    pub note: String,
    pub triaged: bool,
    pub worked_by: Vec<Attempt>,
}

/// A shift pull request as a todo links to it. `repo` is the family repo name,
/// the same spelling as the keys of `commits`.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct TodoPr {
    pub repo: String,
    pub number: u64,
    pub state: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct Attempt {
    pub by: String,
    pub host: String,
    pub slot: String,
    pub model: String,
    pub session: String,
    pub started: String,
    pub ended: String,
    pub outcome: String,
    pub cost_usd: Option<f64>,
    pub note: String,
    pub shift: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct TodosQuery {
    pub family: Option<String>,
    pub status: Option<String>,
    pub mode: Option<String>,
    pub repo: Option<String>,
    pub requested_by: Option<String>,
    pub worked_by: Option<String>,
    pub shift: Option<String>,
    #[serde(flatten)]
    pub paging: PageParams,
}

/// `POST /api/v1/shift/todos`: one todo (`text`) or many (`texts`).
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct FileTodoRequest {
    pub family: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub texts: Option<Vec<String>>,
    pub mode: String,
    #[serde(default)]
    pub repos: Option<Vec<String>>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub blocked_by: Option<Vec<String>>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FiledTodos {
    pub todos: Vec<ShiftTodo>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct TodoActionRequest {
    pub action: String,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub note: Option<String>,
}

/// A slot's report. `slot` is a string (todoq names slots `w1`, `w2`, ...);
/// a number is accepted and stored as its decimal spelling.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct HeartbeatRequest {
    pub operator: String,
    pub host: String,
    pub slot: Value,
    pub family: String,
    pub state: String,
    #[serde(default)]
    pub todo_id: Option<String>,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub lease_until: Option<String>,
    #[serde(default)]
    pub shift: Option<String>,
    #[serde(default)]
    pub planned_slots: Option<i64>,
    #[serde(default)]
    pub schedule: Option<Value>,
    #[serde(default)]
    pub version: Option<String>,
}

/// A stored heartbeat, as returned by `GET /api/v1/shift/workers`.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct Heartbeat {
    pub operator: String,
    pub host: String,
    pub slot: String,
    pub family: String,
    pub state: String,
    pub todo_id: Option<String>,
    pub stage: Option<String>,
    pub lease_until: Option<String>,
    pub shift: Option<String>,
    pub planned_slots: Option<i64>,
    pub schedule: Option<Value>,
    pub version: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WorkerRow {
    #[serde(flatten)]
    pub heartbeat: Heartbeat,
    pub last_seen: String,
    pub healthy: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WorkersResponse {
    pub generated_at: String,
    pub workers: Vec<WorkerRow>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct HistoryQuery {
    pub hours: Option<i64>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct HistoryResponse {
    pub from: String,
    pub to: String,
    pub slots: Vec<SlotHistory>,
    pub capacity: Vec<CapacityBucket>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct SlotHistory {
    pub operator: String,
    pub host: String,
    pub slot: String,
    pub family: String,
    pub segments: Vec<Segment>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct Segment {
    pub from: String,
    pub to: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub todo_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct CapacityBucket {
    pub at: String,
    pub planned: i64,
    pub busy: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_depth: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct ShiftsQuery {
    pub family: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ShiftsResponse {
    pub shifts: Vec<ShiftBranch>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ShiftBranch {
    pub branch: String,
    pub kind: String,
    pub date: String,
    pub repos: Vec<ShiftRepo>,
    pub todo_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ShiftRepo {
    pub repo: String,
    pub head: String,
    pub ahead: i64,
    pub behind: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<ShiftPr>,
    /// Todos whose commits are on this branch but not on the base, neither as
    /// themselves nor replayed (no base commit carries their `Todo:` trailer).
    /// After the shift's pull request merged, these are stranded: work that
    /// landed on the branch too late to ride that pull request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmerged_todos: Vec<String>,
    /// When this branch's own pull request is closed: an open pull request in
    /// the same repo whose commits carry some of the unmerged todos. The
    /// operator closes a shift PR and opens a replacement from another branch,
    /// cherry-picked onto the base; the work is under review there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_pr: Option<ShiftPr>,
    /// The unmerged todos that [`ShiftRepo::review_pr`] carries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewed_todos: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ShiftPr {
    pub number: u64,
    pub state: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ShiftPrRequest {
    pub branch: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ShiftPrResponse {
    pub prs: Vec<CreatedPr>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CreatedPr {
    pub repo: String,
    pub number: u64,
    pub url: String,
    pub created: bool,
}

/// Where a todo is in its lifecycle, spelled as todoq writes it.
///
/// `open -> claimed -> done | blocked | handoff`; an admin `release` returns
/// unfinished work to `open` and `block` parks it. `done` is final: the server
/// never moves a todo out of it (see [`TodoStatus::allows`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TodoStatus {
    #[default]
    Open,
    Claimed,
    Done,
    Blocked,
    Handoff,
}

impl TodoStatus {
    pub(crate) const ALL: [Self; 5] = [
        Self::Open,
        Self::Claimed,
        Self::Done,
        Self::Blocked,
        Self::Handoff,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Claimed => "claimed",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Handoff => "handoff",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.as_str() == text)
    }

    /// Whether the server may move a todo from `self` to `next`. Staying put
    /// is allowed (a repeated block updates the note); nothing leaves `done`,
    /// because landed work reopened would be worked and landed twice.
    pub(crate) fn allows(self, next: Self) -> bool {
        match (self, next) {
            (from, to) if from == to => true,
            (Self::Done, _) => false,
            (Self::Open | Self::Claimed | Self::Blocked | Self::Handoff, _) => true,
        }
    }
}

/// What a worker slot reports it is doing in a heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerState {
    Idle,
    Working,
    Stopping,
    Paused,
}

impl WorkerState {
    pub(crate) const ALL: [Self; 4] = [Self::Idle, Self::Working, Self::Stopping, Self::Paused];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Stopping => "stopping",
            Self::Paused => "paused",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == text)
    }
}
