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
        if queue.owner == owner && queue.family.repos.iter().any(|r| r.name == repo) {
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
