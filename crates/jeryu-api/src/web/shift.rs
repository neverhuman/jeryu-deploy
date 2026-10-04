//! Shift pages API: todoq family queues, slot heartbeats, and shift branches.
//!
//! Routes live under `/api/v1/shift/`. Reads need any logged-in account;
//! every POST is admin-only (see `auth::admin_only_request`). Queues are read
//! from and written to the hosted `<family>-todo` bare repos directly; see
//! `queue.rs`. Heartbeat history is kept in `<data_dir>/shift.sqlite`.

mod heartbeats;
pub(crate) mod queue;
mod shifts;
mod todo_file;
mod truth;
mod types;
mod visibility;

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
pub(crate) mod tests;

use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{DateTime, Utc};
use jeryu_core::AccountSummary;
use serde_json::{Value, json};

use super::WebState;
use super::pipeline::{self, NewEvent};
use super::workcells_support::{TypedError, typed_error};
use heartbeats::{HEALTHY_MS, HeartbeatStore};
pub(crate) use heartbeats::{migrate as migrate_shift_store, rfc3339_ms};
use queue::{Queue, WriteError, commit_change, discover};
pub(crate) use queue::{git as run_git, resolve as resolve_commit};
use todo_file::{MODES, TodoFile, iso, new_id};
use types::*;
pub(crate) use types::{BlockKind, ShiftBranch, ShiftRepo, ShiftTodo, TodoStatus, WorkerRow};
#[cfg(test)]
pub(crate) use types::{Heartbeat, ShiftPr};
use visibility::stage_event;
pub(crate) use visibility::{FamilySnapshot, attention_snapshot, shift_context, worker_rows};

const PR_AUTHOR_ENV: &str = "JERYU_SHIFT_PR_AUTHOR";
const DEFAULT_PR_AUTHOR: &str = "alton2";
const STAGES: &[&str] = &["prepare", "agent", "gate", "land", "record"];
const DOCS: &str = "docs/architecture.md";

/// Opt every hosted family queue out of automatic default-branch protection.
///
/// A queue repo (`<family>-todo` with a `queue` branch carrying `family.toml`)
/// is written by todoq itself: claims are pushed straight to `queue`, so that
/// branch must never be made pull-request-only. Called once at startup, after
/// the forge core has backfilled protection rules, with an admin `actor`;
/// opting out also removes the rule the backfill just created, as long as it
/// is still exactly the automatic one. Returns how many repos it changed, and
/// skips a queue whose repo is unregistered, already exempt, or whose opt-out
/// the core refuses (a repository that requires a status context keeps its
/// protection).
pub(crate) fn exempt_queues_from_default_branch_protection(state: &WebState, actor: &str) -> usize {
    let mut exempted = 0;
    for queue in discover(&state.repo_manager) {
        let Ok(repo) = state.core.get_repository(&queue.owner, &queue.repo) else {
            continue;
        };
        if repo.default_branch_protection_opt_out {
            continue;
        }
        if state
            .core
            .set_default_branch_protection_opt_out(actor, &queue.owner, &queue.repo, true)
            .is_ok()
        {
            exempted += 1;
        }
    }
    exempted
}

/// Shift state carried on `WebState`.
#[derive(Clone)]
pub(crate) struct ShiftState {
    pub(crate) heartbeats: HeartbeatStore,
    pub(crate) pr_author: String,
    /// Derived merged/released state of done todos (see `truth.rs`).
    pub(crate) truth: truth::TruthCache,
}

impl ShiftState {
    pub(crate) fn open(path: &Path) -> Self {
        let pr_author = std::env::var(PR_AUTHOR_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_PR_AUTHOR.to_string());
        Self {
            heartbeats: HeartbeatStore::open(path).expect("open shift heartbeat store"),
            pr_author,
            truth: truth::TruthCache::default(),
        }
    }
}

