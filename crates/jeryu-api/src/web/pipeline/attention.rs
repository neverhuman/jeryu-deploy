//! The attention inbox: `GET /api/v1/attention`.
//!
//! Everything in the pipeline that is waiting on a person, computed from the
//! current state on each call (cached for a few seconds) and never from old
//! events, so an item disappears when its cause is fixed. One small collector
//! per source gathers plain facts; one pure rule per source turns facts into
//! items, which is what the tests drive.
//!
//! Each item names exactly one next step. `action.command` is set when the
//! step is a shell command to run off-site; otherwise the step is to open
//! `href` and do what `action.label` says. `next_step` spells that out in one
//! sentence for a reader, human or agent, with no other context.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::super::WebState;
use super::super::control_plane::{GateRunnerRecord, is_online, is_reviewer};
use super::super::merge_queue::{QueueEntry, QueueState};
use super::super::pulls::PullPosture;
use super::super::shift::{FamilySnapshot, ShiftBranch, ShiftTodo, WorkerRow};
use super::Event;

pub(crate) const ATTENTION_SCHEMA: &str = "jeryu.attention/v1";
const CACHE_FOR: Duration = Duration::from_secs(10);
/// A claim whose lease died this long ago is stuck rather than between renewals.
const STUCK_CLAIM_MINUTES: i64 = 10;
/// A mergeable PR left open this long is waiting on somebody to merge it.
const READY_TO_MERGE_MINUTES: i64 = 10;
const QUEUE_LOOKBACK_HOURS: i64 = 24;
const MAX_REASON_CHARS: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    /// The pipeline is stuck or broken.
    Critical,
    /// A human decision or click is the next step.
    Action,
    /// Unusual; may heal by itself.
    Watch,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Action {
    pub label: String,
    /// A copyable shell line when the step happens off-site.
    pub command: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Item {
    pub id: String,
    pub kind: &'static str,
    pub severity: Severity,
    pub title: String,
    pub reason: String,
    pub since: Option<String>,
    pub family: Option<String>,
    pub repo: Option<String>,
    pub pr: Option<u64>,
    pub todo_id: Option<String>,
    pub sha: Option<String>,
    pub shift: Option<String>,
    pub href: String,
    pub action: Action,
    pub next_step: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct Counts {
    pub critical: usize,
    pub action: usize,
    pub watch: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct AttentionResponse {
    pub schema_version: &'static str,
    pub generated_at: String,
    pub items: Vec<Item>,
    pub counts: Counts,
}

/// The few fields an item is built from; the rest default to `None`.
struct Draft<'a> {
    id: String,
    kind: &'static str,
    severity: Severity,
    title: String,
    reason: String,
    href: String,
    label: &'a str,
    command: Option<String>,
}

impl Draft<'_> {
    fn build(self) -> Item {
        let next_step = match &self.command {
            Some(command) => format!("{}: run `{command}`", self.label),
            None => format!("{}: open {}", self.label, self.href),
        };
        Item {
            id: self.id,
            kind: self.kind,
            severity: self.severity,
            title: self.title,
            reason: clip(&self.reason),
            since: None,
            family: None,
            repo: None,
            pr: None,
            todo_id: None,
            sha: None,
            shift: None,
            href: self.href,
            action: Action {
                label: self.label.to_string(),
                command: self.command,
            },
            next_step,
        }
    }
}

fn clip(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_REASON_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_REASON_CHARS - 1).collect();
    out.push('…');
    out
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn todo_href(family: &str, id: &str) -> String {
    format!("/work/shift?family={family}&todo={id}")
}

fn pull_href(repo: &str, number: u64) -> String {
    format!("/repos/jeryu/{repo}/pulls/{number}")
}

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

/// An open pull request and where its merge gate stands.
#[derive(Clone, Debug)]
pub(crate) struct PullFacts {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub author: String,
    pub head_sha: String,
    pub updated_at: DateTime<Utc>,
    pub posture: PullPosture,
}

/// Open pull requests waiting on a person: changes requested, red checks,
/// missing approvals, or mergeable and simply not merged.
pub(crate) fn pull_items(pulls: &[PullFacts], now: DateTime<Utc>) -> Vec<Item> {
    let mut items = Vec::new();
    for pull in pulls {
        let posture = &pull.posture;
        let label = format!("{}#{}", pull.repo, pull.number);
        let verdict: Option<(&'static str, String, String, &str)> = if posture.changes_requested > 0
        {
            Some((
                "pr_changes_requested",
                format!("Changes requested on {label}"),
                format!(
                    "A reviewer asked for changes on the current head of \"{}\" by {}. It \
                     cannot merge until the author pushes a fix or the reviewer withdraws \
                     the request. The review on the pull request page says what is wrong.",
                    pull.title, pull.author
                ),
                "Read the review and push a fix, or dismiss it",
            ))
        } else if !posture.failing.is_empty() {
            Some((
                "pr_checks_failing",
                format!("Checks failing on {label}"),
                format!(
                    "\"{}\" by {} cannot merge: {} failed on its current head. The checks \
                     panel on the pull request page links each failing check's details.",
                    pull.title,
                    pull.author,
                    posture.failing.join(", ")
                ),
                "Open the failing check and fix or re-run it",
            ))
        } else if posture.checks_green && posture.approvals < posture.required_approvals {
            Some((
                "pr_awaiting_approval",
                format!("{label} is waiting for approval"),
                format!(
                    "\"{}\" by {} has green checks and {} of {} required approval(s). It \
                     needs a reviewer other than its author to approve the current head.",
                    pull.title, pull.author, posture.approvals, posture.required_approvals
                ),
                "Review and approve the pull request",
            ))
        } else if posture.can_merge
            && (now - pull.updated_at).num_minutes() >= READY_TO_MERGE_MINUTES
        {
            Some((
                "pr_ready_to_merge",
                format!("{label} is ready to merge"),
                format!(
                    "\"{}\" by {} passes its merge gate (checks green, approvals in) and \
                     has sat open for {} minutes. Nothing automatic is going to merge it.",
                    pull.title,
                    pull.author,
                    (now - pull.updated_at).num_minutes()
                ),
                "Merge the pull request",
            ))
        } else {
            None
        };
        if let Some((kind, title, reason, step)) = verdict {
            let mut item = Draft {
                id: format!("{}:{}:{}", kind.replace('_', "-"), pull.repo, pull.number),
                kind,
                severity: Severity::Action,
                title,
                reason,
                href: pull_href(&pull.repo, pull.number),
                label: step,
                command: None,
            }
            .build();
            item.since = Some(pull.updated_at.to_rfc3339());
            item.repo = Some(pull.repo.clone());
            item.pr = Some(pull.number);
            item.sha = Some(pull.head_sha.clone());
            items.push(item);
        }
    }
    items
}

/// Merge-queue entries that failed or were dropped in the last day while
/// their pull request is still open: an approved PR that will not land alone.
pub(crate) fn queue_items(
    entries: &[QueueEntry],
    open_pulls: &BTreeSet<(String, u64)>,
    now: DateTime<Utc>,
) -> Vec<Item> {
    let mut items = Vec::new();
    for entry in entries {
        if !matches!(entry.state, QueueState::Failed | QueueState::Dequeued)
            || !open_pulls.contains(&(entry.repo.clone(), entry.number))
        {
            continue;
        }
        let at = entry
            .attempts
            .last()
            .map_or(entry.enqueued_at.as_str(), |attempt| attempt.at.as_str());
        let recent = parse_time(at).is_some_and(|at| (now - at).num_hours() < QUEUE_LOOKBACK_HOURS);
        if !recent {
            continue;
        }
        let mut item = Draft {
            id: format!("queue-failed:{}:{}", entry.repo, entry.number),
            kind: "queue_failed",
            severity: Severity::Action,
            title: format!(
                "{}#{} fell out of the merge queue",
                entry.repo, entry.number
            ),
            reason: format!(
                "The merge queue tried to land this approved pull request onto {} and gave \
                 up: {}. It stays open and will not be retried until somebody queues it \
                 again, usually after a rebase or a fix.",
                entry.base,
                entry.reason.as_deref().unwrap_or("no reason was recorded")
            ),
            href: pull_href(&entry.repo, entry.number),
            label: "Fix what the reason names, then queue the pull request again",
            command: None,
        }
        .build();
        item.since = Some(at.to_string());
        item.repo = Some(entry.repo.clone());
        item.pr = Some(entry.number);
        item.sha = Some(entry.pr_head_sha.clone());
        items.push(item);
    }
    items
}

/// Reviewers that could not review an open PR, and a gate with no runner.
pub(crate) fn runner_items(
    runners: &[GateRunnerRecord],
    open_pulls: &BTreeSet<(String, u64)>,
    queue_building: bool,
    now: DateTime<Utc>,
) -> Vec<Item> {
    let mut items = Vec::new();
    for record in runners.iter().filter(|r| is_reviewer(&r.heartbeat)) {
        let Some(last) = &record.heartbeat.last else {
            continue;
        };
        let explained = match last.conclusion.as_str() {
            "hold" => "it found a problem it rates critical and requested changes",
            "too_large" => "the diff is larger than the reviewer accepts, so nothing was posted",
            "publication_rejected" => "the forge refused the review it tried to post",
            "failed" => "the review run ended without a usable verdict",
            _ => continue,
        };
        if !open_pulls.contains(&(last.repo.clone(), last.pr)) {
            continue;
        }
        let mut item = Draft {
            id: format!("reviewer-stuck:{}:{}", last.repo, last.pr),
            kind: "reviewer_stuck",
            severity: Severity::Action,
            title: format!(
                "The automated reviewer stopped on {}#{}",
                last.repo, last.pr
            ),
            reason: format!(
                "{} reviewed this pull request and ended with \"{}\": {explained}. The \
                 automated reviewer will not approve this head, so a person has to review \
                 it or the author has to change it.",
                record.heartbeat.runner_id, last.conclusion
            ),
            href: pull_href(&last.repo, last.pr),
            label: "Review the pull request by hand",
            command: None,
        }
        .build();
        item.since = Some(last.finished_at.to_rfc3339());
        item.repo = Some(last.repo.clone());
        item.pr = Some(last.pr);
        item.sha = Some(last.sha.clone());
        items.push(item);
    }
    let gate_online = runners
        .iter()
        .any(|r| !is_reviewer(&r.heartbeat) && is_online(r, now));
    if !gate_online && (!open_pulls.is_empty() || queue_building) {
        let last_seen = runners
            .iter()
            .filter(|r| !is_reviewer(&r.heartbeat))
            .map(|r| r.received_at)
            .max();
        let mut item = Draft {
            id: "gate-runner-down".to_string(),
            kind: "gate_runner_down",
            severity: Severity::Critical,
            title: "No PR gate runner is reporting".to_string(),
            reason: format!(
                "{} pull request(s) are open{} and no gate runner slot has sent a heartbeat \
                 in the last 3 minutes, so required checks will never be posted and nothing \
                 can merge. The runners are systemd user timers on the gate host.",
                open_pulls.len(),
                if queue_building {
                    " and the merge queue is waiting on a gate"
                } else {
                    ""
                }
            ),
            href: "/runners".to_string(),
            label: "Check the gate runner timers on the gate host",
            command: Some("systemctl --user list-timers 'pr-gate-runner@*'".to_string()),
        }
        .build();
        item.since = last_seen.map(|seen| seen.to_rfc3339());
        items.push(item);
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

/// What production runs for one repository, from the Deployments API.
#[derive(Clone, Debug)]
pub(crate) struct ProductionFacts {
    /// `owner/name`.
    pub repo: String,
    /// The live deployment's sha and creation time.
    pub current: Option<(String, DateTime<Utc>)>,
    /// The newest deployment's state, release name, sha and creation time.
    pub latest: Option<LatestDeployment>,
}

#[derive(Clone, Debug)]
pub(crate) struct LatestDeployment {
    pub state: Option<String>,
    pub release: Option<String>,
    pub sha: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A staged release nobody deployed, a staging that gave up, a failed deploy.
pub(crate) fn release_items(
    staged: Option<&Event>,
    stage_failed: Option<&Event>,
    production: &[ProductionFacts],
) -> Vec<Item> {
    let mut items = Vec::new();
    if let Some(event) = staged {
        let staged_at = parse_time(&event.ts);
        let facts = production
            .iter()
            .find(|facts| Some(&facts.repo) == event.repo.as_ref());
        let live = facts.and_then(|facts| facts.current.as_ref());
        let superseded = live.is_some_and(|(sha, deployed_at)| {
            Some(sha) == event.sha.as_ref() || staged_at.is_some_and(|at| *deployed_at >= at)
        });
        if !superseded {
            let release = event
                .detail
                .as_ref()
                .and_then(|detail| detail["release"].as_str())
                .unwrap_or("a release");
            let command = event
                .detail
                .as_ref()
                .and_then(|detail| detail["deploy_command"].as_str())
                .map(str::to_string);
            let mut item = Draft {
                id: format!("release-staged:{release}"),
                kind: "release_staged",
                severity: Severity::Action,
                title: format!("{release} is staged and waiting for a deploy"),
                reason: format!(
                    "The release was built from a commit whose gate is green and copied to \
                     the production host, but production still runs {}. Deploying is a \
                     deliberate manual step: nothing will switch production until the \
                     deploy command is run.",
                    live.map_or_else(
                        || "an earlier build".to_string(),
                        |(sha, _)| sha.chars().take(10).collect()
                    )
                ),
                href: "/releases".to_string(),
                label: "Deploy the staged release",
                command,
            }
            .build();
            item.since = Some(event.ts.clone());
            item.repo = event.repo.clone();
            item.sha = event.sha.clone();
            items.push(item);
        }
    }
    if let Some(event) = stage_failed.filter(|event| event.needs_human)
        && staged.is_none_or(|staged| staged.seq < event.seq)
    {
        let mut item = Draft {
            id: format!(
                "release-stage-failed:{}",
                event.sha.as_deref().unwrap_or("-")
            ),
            kind: "release_stage_failed",
            severity: Severity::Critical,
            title: "Staging a release failed and was given up".to_string(),
            reason: format!(
                "The auto-stager could not build a release from {} and stopped retrying \
                 that commit, so nothing newer than the last staged release can be \
                 deployed. {} The event's log tail on the Activity page shows where the \
                 build stopped.",
                event.sha.as_deref().unwrap_or("the newest green commit"),
                event.reason.as_deref().unwrap_or("")
            ),
            href: "/activity?kind=release.".to_string(),
            label: "Read the staging log and fix the build",
            command: None,
        }
        .build();
        item.since = Some(event.ts.clone());
        item.repo = event.repo.clone();
        item.sha = event.sha.clone();
        items.push(item);
    }
    for facts in production {
        let Some(latest) = &facts.latest else {
            continue;
        };
        if !matches!(latest.state.as_deref(), Some("failure" | "error")) {
            continue;
        }
        let what = latest.release.clone().unwrap_or_else(|| latest.sha.clone());
        let mut item = Draft {
            id: format!("deploy-failed:{}:{what}", facts.repo),
            kind: "deploy_failed",
            severity: Severity::Critical,
            title: format!("The production deploy of {what} failed"),
            reason: format!(
                "The newest production deployment of {} ended in {}{}. The deploy script \
                 rolls back on failure, so production probably runs the previous release; \
                 confirm what is live before deploying again.",
                facts.repo,
                latest.state.as_deref().unwrap_or("failure"),
                latest
                    .description
                    .as_ref()
                    .map(|text| format!(": {text}"))
                    .unwrap_or_default()
            ),
            href: "/releases".to_string(),
            label: "Check what production runs, then redeploy or roll back",
            command: None,
        }
        .build();
        item.since = Some(latest.created_at.to_rfc3339());
        item.repo = Some(facts.repo.clone());
        item.sha = Some(latest.sha.clone());
        items.push(item);
    }
    items
}

fn open_pull_facts(state: &WebState) -> Vec<PullFacts> {
    let mut pulls = Vec::new();
    for repo in state.core.list_repositories(None) {
        let Ok(listed) = state.core.list_pull_requests(&repo.owner, &repo.name, None) else {
            continue;
        };
        for pr in listed {
            let Some(posture) = super::super::pulls::attention_posture(state, &pr) else {
                continue;
            };
            pulls.push(PullFacts {
                repo: format!("{}/{}", pr.owner, pr.repo),
                number: pr.number,
                title: pr.title.clone(),
                author: pr.author.clone(),
                head_sha: pr.head.sha.clone(),
                updated_at: pr.updated_at,
                posture,
            });
        }
    }
    pulls
}

fn production_facts(state: &WebState) -> Vec<ProductionFacts> {
    let mut all = Vec::new();
    for repo in state.core.list_repositories(None) {
        let Some(production) = state
            .core
            .deployment_environments(&repo.owner, &repo.name)
            .ok()
            .into_iter()
            .flatten()
            .find(|env| env.environment == "production")
        else {
            continue;
        };
        all.push(ProductionFacts {
            repo: repo.full_name.clone(),
            current: production
                .current
                .map(|current| (current.deployment.sha, current.deployment.created_at)),
            latest: production.latest.map(|latest| LatestDeployment {
                state: latest
                    .status
                    .as_ref()
                    .map(|status| status.state.as_str().to_string()),
                description: latest.status.and_then(|status| status.description),
                release: latest
                    .deployment
                    .payload
                    .get("release")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                sha: latest.deployment.sha,
                created_at: latest.deployment.created_at,
            }),
        });
    }
    all
}

/// Everything waiting on a person right now, most urgent first.
pub(crate) fn collect(state: &WebState, now: DateTime<Utc>) -> AttentionResponse {
    let mut items = Vec::new();
    let families: Vec<FamilySnapshot> = super::super::shift::attention_snapshot(state, now);
    let mut waiting = Vec::new();
    for family in &families {
        items.extend(todo_items(&family.name, &family.todos, now));
        items.extend(shift_items(&family.name, &family.shifts));
        waiting.push((
            family.name.clone(),
            family
                .todos
                .iter()
                .filter(|t| t.triaged && matches!(t.status.as_str(), "open" | "claimed"))
                .count(),
        ));
    }
    items.extend(worker_items(
        &waiting,
        &super::super::shift::worker_rows(state, now),
    ));
    let pulls = open_pull_facts(state);
    let open: BTreeSet<(String, u64)> = pulls.iter().map(|p| (p.repo.clone(), p.number)).collect();
    items.extend(pull_items(&pulls, now));
    let entries = state.merge_queue.entries(state, |_| true);
    let building = entries.iter().any(|e| e.state == QueueState::Building);
    items.extend(queue_items(&entries, &open, now));
    items.extend(runner_items(
        &state.gate_runners.snapshot(),
        &open,
        building,
        now,
    ));
    let staged = state.events.newest_of_kind("release.staged").ok().flatten();
    let stage_failed = state
        .events
        .newest_of_kind("release.stage_failed")
        .ok()
        .flatten();
    items.extend(release_items(
        staged.as_ref(),
        stage_failed.as_ref(),
        &production_facts(state),
    ));
    items.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| a.since.cmp(&b.since))
            .then_with(|| a.id.cmp(&b.id))
    });
    let count = |severity| items.iter().filter(|i| i.severity == severity).count();
    AttentionResponse {
        schema_version: ATTENTION_SCHEMA,
        generated_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        counts: Counts {
            critical: count(Severity::Critical),
            action: count(Severity::Action),
            watch: count(Severity::Watch),
        },
        items,
    }
}

/// The last answer, reused for [`CACHE_FOR`]: the inbox badge polls this.
#[derive(Clone, Default)]
pub(crate) struct AttentionCache {
    inner: Arc<Mutex<Option<(Instant, AttentionResponse)>>>,
}

/// `GET /api/v1/attention` (admin-only by path, see `auth::admin_only_request`).
pub(crate) async fn attention(State(state): State<Arc<WebState>>) -> AxumResponse {
    let cached = {
        let cache = state
            .attention
            .inner
            .lock()
            .expect("attention cache mutex poisoned");
        cache
            .as_ref()
            .filter(|(at, _)| at.elapsed() < CACHE_FOR)
            .map(|(_, response)| response.clone())
    };
    if let Some(response) = cached {
        return Json(response).into_response();
    }
    // The collectors run git and read every open pull request: keep them off
    // the async workers.
    let worker_state = state.clone();
    let response = tokio::task::spawn_blocking(move || collect(&worker_state, Utc::now())).await;
    match response {
        Ok(response) => {
            *state
                .attention
                .inner
                .lock()
                .expect("attention cache mutex poisoned") =
                Some((Instant::now(), response.clone()));
            Json(response).into_response()
        }
        Err(error) => {
            use super::super::workcells_support::{TypedError, typed_error};
            let reason = error.to_string();
            typed_error(TypedError {
                status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                code: "attention_collect_failed",
                purpose: "list what in the pipeline is waiting on a person",
                reason: &reason,
                common_fixes: &["retry in a few seconds"],
                docs_url: "docs/pipeline-events.md",
                repair_hint: "retry; if it persists check the server log for a collector panic",
                message: &reason,
            })
        }
    }
}
