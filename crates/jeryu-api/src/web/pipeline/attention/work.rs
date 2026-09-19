//! Attention rules for the work queue: todos, shift branches and workers.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use super::{Draft, Item, STUCK_CLAIM_MINUTES, Severity, parse_time, todo_href};
use crate::web::shift::{ShiftBranch, ShiftRepo, ShiftTodo, WorkerRow};

/// When the todo last changed hands: its newest attempt's end, else filing.
fn todo_since(todo: &ShiftTodo) -> Option<String> {
    todo.worked_by
        .iter()
        .rev()
        .map(|attempt| attempt.ended.clone())
        .find(|ended| !ended.is_empty())
        .or_else(|| Some(todo.filed_at.clone()).filter(|filed| !filed.is_empty()))
}

/// Queue todos that wait on a person: blocked, handed off, untriaged, a dead
/// claim, or open behind a blocker that itself cannot move.
pub(crate) fn todo_items(family: &str, todos: &[ShiftTodo], now: DateTime<Utc>) -> Vec<Item> {
    let by_id: BTreeMap<&str, &ShiftTodo> = todos.iter().map(|t| (t.id.as_str(), t)).collect();
    let mut items = Vec::new();
    for todo in todos {
        let draft = |kind: &'static str, severity, title: String, reason: String, label| Draft {
            id: format!("{}:{family}:{}", kind.replace('_', "-"), todo.id),
            kind,
            severity,
            title,
            reason,
            href: todo_href(family, &todo.id),
            label,
            command: None,
        };
        let drafted = match todo.status.as_str() {
            "blocked" => Some(draft(
                "todo_blocked",
                Severity::Action,
                format!("Blocked: {}", todo.title),
                if todo.note.trim().is_empty() {
                    format!(
                        "This {family} todo is blocked after {} attempt(s) and nobody left a \
                         note saying why. No worker will pick it up again until somebody \
                         releases it.",
                        todo.attempts
                    )
                } else {
                    format!(
                        "This {family} todo is blocked, so no worker will pick it up again \
                         until somebody releases it. The note left on it: {}",
                        todo.note.trim()
                    )
                },
                "Read the note, fix or re-scope the todo, then release it",
            )),
            "handoff" => Some(draft(
                "todo_handoff",
                Severity::Action,
                format!("Handed to a person: {}", todo.title),
                format!(
                    "This {family} todo was handed off for a person to finish. {}",
                    todo.note.trim()
                ),
                "Finish the work by hand or release the todo back to the workers",
            )),
            "claimed" => {
                let dead_for = parse_time(&todo.lease_until).map(|lease| now - lease);
                dead_for
                    .filter(|gone| !todo.lease_live && gone.num_minutes() >= STUCK_CLAIM_MINUTES)
                    .map(|gone| {
                        draft(
                            "todo_stuck_claim",
                            Severity::Watch,
                            format!("Stuck claim: {}", todo.title),
                            format!(
                                "{} claimed this {family} todo but stopped renewing its lease \
                                 {} minutes ago, so the worker probably died mid-run. Another \
                                 worker reclaims it on its next pass; if it stays here, release \
                                 it by hand.",
                                todo.claim_by,
                                gone.num_minutes()
                            ),
                            "Release the claim if no worker reclaims it",
                        )
                    })
            }
            "open" if !todo.triaged => Some(draft(
                "todo_untriaged",
                Severity::Action,
                format!("Needs triage: {}", todo.title),
                format!(
                    "This {family} todo was filed without a title and repos of its own, so \
                     workers skip it until it is triaged."
                ),
                "Set the todo's title and repos so a worker can claim it",
            )),
            "open" => todo
                .blocked_by
                .iter()
                .filter_map(|id| by_id.get(id.as_str()))
                .find_map(|blocker| match blocker.status.as_str() {
                    "blocked" | "handoff" => Some(format!(
                        "It waits on \"{}\" ({}), which is {} and will not move without a person.",
                        blocker.title, blocker.id, blocker.status
                    )),
                    "done" if !blocker.merged => Some(format!(
                        "It waits on \"{}\" ({}), which is done but not merged to the base \
                         branch yet. Merging that shift's pull request unblocks it.",
                        blocker.title, blocker.id
                    )),
                    _ => None,
                })
                .map(|why| {
                    draft(
                        "todo_waiting_on_blocker",
                        Severity::Watch,
                        format!("Waiting on a blocker: {}", todo.title),
                        format!("No worker may start this {family} todo yet. {why}"),
                        "Clear the blocker it names",
                    )
                }),
            _ => None,
        };
        if let Some(draft) = drafted {
            let mut item = draft.build();
            item.since = if item.kind == "todo_stuck_claim" {
                Some(todo.lease_until.clone())
            } else {
                todo_since(todo)
            };
            item.family = Some(family.to_string());
            item.todo_id = Some(todo.id.clone());
            item.shift = Some(todo.shift.clone()).filter(|shift| !shift.is_empty());
            items.push(item);
        }
    }
    items
}