fn shift_error(status: StatusCode, code: &str, reason: &str, hint: &str) -> AxumResponse {
    typed_error(TypedError {
        status,
        code,
        purpose: "operate the todoq shift queue",
        reason,
        common_fixes: &[
            "list families with GET /api/v1/shift/families",
            "check the request body against the shift API contract",
        ],
        docs_url: DOCS,
        repair_hint: hint,
        message: reason,
    })
}

fn bad_request(reason: &str) -> AxumResponse {
    shift_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "shift_invalid_request",
        reason,
        "fix the field named in the message and retry",
    )
}

fn parse<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, Box<AxumResponse>> {
    serde_json::from_slice(body).map_err(|err| Box::new(bad_request(&err.to_string())))
}

/// The queue of `family`, whichever spelling the caller used. A family the
/// forge knows nothing about is a typed 400; a family it knows but hosts no
/// queue for is a 404.
fn find_queue(state: &WebState, family: &str) -> Result<Queue, Box<AxumResponse>> {
    let key = super::family::canonical(family);
    discover(&state.repo_manager)
        .into_iter()
        .find(|q| q.family.name == key)
        .ok_or_else(|| {
            if !super::family::known(state).contains(&key) {
                return Box::new(super::family::unknown(state, family));
            }
            Box::new(shift_error(
                StatusCode::NOT_FOUND,
                "shift_family_not_found",
                &format!("no queue for family {key:?}"),
                "host a <family>-todo repo with a queue branch and family.toml",
            ))
        })
}

fn git_failure(reason: &str) -> AxumResponse {
    shift_error(
        StatusCode::CONFLICT,
        "shift_queue_write_failed",
        reason,
        "retry; the queue moved or git refused the update",
    )
}

/// `GET /api/v1/shift/families`
pub(crate) async fn families(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> Json<FamiliesResponse> {
    // Any logged-in account reads the families, but where a repo is hosted is
    // told only to a reader who may read that repo.
    let may_read = |owner: &str, repo: &str| {
        account.role == jeryu_core::UserRole::Admin
            || state.core.user_can_read_repo(&account.login, owner, repo)
    };
    let families = discover(&state.repo_manager)
        .into_iter()
        .map(|q| FamilySummary {
            queue_repo: q.full_name(),
            repos: q
                .family
                .repos
                .iter()
                .map(|repo| FamilyRepoSummary {
                    name: repo.name.clone(),
                    order: repo.order,
                    owner: truth::hosted_owner(&state, &q, &repo.name)
                        .filter(|owner| may_read(owner, &repo.name)),
                })
                .collect(),
            label: crate::web::family::label(&q.family.name),
            name: q.family.name,
            shift_tz: q.family.shift_tz,
            landing: q.family.landing,
        })
        .collect();
    Json(FamiliesResponse { families })
}

fn matches(todo: &ShiftTodo, query: &TodosQuery) -> bool {
    let eq = |want: &Option<String>, have: &str| {
        want.as_deref()
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .is_none_or(|w| w == have)
    };
    let repo_ok = query
        .repo
        .as_deref()
        .filter(|r| !r.is_empty())
        .is_none_or(|r| todo.repos.iter().any(|x| x == r));
    let worker_ok = query
        .worked_by
        .as_deref()
        .filter(|w| !w.is_empty())
        .is_none_or(|w| {
            todo.claim_by == w
                || todo.claim_by.starts_with(&format!("{w}/"))
                || todo
                    .worked_by
                    .iter()
                    .any(|a| a.by == w || a.by.starts_with(&format!("{w}/")))
        });
    eq(&query.status, todo.status.as_str())
        && eq(&query.mode, &todo.mode)
        && eq(&query.requested_by, &todo.requested_by)
        && eq(&query.shift, &todo.shift)
        && repo_ok
        && worker_ok
}

pub(crate) fn queue_todos(
    state: &WebState,
    queue: &Queue,
) -> Result<Vec<queue::QueuedTodo>, String> {
    queue::read_todos(
        &state.repo_manager.config().git_bin,
        &queue.path,
        &queue.head,
    )
}

/// Every todo of every hosted family queue, unfiltered and unenriched: what
/// product search (`search.rs`) reads. Returns the todos it could read and,
/// per queue it could not, one line naming the family and the reason, so a
/// broken queue narrows the answer instead of failing the whole search.
pub(crate) fn all_todos(state: &WebState) -> (Vec<ShiftTodo>, Vec<String>) {
    let now = Utc::now();
    let mut todos = Vec::new();
    let mut problems = Vec::new();
    for queue in discover(&state.repo_manager) {
        match queue_todos(state, &queue) {
            Ok(queued) => todos.extend(queued.iter().map(|q| q.todo.to_api(now))),
            Err(err) => problems.push(format!("todo queue {}: {err}", queue.family.name)),
        }
    }
    (todos, problems)
}

/// `GET /api/v1/shift/todos`
pub(crate) async fn list_todos(
    State(state): State<Arc<WebState>>,
    Query(query): Query<TodosQuery>,
) -> AxumResponse {
    let page = match query.paging.page() {
        Ok(page) => page,
        Err(rejection) => return rejection.into_response(),
    };
    // A family nobody hosts is a mistake in the request, not an empty queue.
    if let Some(family) = query.family.as_deref().filter(|f| !f.is_empty())
        && let Err(resp) = find_queue(&state, family)
    {
        return *resp;
    }
    let now = Utc::now();
    let mut todos = Vec::new();
    for queue in discover(&state.repo_manager) {
        if let Some(family) = query.family.as_deref().filter(|f| !f.is_empty())
            && super::family::canonical(family) != queue.family.name
        {
            continue;
        }
        let queued = match queue_todos(&state, &queue) {
            Ok(queued) => queued,
            Err(err) => {
                return shift_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "shift_queue_read_failed",
                    &err,
                    "check the queue repo on disk",
                );
            }
        };
        let mut family_todos: Vec<ShiftTodo> = queued
            .iter()
            .map(|q| q.todo.to_api(now))
            .filter(|t| matches(t, &query))
            .collect();
        state.shift.truth.enrich(&state, &queue, &mut family_todos);
        todos.extend(family_todos);
    }
    todos.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
    let (todos, page) = page.apply(todos);
    Json(TodosResponse {
        generated_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        todos,
        page,
    })
    .into_response()
}

