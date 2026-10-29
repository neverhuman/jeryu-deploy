//! Wire types for `/api/v1/shift/*` (see ~/shiftwork-plan.md, "Contract").

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::web::paging::{PageInfo, PageParams};
use crate::web::strict_query::{StrictFields, filter_one_of};
use crate::web::usage::Usage;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FamiliesResponse {
    pub families: Vec<FamilySummary>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FamilySummary {
    /// Canonical family key: the same string every family-taking endpoint
    /// returns and accepts (see `crate::web::family`).
    pub name: String,
    /// What a reader is shown for `name`.
    pub label: String,
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
    /// The clone URL todoq works from; its last two path segments name the
    /// hosted repository unless `jeryu_repo` says otherwise.
    #[serde(default, skip_serializing)]
    pub url: String,
    /// `owner/name` on this forge when the URL does not spell it (todoq's
    /// `RepoSpec.jeryu_repo`).
    #[serde(default, skip_serializing)]
    pub jeryu_repo: String,
}

impl FamilyRepo {
    /// The owner `family.toml` says this repo is hosted under: `jeryu_repo`,
    /// else the clone URL, the same rule todoq uses. Only a spelling whose
    /// repository name is this entry's name counts, since every reader opens
    /// `<owner>/<name>`.
    pub(crate) fn declared_owner(&self) -> Option<&str> {
        let spelled = if self.jeryu_repo.is_empty() {
            let path = self.url.trim_end_matches('/');
            let path = path.strip_suffix(".git").unwrap_or(path);
            let (rest, name) = path.rsplit_once('/')?;
            let owner = rest.rsplit_once('/').map_or(rest, |(_, owner)| owner);
            (owner, name)
        } else {
            self.jeryu_repo.split_once('/')?
        };
        match spelled {
            (owner, name) if !owner.is_empty() && name == self.name => Some(owner),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TodosResponse {
    pub generated_at: String,
    pub todos: Vec<ShiftTodo>,
    /// Todos matching the filter before paging, the top-level `total` every
    /// paged `/api/v1` listing answers with (`docs/pagination.md`).
    pub total: usize,
    pub page: PageInfo,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct ShiftTodo {
    #[serde(flatten)]
    pub hypothesis: super::hypothesis::Hypothesis,
    pub outcome_check_due_at: Option<String>,
    pub id: String,
    /// Canonical family key (see `crate::web::family`).
    pub family: String,
    /// What a reader is shown for `family`.
    pub family_label: String,
    pub title: String,
    pub reworks: Vec<String>,
    pub body: String,
    pub repos: Vec<String>,
    pub mode: String,
    pub priority: i64,
    pub blocked_by: Vec<String>,
    /// One line per `blocked_by` entry whose work is not on the base branch
    /// yet, naming the id and what it waits for, as `/work` shows it beside a
    /// todo no worker may claim (see `chain.rs`). Empty on any todo a worker
    /// would not pick up anyway.
    pub blocked_on: Vec<String>,
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
    /// Per-repository landing facts for this todo, including saved work that
    /// did not reach the base branch.
    pub landing: Vec<TodoLanding>,
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
    /// What triaging the todo cost, when todoq recorded a non-zero amount.
    pub triage_cost_usd: Option<f64>,
    /// When the attempt in progress began and which model runs it, as the
    /// claiming worker wrote them; `null` when none is in progress (an attempt
    /// reaches `worked_by` only once it ends). Older workers write neither.
    pub claimed_at: Option<String>,
    pub claim_model: Option<String>,
    pub note: String,
    /// When a parked todo comes back by itself, as the file spells it; empty
    /// when it is parked with no date or not parked at all.
    pub park_until: String,
    /// The person who has taken the todo on; `null` when nobody has.
    pub assignee: Option<String>,
    /// What kind of help a `blocked` or `handoff` todo needs, read off its
    /// note and its newest attempt's outcome (see [`BlockKind::derive`]).
    /// `null` on every other status.
    pub block_kind: Option<BlockKind>,
    pub triaged: bool,
    pub worked_by: Vec<Attempt>,
    /// The body's `## Acceptance criteria` checklist (see [`Acceptance`]).
    pub acceptance: Acceptance,
}

/// A todo's acceptance criteria, read off its body. `state` is `set` when the
/// body has the checklist, `none` when the filer recorded `criteria = "none"`,
/// and `unknown` for a todo filed before the block existed.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub(crate) struct Acceptance {
    pub state: String,
    pub total: i64,
    pub met: i64,
    pub items: Vec<Criterion>,
}

/// One criterion: its box, and the evidence line the worker recorded under it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct Criterion {
    pub text: String,
    pub checked: bool,
    pub evidence: Option<String>,
}

/// Why a todo stopped, which decides what the attention inbox asks of a
/// person: a worker cannot finish an owner's task, cannot pay for a todo past
/// its budget, and cannot add a repo to the family config.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BlockKind {
    /// Only the owner can do it ("OWNER: ...", "needs the owner first").
    OwnerTask,
    /// The attempts spent the todo's budget, so another attempt stops again.
    OverBudget,
    /// It names a repo the family config does not list.
    UnknownRepo,
    /// A worker did what it could and handed the rest to a person.
    Handoff,
    /// Anything else a worker could not get past.
    AgentBlocked,
}

impl BlockKind {
    /// The kinds, for the test that checks each one's wire spelling. Product
    /// code reads a kind, never the list.
    #[cfg(test)]
    pub(crate) const ALL: [Self; 5] = [
        Self::OwnerTask,
        Self::OverBudget,
        Self::UnknownRepo,
        Self::Handoff,
        Self::AgentBlocked,
    ];

    /// The wire spelling, which the serde rename above must agree with.
    #[cfg(test)]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::OwnerTask => "owner_task",
            Self::OverBudget => "over_budget",
            Self::UnknownRepo => "unknown_repo",
            Self::Handoff => "handoff",
            Self::AgentBlocked => "agent_blocked",
        }
    }

    /// The kind a stopped todo's own words name. Both the note an admin or a
    /// worker left and the newest attempt's outcome are read, because a worker
    /// that ran out of budget records it as the outcome and may leave no note.
    /// The order is the order of how little a worker can do about it.
    pub(crate) fn derive(
        status: TodoStatus,
        title: &str,
        note: &str,
        outcome: &str,
    ) -> Option<Self> {
        if !matches!(status, TodoStatus::Blocked | TodoStatus::Handoff) {
            return None;
        }
        let said = format!("{title}\n{note}\n{outcome}").to_lowercase();
        let says = |needles: &[&str]| needles.iter().any(|needle| said.contains(needle));
        Some(
            if says(&["owner:", "owner_task", "needs the owner", "only the owner"]) {
                Self::OwnerTask
            } else if says(&[
                "over_budget",
                "over budget",
                "budget cap",
                "cost cap",
                "out of budget",
            ]) {
                Self::OverBudget
            } else if says(&[
                "unknown_repo",
                "unknown repo",
                "is not in family",
                "not in the family config",
            ]) {
                Self::UnknownRepo
            } else if status == TodoStatus::Handoff
                || says(&["handoff", "finish by hand", "by hand"])
            {
                Self::Handoff
            } else {
                Self::AgentBlocked
            },
        )
    }
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

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TodoLandingState {
    Landed,
    GateFailed,
    Conflict,
    PushRace,
    Kept,
    Pending,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TodoLanding {
    pub repo: String,
    pub state: TodoLandingState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rescue_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rescue_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rescue_ahead: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
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
    /// The harness CLI that ran the attempt (`claude`, `codex`); absent on
    /// attempts recorded before todoq wrote it, which ran the operator default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// `agent:model:p<prompt sha8>`, the full recipe the attempt ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub factory: Option<String>,
    /// The worker's run directory name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// The routing experiment that chose the attempt's arm; absent when the
    /// attempt was not routed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experiment: Option<String>,
    /// The probability the arm had of being drawn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arm_p: Option<f64>,
    /// True when the cost was priced from a transcript because the run was
    /// stopped before it reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_estimated: Option<bool>,
    /// Token counts and cost by model, as the file has them:
    /// `{ "<model>": { input, output, cache_read, cache_write, thinking, cost_usd, ... } }`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct TodosQuery {
    /// Either spelling of a family key; the server canonicalises it.
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

impl StrictFields for TodosQuery {
    const KEYS: &'static [&'static str] = &[
        "family",
        "status",
        "mode",
        "repo",
        "requested_by",
        "worked_by",
        "shift",
        "limit",
        "per_page",
        "page",
    ];

    fn check_values(&self) -> Result<(), String> {
        filter_one_of("status", self.status.as_ref(), &TodoStatus::names())?;
        filter_one_of("mode", self.mode.as_ref(), super::todo_file::MODES)
    }
}

/// `GET /api/v1/shift/workers`: every slot, or one family's slots.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct WorkersQuery {
    /// Either spelling of a family key; the server canonicalises it.
    pub family: Option<String>,
}

impl StrictFields for WorkersQuery {
    const KEYS: &'static [&'static str] = &["family"];
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
    /// `park`: when the todo comes back by itself, RFC 3339. Left out, the
    /// todo stays parked until somebody acts on it.
    #[serde(default)]
    pub until: Option<String>,
    /// `release`: take the todo back from a worker whose claim lease is still
    /// live. Without it such a release answers `409 claim_live`, because the
    /// released todo would be claimed by a second worker while the first is
    /// still running it.
    #[serde(default)]
    pub force: bool,
    /// `edit`: the fields to overwrite. A field left out is left alone.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub repos: Option<Vec<String>>,
    #[serde(default)]
    pub reworks: Option<Vec<String>>,
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
    /// What the slot is, spelled as [`WorkerKind`]: left out by a queue slot,
    /// `"pool"` by one member of an external fleet.
    #[serde(default)]
    pub kind: Option<String>,
    /// A few words behind `state`, e.g. why the slot is paused.
    #[serde(default)]
    pub detail: Option<String>,
    /// How many jobs this slot can hold at once, and holds now. Only a pool
    /// slot reports them; a queue slot carries one todo and leaves them out.
    #[serde(default)]
    pub capacity: Option<i64>,
    #[serde(default)]
    pub active: Option<i64>,
    /// Running token totals for the slot's current attempt, reset when
    /// `todo_id` changes (see `crate::web::usage`). A producer that reports
    /// none leaves it out, and the slot's usage reads `null`.
    #[serde(default)]
    pub usage: Option<Usage>,
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
    /// `"todoq"` unless the reporter said otherwise (see [`WorkerKind`]).
    pub kind: String,
    pub detail: Option<String>,
    pub capacity: Option<i64>,
    pub active: Option<i64>,
    /// What the slot's current attempt has spent so far; `null` when the
    /// producer reports no usage. Filled in from the usage samples the slot
    /// reported, not from the heartbeat row (see `crate::web::usage`).
    pub usage: Option<Usage>,
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
    /// Canonical key of the family whose repos carry this branch.
    pub family: String,
    /// What a reader is shown for `family`.
    pub family_label: String,
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
/// unfinished work to `open`, `block` holds it for a person, `park` sets it
/// aside (with a date it comes back by itself, see `park_until`) and `close`
/// says it will not be done. `done` is final: the server never moves a todo
/// out of it (see [`TodoStatus::allows`]). A closed todo is not: an admin
/// releases it to pick the work up again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TodoStatus {
    #[default]
    Open,
    Claimed,
    Done,
    Blocked,
    Handoff,
    Parked,
    Closed,
}

