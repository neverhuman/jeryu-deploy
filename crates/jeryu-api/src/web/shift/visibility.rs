//! What the pipeline event log and the attention inbox need from the shift
//! queue: a slot's stage changes as events, a pull request's family and shift
//! branch, and a snapshot of every family's todos, shifts and workers.

use chrono::{DateTime, Utc};
use serde_json::json;

use super::super::WebState;
use super::super::pipeline::NewEvent;
use super::heartbeats::{HEALTHY_MS, rfc3339_ms};
use super::queue::discover;
use super::types::{Heartbeat, ShiftBranch, ShiftTodo, WorkerRow};
use super::{queue_todos, shifts};

/// What a slot is doing, in one word: its stage while working, else its state.
fn slot_label(heartbeat: &Heartbeat) -> &str {
    match (heartbeat.state.as_str(), heartbeat.stage.as_deref()) {
        ("working", Some(stage)) => stage,
        (state, _) => state,
    }
}

/// The `worker.stage` event for a slot whose state, stage or todo changed
/// since its previous beat. A slot's first beat is an event only when it is
/// already doing something: an idle slot appearing is not news.
pub(crate) fn stage_event(previous: Option<&Heartbeat>, current: &Heartbeat) -> Option<NewEvent> {
    let unchanged = previous.is_some_and(|p| {
        p.state == current.state && p.stage == current.stage && p.todo_id == current.todo_id
    });
    if unchanged || (previous.is_none() && current.state == "idle") {
        return None;
    }
    let mut summary = format!("{} {}: ", current.slot, current.family);
    if let Some(previous) = previous {
        summary.push_str(&format!("{} -> ", slot_label(previous)));
    }
    summary.push_str(slot_label(current));
    let todo = current
        .todo_id
        .as_ref()
        .or_else(|| previous.and_then(|p| p.todo_id.as_ref()));
    if let Some(todo) = todo {
        summary.push_str(&format!(" on {todo}"));
    }
    Some(NewEvent {
        actor: Some(format!("{}/{}", current.operator, current.slot)),
        family: Some(current.family.clone()),
        todo_id: todo.cloned(),
        shift: current.shift.clone(),
        outcome: (current.state == "paused").then(|| "paused".to_string()),
        detail: Some(json!({
            "host": current.host,
            "state": current.state,
            "stage": current.stage,
            "previous_state": previous.map(|p| &p.state),
            "previous_stage": previous.and_then(|p| p.stage.as_ref()),
        })),
        ..NewEvent::forge("worker.stage", summary)
    })
}

/// The family that works `owner/repo` and, when `branch` is one of that
/// family's shift branches, the branch: the join keys a pull request event
/// needs to show up on a todo's trace.
pub(crate) fn shift_context(
    state: &WebState,
    owner: &str,
    repo: &str,
    branch: &str,
) -> (Option<String>, Option<String>) {
    for queue in discover(&state.repo_manager) {
        if queue.family.repos.iter().any(|r| r.name == repo)
            && super::truth::hosted_owner(state, &queue, repo).as_deref() == Some(owner)
        {
            let shift = shifts::classify(&queue, branch).map(|_| branch.to_string());
            return (Some(queue.family.name), shift);
        }
    }
    (None, None)
}

/// One family's queue and shift branches, for the attention inbox.
pub(crate) struct FamilySnapshot {
    pub name: String,
    pub todos: Vec<ShiftTodo>,
    pub shifts: Vec<ShiftBranch>,
}

/// Every family's todos (with derived merged/released) and shift branches.
pub(crate) fn attention_snapshot(state: &WebState, now: DateTime<Utc>) -> Vec<FamilySnapshot> {
    discover(&state.repo_manager)
        .into_iter()
        .map(|queue| {
            let queued = queue_todos(state, &queue).unwrap_or_default();
            let mut todos: Vec<ShiftTodo> = queued.iter().map(|q| q.todo.to_api(now)).collect();
            state.shift.truth.enrich(state, &queue, &mut todos);
            FamilySnapshot {
                shifts: shifts::list(state, &queue, &queued),
                name: queue.family.name,
                todos,
            }
        })
        .collect()
}

/// Every slot seen in the last 24 hours, with its health at `now`.
pub(crate) fn worker_rows(state: &WebState, now: DateTime<Utc>) -> Vec<WorkerRow> {
    let now = now.timestamp_millis();
    state
        .shift
        .heartbeats
        .latest(now - 24 * 60 * 60 * 1000)
        .unwrap_or_default()
        .into_iter()
        .map(|row| WorkerRow {
            last_seen: rfc3339_ms(row.received_ms),
            healthy: now - row.received_ms <= HEALTHY_MS,
            heartbeat: row.heartbeat,
        })
        .collect()
}

/// Where one todo lives, for a surface that follows a single piece of work:
/// the todo with its derived truth, and what its repositories are called and
/// who hosts them.
pub(crate) struct TodoTrace {
    pub todo: ShiftTodo,
    pub base_branch: String,
    /// Every repo the todo committed to, as (family repo name, hosted owner).
    pub owners: Vec<(String, String)>,
}

/// The todos named by `ids`, with derived merged/released and the carrying
/// pull requests filled in (see `truth.rs`). An id no family queue holds is
/// left out, so the caller can tell "no such todo" from "nothing happened".
pub(crate) fn todo_traces(state: &WebState, ids: &[String], now: DateTime<Utc>) -> Vec<TodoTrace> {
    let mut traces = Vec::new();
    for queue in discover(&state.repo_manager) {
        let queued = queue_todos(state, &queue).unwrap_or_default();
        let mut todos: Vec<ShiftTodo> = queued
            .iter()
            .map(|q| q.todo.to_api(now))
            .filter(|todo| ids.iter().any(|id| id == &todo.id))
            .collect();
        if todos.is_empty() {
            continue;
        }
        state.shift.truth.enrich(state, &queue, &mut todos);
        for todo in todos {
            let owners = todo
                .commits
                .keys()
                .filter_map(|repo| {
                    let owner = super::truth::hosted_owner(state, &queue, repo)?;
                    Some((repo.clone(), owner))
                })
                .collect();
            traces.push(TodoTrace {
                todo,
                base_branch: queue.family.base_branch.clone(),
                owners,
            });
        }
    }
    traces
}

/// The todo ids that the commits in `range` (any revision range git log takes,
/// `base..head`) carry as a `Todo:` trailer: how a pull request is joined to
/// the work it carries.
pub(crate) fn trailer_todos(state: &WebState, owner: &str, repo: &str, range: &str) -> Vec<String> {
    let Ok(opened) = state.repo_manager.open_parts(owner, repo) else {
        return Vec::new();
    };
    let git_bin = &state.repo_manager.config().git_bin;
    let mut ids: Vec<String> = super::truth::scan_trailers(git_bin, &opened.path, range)
        .into_keys()
        .collect();
    ids.sort();
    ids
}

/// The commit on `base_branch` that carries `Todo: <id>`, when one does: where
/// a todo's work ended up after its shift branch was replayed onto the base.
pub(crate) fn trailer_commit(
    state: &WebState,
    owner: &str,
    repo: &str,
    base_branch: &str,
    id: &str,
) -> Option<String> {
    let opened = state.repo_manager.open_parts(owner, repo).ok()?;
    let git_bin = &state.repo_manager.config().git_bin;
    let base = format!("refs/heads/{base_branch}");
    super::truth::scan_trailers(git_bin, &opened.path, &base)
        .get(id)
        .cloned()
}