fn validate_list(ids: &[String], what: &str) -> Result<(), String> {
    if ids.iter().any(|id| id.trim().is_empty()) {
        return Err(format!("{what} entries must not be empty"));
    }
    Ok(())
}

/// Build the todos a file request describes (not yet written).
fn todos_from_request(
    request: &FileTodoRequest,
    queue: &Queue,
    login: &str,
) -> Result<Vec<TodoFile>, String> {
    if !MODES.contains(&request.mode.as_str()) {
        return Err(format!("mode must be one of {MODES:?}"));
    }
    let texts: Vec<String> = match (&request.text, &request.texts) {
        (Some(text), None) => vec![text.clone()],
        (None, Some(texts)) => texts.clone(),
        _ => return Err("send exactly one of text or texts".to_string()),
    };
    let texts: Vec<String> = texts
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if texts.is_empty() {
        return Err("text must not be empty".to_string());
    }
    let priority = request.priority.unwrap_or(3);
    if !(1..=4).contains(&priority) {
        return Err("priority must be 1..4".to_string());
    }
    let repos = request.repos.clone().unwrap_or_default();
    for repo in &repos {
        if !queue.family.repos.iter().any(|r| &r.name == repo) {
            return Err(format!(
                "repo {repo:?} is not in family {:?}",
                queue.family.name
            ));
        }
    }
    let blocked_by = request.blocked_by.clone().unwrap_or_default();
    validate_list(&blocked_by, "blocked_by")?;
    let title = request
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    if title.is_some() && texts.len() > 1 {
        return Err("title applies to a single todo only".to_string());
    }
    let triaged = title.is_some() && !repos.is_empty();
    let now = Utc::now();
    let mut ids = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for text in texts {
        let title = title
            .map(str::to_string)
            .unwrap_or_else(|| default_title(&text));
        let mut id = new_id(now);
        while !ids.insert(id.clone()) {
            id = new_id(now);
        }
        let mut todo = TodoFile::new(id, queue.family.name.clone(), title);
        todo.body = text;
        todo.repos = repos.clone();
        todo.mode = request.mode.clone();
        todo.priority = priority;
        todo.blocked_by = blocked_by.clone();
        todo.requested_by = login.to_string();
        todo.filed_at = iso(now);
        todo.triaged = triaged;
        out.push(todo);
    }
    Ok(out)
}