/// Shift branches holding work that nobody has asked anyone to review.
pub(crate) fn shift_items(family: &str, shifts: &[ShiftBranch]) -> Vec<Item> {
    let mut items = Vec::new();
    for shift in shifts {
        for repo in &shift.repos {
            // Work that landed on the branch after its pull request merged
            // is stranded: the branch looks done, and nothing will review it.
            let stranded = repo.pr.as_ref().is_some_and(|pr| pr.state == "merged")
                && !repo.unmerged_todos.is_empty();
            if stranded {
                items.push(stranded_item(family, shift, repo));
                continue;
            }
            let unreviewed =
                repo.ahead > 0 && repo.pr.as_ref().is_none_or(|pr| pr.state == "closed");
            if !unreviewed {
                continue;
            }
            let mut item = Draft {
                id: format!("shift-without-pr:{family}:{}:{}", repo.repo, shift.branch),
                kind: "shift_without_pr",
                severity: Severity::Action,
                title: format!(
                    "{} has work on {} and no pull request",
                    repo.repo, shift.branch
                ),
                reason: format!(
                    "The {family} shift branch {} in {} holds {} commit(s){} that are not on \
                     the base branch, and no open pull request asks for them to be reviewed, \
                     so the work cannot land.",
                    shift.branch,
                    repo.repo,
                    repo.ahead,
                    if shift.todo_ids.is_empty() {
                        String::new()
                    } else {
                        format!(" from {} todo(s)", shift.todo_ids.len())
                    }
                ),
                href: format!("/work/shift?family={family}"),
                label: "Open the shift's review PR",
                command: None,
            }
            .build();
            item.family = Some(family.to_string());
            item.repo = Some(repo.repo.clone());
            item.sha = Some(repo.head.clone());
            item.shift = Some(shift.branch.clone());
            items.push(item);
        }
    }
    items
}

/// Families with queued work and no healthy worker slot to do it.
pub(crate) fn worker_items(families: &[(String, usize)], workers: &[WorkerRow]) -> Vec<Item> {
    let mut items = Vec::new();
    for (family, waiting) in families.iter().filter(|(_, waiting)| *waiting > 0) {
        let slots: Vec<&WorkerRow> = workers
            .iter()
            .filter(|w| &w.heartbeat.family == family && w.heartbeat.slot != "supervisor")
            .collect();
        if slots.iter().any(|w| w.healthy) {
            continue;
        }
        let last_seen = slots.iter().map(|w| w.last_seen.clone()).max();
        let mut item = Draft {
            id: format!("workers-down:{family}"),
            kind: "workers_down",
            severity: Severity::Critical,
            title: format!("No {family} worker is running"),
            reason: format!(
                "{waiting} {family} todo(s) are open or claimed and no worker slot for the \
                 family has sent a heartbeat in the last 2 minutes{}. Nothing will be \
                 worked until a worker runs again; workers are started by the todoq \
                 supervisor unit on the operator's host.",
                last_seen
                    .as_ref()
                    .map(|seen| format!(" (last seen {seen})"))
                    .unwrap_or_default()
            ),
            href: "/work/shift/workers".to_string(),
            label: "Check the todoq supervisor on the worker host",
            command: Some(format!("systemctl --user status todoq-supervisor@{family}")),
        }
        .build();
        item.since = last_seen;
        item.family = Some(family.clone());
        items.push(item);
    }
    items
}

fn stranded_item(family: &str, shift: &ShiftBranch, repo: &ShiftRepo) -> Item {
    let mut item = Draft {
        id: format!("shift-stranded:{family}:{}:{}", repo.repo, shift.branch),
        kind: "shift_stranded_work",
        severity: Severity::Action,
        title: format!(
            "{} has finished work on {} that never reached the base branch",
            repo.repo, shift.branch
        ),
        reason: format!(
            "The pull request for the {family} shift branch {} in {} already merged, but {} \
             todo(s) landed on the branch afterwards ({}). Their commits are on no open pull \
             request and not on the base branch, so the work is finished and going nowhere.",
            shift.branch,
            repo.repo,
            repo.unmerged_todos.len(),
            repo.unmerged_todos.join(", ")
        ),
        href: format!("/work/shift?family={family}"),
        label: "Open a new review PR for the branch",
        command: None,
    }
    .build();
    item.family = Some(family.to_string());
    item.repo = Some(repo.repo.clone());
    item.sha = Some(repo.head.clone());
    item.shift = Some(shift.branch.clone());
    item.todo_id = repo.unmerged_todos.first().cloned();
    item
}
