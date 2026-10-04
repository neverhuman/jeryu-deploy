//! Attention rules for the work queue: todos, shift branches and workers.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use super::{
    ApiCall, Draft, Hosts, Item, STUCK_CLAIM_MINUTES, Severity, Shell, UNTRIAGED_MINUTES,
    WORKERS_HREF, open_shift_pr_call, parse_time, todo_action_call, todo_href, work_href,
};
use crate::web::shift::{BlockKind, ShiftBranch, ShiftRepo, ShiftTodo, TodoStatus, WorkerRow};

/// When the todo last changed hands: its newest attempt's end, else filing.
fn todo_since(todo: &ShiftTodo) -> Option<String> {
    todo.worked_by
        .iter()
        .rev()
        .map(|attempt| attempt.ended.clone())
        .find(|ended| !ended.is_empty())
        .or_else(|| Some(todo.filed_at.clone()).filter(|filed| !filed.is_empty()))
}

/// A note as one closed sentence, so whatever follows it reads as its own.
fn sentence(note: &str) -> String {
    let note = note.trim();
    if note.ends_with(['.', '!', '?', ':', ';']) {
        note.to_string()
    } else {
        format!("{note}.")
    }
}

/// What a blocked todo's kind means for a person: the sentence that closes
/// its reason, and the one step its action offers. Releasing is offered only
/// where releasing can help: an over-budget todo sums its spend over every
/// attempt, so the next attempt stops where the last one did.
fn blocked_step(family: &str, kind: BlockKind) -> (String, &'static str) {
    match kind {
        BlockKind::OwnerTask => (
            format!(
                "Only the owner can do this, so handing it back to a {family} worker would \
                 block it again."
            ),
            "Do it, then mark done",
        ),
        BlockKind::OverBudget => (
            "Spend is summed over every attempt, so releasing it stops it again at once."
                .to_string(),
            "Close and refile smaller, or raise the cap",
        ),
        BlockKind::UnknownRepo => (
            format!("No {family} worker can clone a repo the family config does not list."),
            "Add the repo to the family config, then release",
        ),
        BlockKind::Handoff | BlockKind::AgentBlocked => (
            format!("No {family} worker picks it up until it is released."),
            "Release the todo",
        ),
    }
}

/// The todo transition that performs a blocked todo's step, where one does it
/// whole. Releasing an over-budget todo only stops it again, and an unknown
/// repo has to be added to the family config first: both steps start outside
/// this API, so neither names a call.
fn blocked_action(kind: BlockKind) -> Option<&'static str> {
    match kind {
        BlockKind::OwnerTask => Some("done"),
        BlockKind::Handoff | BlockKind::AgentBlocked => Some("release"),
        BlockKind::OverBudget | BlockKind::UnknownRepo => None,
    }
}

/// The call that performs one todo item's step, for the kinds whose step is a
/// transition of the todo. A step that waits on a person's judgement — what
/// the title should be, whether a worker is really gone — names none: the
/// item is for a reader, and `action.api` is for replaying a decision already
/// made.
fn todo_api(family: &str, todo: &ShiftTodo, item: &Item) -> Option<ApiCall> {
    let action = match item.kind {
        "todo_blocked" => blocked_action(todo.block_kind.unwrap_or(BlockKind::AgentBlocked))?,
        // A park that has not run out is nobody's step yet.
        "todo_parked" if item.severity == Severity::Action => "release",
        "todo_handoff" => "release",
        _ => return None,
    };
    Some(todo_action_call(family, &todo.id, action))
}