/// First line of the ask, trimmed to a readable title.
fn default_title(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let mut title: String = line.chars().take(100).collect();
    if line.chars().count() > 100 {
        title = title.trim_end().to_string();
        title.push('…');
    }
    if title.is_empty() {
        "todo".to_string()
    } else {
        title
    }
}

/// `POST /api/v1/shift/todos`
pub(crate) async fn file_todos(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    body: Bytes,
) -> AxumResponse {
    let request: FileTodoRequest = match parse(&body) {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let queue = match find_queue(&state, &request.family) {
        Ok(q) => q,
        Err(resp) => return *resp,
    };
    let todos = match todos_from_request(&request, &queue, &account.login) {
        Ok(t) => t,
        Err(reason) => return bad_request(&reason),
    };
    let message = format!("file {} todo(s) by {}/web", todos.len(), account.login);
    let result = commit_change(
        &state.repo_manager,
        &queue,
        &account.login,
        &message,
        |_existing| {
            let changes = todos
                .iter()
                .map(|t| (format!("todos/{}", t.filename()), Some(t.dump())))
                .collect();
            Ok::<_, String>((changes, ()))
        },
    );
    if let Err(WriteError::Git(reason) | WriteError::Rejected(reason)) = result {
        return git_failure(&reason);
    }
    let now = Utc::now();
    let api: Vec<ShiftTodo> = todos.iter().map(|t| t.to_api(now)).collect();
    for todo in &api {
        pipeline::emit(
            &state,
            NewEvent {
                actor: Some(format!("{}/web", account.login)),
                family: Some(todo.family.clone()),
                todo_id: Some(todo.id.clone()),
                detail: Some(json!({
                    "mode": todo.mode,
                    "priority": todo.priority,
                    "repos": todo.repos,
                    "triaged": todo.triaged,
                })),
                ..NewEvent::forge("todo.filed", format!("filed: {}", todo.title))
            },
        );
    }
    if request.texts.is_some() {
        (StatusCode::CREATED, Json(FiledTodos { todos: api })).into_response()
    } else {
        (StatusCode::CREATED, Json(api.into_iter().next())).into_response()
    }
}

/// Apply an admin action to a todo in place. `family_repos` is the family
/// config's repo list, which `edit` validates new repos against: a todo may
/// only name a repo its family hosts, or no worker could ever clone it.
pub(crate) fn apply_action(
    todo: &mut TodoFile,
    request: &TodoActionRequest,
    family_repos: &[String],
) -> Result<(), String> {
    let note = request
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let target = match request.action.as_str() {
        "release" => Some(TodoStatus::Open),
        "block" => Some(TodoStatus::Blocked),
        "done" => Some(TodoStatus::Done),
        "close" => Some(TodoStatus::Closed),
        "park" => Some(TodoStatus::Parked),
        _ => None,
    };
    if let Some(target) = target
        && !todo.status.allows(target)
    {
        return Err(format!(
            "cannot {} a {} todo",
            request.action,
            todo.status.as_str()
        ));
    }
    match request.action.as_str() {
        "release" => {
            todo.status = TodoStatus::Open;
            todo.lease_until.clear();
            todo.park_until.clear();
            todo.attempts = 0;
            if let Some(note) = note {
                todo.note = note.to_string();
            }
        }
        "block" => {
            todo.status = TodoStatus::Blocked;
            todo.lease_until.clear();
            let reason = note.or_else(|| request.value.as_ref().and_then(Value::as_str));
            if let Some(reason) = reason {
                todo.note = reason.to_string();
            }
        }
        "done" => {
            todo.status = TodoStatus::Done;
            todo.lease_until.clear();
            todo.park_until.clear();
            if let Some(note) = note {
                todo.note = note.to_string();
            }
        }
        "close" => {
            todo.status = TodoStatus::Closed;
            todo.lease_until.clear();
            todo.park_until.clear();
            if let Some(note) = note {
                todo.note = note.to_string();
            }
        }
        "park" => {
            // A park with no date waits for a person; one with a date comes
            // back by itself, so the date must be a date the server can read.
            let until = request
                .until
                .as_deref()
                .map(str::trim)
                .filter(|until| !until.is_empty());
            let until = match until {
                Some(until) => Some(
                    DateTime::parse_from_rfc3339(until)
                        .map_err(|err| format!("until must be an RFC 3339 time: {err}"))?,
                ),
                None => None,
            };
            todo.status = TodoStatus::Parked;
            todo.lease_until.clear();
            todo.park_until = until
                .map(|until| iso(until.with_timezone(&Utc)))
                .unwrap_or_default();
            if let Some(note) = note {
                todo.note = note.to_string();
            }
        }
        "edit" => {
            if request.title.is_none() && request.body.is_none() && request.repos.is_none() {
                return Err("edit needs one of title, body or repos".to_string());
            }
            if let Some(title) = &request.title {
                let title = title.trim();
                if title.is_empty() {
                    return Err("title must not be empty".to_string());
                }
                todo.title = title.to_string();
            }
            if let Some(body) = &request.body {
                todo.body = body.trim_matches('\n').to_string();
            }
            if let Some(repos) = &request.repos {
                validate_list(repos, "repos")?;
                for repo in repos {
                    if !family_repos.iter().any(|known| known == repo) {
                        return Err(format!("repo {repo:?} is not in family {:?}", todo.family));
                    }
                }
                todo.repos = repos.clone();
            }
            // A todo filed without a title and repos is untriaged; giving it
            // both is exactly what triage is.
            if !todo.title.is_empty() && !todo.repos.is_empty() {
                todo.triaged = true;
            }
            if let Some(note) = note {
                todo.note = note.to_string();
            }
        }
        "priority" => {
            let value = request
                .value
                .as_ref()
                .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
                .ok_or("priority needs value 1..4")?;
            if !(1..=4).contains(&value) {
                return Err("priority needs value 1..4".to_string());
            }
            todo.priority = value;
        }
        "mode" => {
            let value = request
                .value
                .as_ref()
                .and_then(Value::as_str)
                .filter(|v| MODES.contains(v))
                .ok_or("mode needs value \"now\" or \"night\"")?;
            todo.mode = value.to_string();
        }
        other => {
            return Err(format!(
                "unknown action {other:?}; use release, block, done, close, park, edit, \
                 priority or mode"
            ));
        }
    }
    Ok(())
}

/// `POST /api/v1/shift/todos/:family/:id/action`
pub(crate) async fn todo_action(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((family, id)): AxumPath<(String, String)>,
    body: Bytes,
) -> AxumResponse {
    let request: TodoActionRequest = match parse(&body) {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let queue = match find_queue(&state, &family) {
        Ok(q) => q,
        Err(resp) => return *resp,
    };
    let family_repos: Vec<String> = queue
        .family
        .repos
        .iter()
        .map(|repo| repo.name.clone())
        .collect();
    let message = format!("{} {id} by {}/web", request.action, account.login);
    enum Refused {
        NotFound,
        Invalid(String),
    }
    let result = commit_change(
        &state.repo_manager,
        &queue,
        &account.login,
        &message,
        |existing| {
            let found = existing
                .iter()
                .find(|q| q.todo.id == id)
                .ok_or(Refused::NotFound)?;
            let mut todo = found.todo.clone();
            apply_action(&mut todo, &request, &family_repos).map_err(Refused::Invalid)?;
            Ok((vec![(found.path.clone(), Some(todo.dump()))], todo))
        },
    );
    match result {
        Ok(todo) => {
            // The inbox keeps its answer for a few seconds; an action a person
            // just took must show in the next read, not after that runs out.
            state.attention.invalidate();
            pipeline::emit(
                &state,
                NewEvent {
                    actor: Some(format!("{}/web", account.login)),
                    family: Some(queue.family.name.clone()),
                    todo_id: Some(todo.id.clone()),
                    outcome: Some(request.action.clone()),
                    reason: request.note.clone(),
                    detail: Some(json!({
                        "action": request.action,
                        "value": request.value,
                        "status": todo.status,
                    })),
                    ..NewEvent::forge(
                        "todo.action",
                        format!("{} by {}: {}", request.action, account.login, todo.title),
                    )
                },
            );
            Json(todo.to_api(Utc::now())).into_response()
        }
        Err(WriteError::Rejected(Refused::NotFound)) => shift_error(
            StatusCode::NOT_FOUND,
            "shift_todo_not_found",
            &format!("no todo {id:?} in family {family:?}"),
            "list todos with GET /api/v1/shift/todos?family=",
        ),
        Err(WriteError::Rejected(Refused::Invalid(reason))) => bad_request(&reason),
        Err(WriteError::Git(reason)) => git_failure(&reason),
    }
}

fn heartbeat_from(request: HeartbeatRequest) -> Result<Heartbeat, String> {
    let slot = match &request.slot {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err("slot must be a string".to_string()),
    };
    for (name, value) in [
        ("operator", &request.operator),
        ("host", &request.host),
        ("family", &request.family),
    ] {
        if value.trim().is_empty() {
            return Err(format!("{name} must not be empty"));
        }
    }
    if slot.trim().is_empty() {
        return Err("slot must not be empty".to_string());
    }
    if WorkerState::parse(&request.state).is_none() {
        let states = WorkerState::ALL.map(WorkerState::as_str);
        return Err(format!("state must be one of {states:?}"));
    }
    if let Some(stage) = &request.stage
        && !stage.is_empty()
        && !STAGES.contains(&stage.as_str())
    {
        return Err(format!("stage must be one of {STAGES:?}"));
    }
    let blank = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
    Ok(Heartbeat {
        operator: request.operator,
        host: request.host,
        slot,
        family: request.family,
        state: request.state,
        todo_id: blank(request.todo_id),
        stage: blank(request.stage),
        lease_until: blank(request.lease_until),
        shift: blank(request.shift),
        planned_slots: request.planned_slots,
        schedule: request.schedule,
        version: blank(request.version),
    })
}

/// `POST /api/v1/shift/heartbeat`
pub(crate) async fn heartbeat(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    body: Bytes,
) -> AxumResponse {
    let request: HeartbeatRequest = match parse(&body) {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let heartbeat = match heartbeat_from(request) {
        Ok(h) => h,
        Err(reason) => return bad_request(&reason),
    };
    let now = Utc::now();
    // Read the slot's previous beat first: a change of state, stage or todo
    // between two beats is a pipeline event; an unchanged beat is not.
    let previous = state
        .shift
        .heartbeats
        .latest_for_slot(&heartbeat)
        .ok()
        .flatten();
    if let Err(err) =
        state
            .shift
            .heartbeats
            .insert(&account.login, &heartbeat, now.timestamp_millis())
    {
        return shift_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "shift_heartbeat_store_failed",
            &err,
            "check <data_dir>/shift.sqlite",
        );
    }
    if let Some(event) = stage_event(previous.as_ref().map(|p| &p.heartbeat), &heartbeat) {
        pipeline::emit(&state, event);
    }
    Json(json!({
        "ok": true,
        "server_time": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }))
    .into_response()
}

/// `GET /api/v1/shift/workers`: every slot seen in the last 24 hours.
pub(crate) async fn workers(State(state): State<Arc<WebState>>) -> AxumResponse {
    let now = Utc::now().timestamp_millis();
    match state.shift.heartbeats.latest(now - 24 * 60 * 60 * 1000) {
        Ok(rows) => Json(WorkersResponse {
            generated_at: rfc3339_ms(now),
            workers: rows
                .into_iter()
                .map(|row| WorkerRow {
                    last_seen: rfc3339_ms(row.received_ms),
                    healthy: now - row.received_ms <= HEALTHY_MS,
                    heartbeat: row.heartbeat,
                })
                .collect(),
        })
        .into_response(),
        Err(err) => shift_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "shift_heartbeat_store_failed",
            &err,
            "check <data_dir>/shift.sqlite",
        ),
    }
}

/// `GET /api/v1/shift/workers/history?hours=24` (1..=336 hours).
pub(crate) async fn workers_history(
    State(state): State<Arc<WebState>>,
    Query(query): Query<HistoryQuery>,
) -> AxumResponse {
    let hours = query.hours.unwrap_or(24).clamp(1, 14 * 24);
    let to = Utc::now().timestamp_millis();
    let from = to - hours * 60 * 60 * 1000;
    match state.shift.heartbeats.between(from - HEALTHY_MS, to) {
        Ok(rows) => Json(heartbeats::build_history(&rows, from, to)).into_response(),
        Err(err) => shift_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "shift_heartbeat_store_failed",
            &err,
            "check <data_dir>/shift.sqlite",
        ),
    }
}