impl TodoStatus {
    pub(crate) const ALL: [Self; 7] = [
        Self::Open,
        Self::Claimed,
        Self::Done,
        Self::Blocked,
        Self::Handoff,
        Self::Parked,
        Self::Closed,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Claimed => "claimed",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Handoff => "handoff",
            Self::Parked => "parked",
            Self::Closed => "closed",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.as_str() == text)
    }

    /// Every status as it is spelled on the wire: the closed set `?status=`
    /// accepts.
    pub(crate) fn names() -> Vec<&'static str> {
        Self::ALL.iter().map(|status| status.as_str()).collect()
    }

    /// Whether the server may move a todo from `self` to `next`. Staying put
    /// is allowed (a repeated block updates the note); nothing leaves `done`,
    /// because landed work reopened would be worked and landed twice.
    pub(crate) fn allows(self, next: Self) -> bool {
        match (self, next) {
            (from, to) if from == to => true,
            (Self::Done, _) => false,
            (
                Self::Open
                | Self::Claimed
                | Self::Blocked
                | Self::Handoff
                | Self::Parked
                | Self::Closed,
                _,
            ) => true,
        }
    }
}

/// What kind of slot a heartbeat is about. A queue slot runs one todo at a
/// time and is what a reporter that says nothing means; a pool slot is one
/// member of a fleet the forge does not run (an account, a container, a
/// browser profile), which holds several jobs at once and reports how many.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WorkerKind {
    #[default]
    Todoq,
    Pool,
}

impl WorkerKind {
    pub(crate) const ALL: [Self; 2] = [Self::Todoq, Self::Pool];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Todoq => "todoq",
            Self::Pool => "pool",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
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