/// Queue todos that wait on a person: blocked, handed off, untriaged, a dead
/// claim, or open behind a blocker that itself cannot move.
/// `has_worker` says whether the family has a healthy worker slot: a worker
/// triages an untriaged todo on its next pass, so one only waits on a person
/// once no worker has got to it for a long time.
pub(crate) fn todo_items(
    family: &str,
    todos: &[ShiftTodo],
    has_worker: bool,
    now: DateTime<Utc>,
) -> Vec<Item> {
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
            api: None,
            command: None,
        };
        let drafted = match todo.status {
            TodoStatus::Blocked => {
                // What a worker cannot get past decides what is asked of a
                // person: releasing an over-budget todo only re-blocks it, and
                // an owner's task is not a worker's to pick up at all.
                let kind = todo.block_kind.unwrap_or(BlockKind::AgentBlocked);
                let (consequence, label) = blocked_step(family, kind);
                Some(draft(
                    "todo_blocked",
                    Severity::Action,
                    format!("Blocked: {}", todo.title),
                    // The note is the information: it leads, verbatim. What
                    // being blocked means comes last and stays short.
                    if todo.note.trim().is_empty() {
                        format!(
                            "No note says why: it was blocked after {} attempt(s). \
                             {consequence}",
                            todo.attempts
                        )
                    } else {
                        format!("{} {consequence}", sentence(&todo.note))
                    },
                    label,
                ))
            }
            TodoStatus::Parked => {
                // A park with a date comes back by itself, so until then it is
                // only worth watching; without one, or once the date has
                // passed, it waits on a person again.
                let due = parse_time(&todo.park_until).is_none_or(|until| until <= now);
                let why = if todo.note.trim().is_empty() {
                    "Somebody set this todo aside.".to_string()
                } else {
                    sentence(&todo.note)
                };
                let consequence = match (todo.park_until.as_str(), due) {
                    (until, false) => format!(
                        "It is parked until {until}, so no {family} worker picks it up before \
                         then."
                    ),
                    ("", true) => format!(
                        "It is parked with no date it comes back on, so no {family} worker \
                         picks it up until a person decides."
                    ),
                    (until, true) => format!(
                        "The park ran out at {until} and it is still parked, so no {family} \
                         worker picks it up."
                    ),
                };
                Some(draft(
                    "todo_parked",
                    if due {
                        Severity::Action
                    } else {
                        Severity::Watch
                    },
                    format!("Parked: {}", todo.title),
                    format!("{why} {consequence}"),
                    if due {
                        "Release the todo or park it again"
                    } else {
                        "Nothing to do until the park runs out"
                    },
                ))
            }
            TodoStatus::Handoff => Some(draft(
                "todo_handoff",
                Severity::Action,
                format!("Handed to a person: {}", todo.title),
                if todo.note.trim().is_empty() {
                    format!("A {family} worker handed this todo to a person and left no note.")
                } else {
                    format!(
                        "{} A {family} worker handed this todo to a person.",
                        sentence(&todo.note)
                    )
                },
                "Finish the work by hand or release the todo back to the workers",
            )),
            TodoStatus::Claimed => {
                let dead_for = parse_time(&todo.lease_until).map(|lease| now - lease);
                dead_for
                    .filter(|gone| !todo.lease_live && gone.num_minutes() >= STUCK_CLAIM_MINUTES)
                    .map(|gone| {
                        draft(
                            "todo_stuck_claim",
                            Severity::Watch,
                            format!("Stuck claim: {}", todo.title),
                            format!(
                                "{} stopped renewing its lease {} minutes ago, so the worker \
                                 probably died mid-run. Another {family} worker reclaims the \
                                 todo on its next pass; release it by hand only if it stays here.",
                                todo.claim_by,
                                gone.num_minutes()
                            ),
                            "Release the claim if no worker reclaims it",
                        )
                    })
            }
            // A {family} worker triages a one-line todo itself on its next
            // pass, so a fresh one is nobody's to look at. Only when a healthy
            // worker has had it for far longer than a pass does the triage look
            // like it is not coming.
            TodoStatus::Open if !todo.triaged => todo_since(todo)
                .as_deref()
                .and_then(parse_time)
                .map(|since| (now - since).num_minutes())
                .filter(|waited| has_worker && *waited > UNTRIAGED_MINUTES)
                .map(|waited| {
                    draft(
                        "todo_untriaged",
                        Severity::Watch,
                        format!("Still untriaged: {}", todo.title),
                        format!(
                            "It was filed without a title and repos of its own. {family} \
                             workers triage on their next pass; not triaged after {waited} \
                             minutes.",
                        ),
                        "Set the todo's title and repos yourself if no worker does",
                    )
                }),
            TodoStatus::Open => todo
                .blocked_by
                .iter()
                .filter_map(|id| by_id.get(id.as_str()))
                .find_map(|blocker| match blocker.status {
                    TodoStatus::Blocked | TodoStatus::Handoff => Some(format!(
                        "It waits on \"{}\" ({}), which is {} and will not move without a \
                         person.",
                        blocker.title,
                        blocker.id,
                        blocker.status.as_str()
                    )),
                    TodoStatus::Done if !blocker.merged => Some(format!(
                        "It waits on \"{}\" ({}), which is done but not merged to the base \
                         branch yet. Merging that shift's pull request unblocks it.",
                        blocker.title, blocker.id
                    )),
                    TodoStatus::Done
                    | TodoStatus::Open
                    | TodoStatus::Claimed
                    | TodoStatus::Parked
                    | TodoStatus::Closed => None,
                })
                .map(|why| {
                    draft(
                        "todo_waiting_on_blocker",
                        Severity::Watch,
                        format!("Waiting on a blocker: {}", todo.title),
                        format!("{why} No {family} worker may start it until then."),
                        "Clear the blocker it names",
                    )
                }),
            TodoStatus::Done | TodoStatus::Closed => None,
        };
        if let Some(draft) = drafted {
            let mut item = draft.build();
            item.action.api = todo_api(family, todo, &item);
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
            // A linear-history merge replays commits under new shas, so a
            // branch whose pull request was closed and replaced by a rebased
            // one stays "ahead" for ever. Only todos that are on no base
            // commit are work that still needs a review.
            let unreviewed = !repo.unmerged_todos.is_empty()
                && repo.pr.as_ref().is_none_or(|pr| pr.state == "closed");
            if !unreviewed {
                continue;
            }
            // When the branch's own pull request was closed and replaced, the
            // todos the replacement carries are under review already; only the
            // ones no open pull request carries still wait for one.
            let waiting: Vec<&String> = repo
                .unmerged_todos
                .iter()
                .filter(|id| !repo.reviewed_todos.contains(id))
                .collect();
            if waiting.is_empty() {
                continue;
            }
            let ids = waiting
                .iter()
                .map(|id| id.as_str())
                .collect::<Vec<&str>>()
                .join(", ");
            let mut item = Draft {
                id: format!("shift-without-pr:{family}:{}:{}", repo.repo, shift.branch),
                kind: "shift_without_pr",
                severity: Severity::Action,
                title: format!(
                    "{} has work on {} and no pull request",
                    repo.repo, shift.branch
                ),
                reason: match &repo.review_pr {
                    Some(pr) => format!(
                        "{} finished todo(s) ({ids}) sit on {} in {} and no open pull request \
                         asks for a review: the shift's own pull request was closed \
                         and the open #{} that replaces it carries the rest of its \
                         todos, not these.",
                        waiting.len(),
                        shift.branch,
                        repo.repo,
                        pr.number
                    ),
                    None => format!(
                        "{} finished todo(s) ({ids}) sit on {} in {} and no open pull request \
                         asks for a review, so the work cannot land.",
                        waiting.len(),
                        shift.branch,
                        repo.repo
                    ),
                },
                href: repo
                    .review_pr
                    .as_ref()
                    .map_or_else(|| work_href(family), |pr| pr.url.clone()),
                label: match repo.review_pr {
                    Some(_) => "Add the missing todo(s) to the open review PR",
                    None => "Open the shift's review PR",
                },
                // Opening the branch's review pull request is one call; moving
                // todos into somebody else's open pull request is not.
                api: match repo.review_pr {
                    Some(_) => None,
                    None => Some(open_shift_pr_call(family, &shift.branch)),
                },
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
pub(crate) fn worker_items(
    families: &[(String, usize)],
    workers: &[WorkerRow],
    hosts: &Hosts,
) -> Vec<Item> {
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
        // The family's own host, from whichever of its rows (the supervisor
        // included) reported last; the release host when none ever did.
        let host = workers
            .iter()
            .filter(|w| &w.heartbeat.family == family && !w.heartbeat.host.trim().is_empty())
            .max_by(|a, b| a.last_seen.cmp(&b.last_seen))
            .map_or(hosts.release.as_str(), |w| w.heartbeat.host.trim());
        let mut item = Draft {
            id: format!("workers-down:{family}"),
            kind: "workers_down",
            severity: Severity::Critical,
            title: format!("No {family} worker is running"),
            reason: format!(
                "{waiting} {family} todo(s) wait and no {family} worker slot has sent a \
                 heartbeat in the last 2 minutes{}. The todoq supervisor on the operator's \
                 host starts the workers.",
                last_seen
                    .as_ref()
                    .map(|seen| format!(" (last seen {seen})"))
                    .unwrap_or_default()
            ),
            href: WORKERS_HREF.to_string(),
            label: "Check the todoq supervisor on the worker host",
            api: None,
            command: Some(Shell {
                line: format!("systemctl --user status todoq-supervisor@{family}"),
                run_in: Hosts::anywhere(host),
            }),
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
            "{} todo(s) ({}) landed on {} in {} after its pull request already merged. They \
             are on no open pull request and not on the base branch: finished {family} work \
             that is going nowhere.",
            repo.unmerged_todos.len(),
            repo.unmerged_todos.join(", "),
            shift.branch,
            repo.repo
        ),
        href: work_href(family),
        label: "Open a new review PR for the branch",
        // The branch's merged pull request is behind it: asking again opens a
        // fresh one for what is still unmerged.
        api: Some(open_shift_pr_call(family, &shift.branch)),
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