/// `GET /api/v1/shift/shifts?family=`
pub(crate) async fn list_shifts(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ShiftsQuery>,
) -> AxumResponse {
    if let Some(family) = query.family.as_deref().filter(|f| !f.is_empty())
        && let Err(resp) = find_queue(&state, family)
    {
        return *resp;
    }
    let mut all = Vec::new();
    for queue in discover(&state.repo_manager) {
        if let Some(family) = query.family.as_deref().filter(|f| !f.is_empty())
            && super::family::canonical(family) != queue.family.name
        {
            continue;
        }
        let todos = queue_todos(&state, &queue).unwrap_or_default();
        all.extend(shifts::list(&state, &queue, &todos));
    }
    all.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.branch.cmp(&b.branch)));
    Json(ShiftsResponse { shifts: all }).into_response()
}

/// `POST /api/v1/shift/shifts/:family/pr`
pub(crate) async fn open_shift_pr(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(family): AxumPath<String>,
    body: Bytes,
) -> AxumResponse {
    let request: ShiftPrRequest = match parse(&body) {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    let queue = match find_queue(&state, &family) {
        Ok(q) => q,
        Err(resp) => return *resp,
    };
    let todos = queue_todos(&state, &queue).unwrap_or_default();
    match shifts::open_prs(&state, &queue, &todos, &request.branch) {
        Ok(prs) if prs.is_empty() => shift_error(
            StatusCode::NOT_FOUND,
            "shift_branch_not_found",
            &format!(
                "no repo in family {family:?} has branch {:?}",
                request.branch
            ),
            "list shift branches with GET /api/v1/shift/shifts",
        ),
        Ok(prs) => {
            for pr in prs.iter().filter(|pr| pr.created) {
                pipeline::emit(
                    &state,
                    NewEvent {
                        actor: Some(format!("{}/web", account.login)),
                        family: Some(queue.family.name.clone()),
                        repo: Some(format!("{}/{}", queue.owner, pr.repo)),
                        pr: i64::try_from(pr.number).ok(),
                        shift: Some(request.branch.clone()),
                        detail: Some(json!({ "url": pr.url })),
                        ..NewEvent::forge(
                            "shift.pr_opened",
                            format!(
                                "opened {}#{} for shift {}",
                                pr.repo, pr.number, request.branch
                            ),
                        )
                    },
                );
            }
            Json(ShiftPrResponse { prs }).into_response()
        }
        Err(reason) => bad_request(&reason),
    }
}
