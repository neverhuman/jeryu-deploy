//! Attention rules for the flow to production: pull requests, the merge
//! queue, runners and releases.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use super::{
    Draft, Hosts, Item, QUEUE_LOOKBACK_HOURS, QUEUE_STUCK_MINUTES, READY_TO_MERGE_MINUTES,
    Severity, Shell, enqueue_call, parse_time, pull_href, ready_for_review_call, regate_call,
};
use crate::web::control_plane::{GateRunnerRecord, is_online, is_reviewer};
use crate::web::merge_queue::{QueueEntry, QueueState};
use crate::web::pipeline::Event;
use crate::web::pulls::PullPosture;

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

/// An open draft pull request. A draft is outside the merge flow, so the only
/// question about it is how long it has sat without a push.
#[derive(Clone, Debug)]
pub(crate) struct DraftFacts {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub author: String,
    pub base_ref: String,
    pub head_sha: String,
    pub updated_at: DateTime<Utc>,
}

/// Drafts nobody has touched for `idle_days`. A draft says "not yet", which is
/// fine for a day or two and a stranded change after that: no gate runs on it,
/// no automation reviews it, and nothing else in the inbox would ever mention
/// it. Marking it ready is a person's decision, so the item waits on one.
pub(crate) fn draft_items(drafts: &[DraftFacts], idle_days: i64, now: DateTime<Utc>) -> Vec<Item> {
    let mut items = Vec::new();
    for draft in drafts {
        let idle = now - draft.updated_at;
        if idle.num_days() < idle_days {
            continue;
        }
        let label = format!("{}#{}", draft.repo, draft.number);
        let mut item = Draft {
            id: format!("pr-draft-waiting:{}:{}", draft.repo, draft.number),
            kind: "pr_draft_waiting",
            severity: Severity::Action,
            title: format!("{label} is a draft waiting to be marked ready"),
            reason: format!(
                "\"{}\" by {} into {} has been a draft with no push for {} day(s). A draft \
                 does not merge, is not reviewed by an automation and is not queued, so it \
                 waits until somebody marks it ready for review or closes it.",
                draft.title,
                draft.author,
                draft.base_ref,
                idle.num_days()
            ),
            href: pull_href(&draft.repo, draft.number),
            label: "Mark the draft ready for review, or close it",
            // Marking it ready is the step; closing it is the other decision,
            // which the pull request page takes.
            api: Some(ready_for_review_call(&draft.repo, draft.number)),
            command: None,
        }
        .build();
        item.since = Some(draft.updated_at.to_rfc3339());
        item.repo = Some(draft.repo.clone());
        item.pr = Some(draft.number);
        item.sha = Some(draft.head_sha.clone());
        items.push(item);
    }
    items
}

/// Open pull requests waiting on a person: changes requested, red checks,
/// missing approvals, or mergeable and simply not merged.
/// `entries` is the merge queue: a pull request the queue is holding is not
/// waiting on anybody to merge it, however long its gate has been green.
pub(crate) fn pull_items(
    pulls: &[PullFacts],
    entries: &[QueueEntry],
    now: DateTime<Utc>,
) -> Vec<Item> {
    let mut items = Vec::new();
    for pull in pulls {
        let posture = &pull.posture;
        let label = format!("{}#{}", pull.repo, pull.number);
        let building = entries.iter().find(|entry| {
            entry.state == QueueState::Building
                && entry.repo == pull.repo
                && entry.number == pull.number
        });
        if let Some(entry) = building {
            // A queue commit normally gates in a few minutes. Past that the
            // queue is not making progress, which is worth a look even though
            // the step is not a merge.
            let waited = parse_time(&entry.enqueued_at).map_or(0, |at| (now - at).num_minutes());
            if waited >= QUEUE_STUCK_MINUTES {
                let mut item = Draft {
                    id: format!("queue-stuck:{}:{}", pull.repo, pull.number),
                    kind: "queue_stuck",
                    severity: Severity::Watch,
                    title: format!(
                        "{label} has been building in the merge queue for {waited} minutes"
                    ),
                    reason: format!(
                        "The merge queue has been waiting {waited} minutes for the gate on \
                         the queue commit it built for \"{}\" by {} onto {}. That normally \
                         takes a few minutes, so either no runner picked the commit up or \
                         its gate never reported.",
                        pull.title, pull.author, entry.base
                    ),
                    href: pull_href(&pull.repo, pull.number),
                    label: "Check the gate runners, then dequeue and queue the pull request again",
                    api: None,
                    command: None,
                }
                .build();
                item.since = Some(entry.enqueued_at.clone());
                item.repo = Some(pull.repo.clone());
                item.pr = Some(pull.number);
                item.sha = Some(pull.head_sha.clone());
                items.push(item);
            }
        }
        let verdict: Option<(&'static str, String, String, &str)> = if posture.changes_requested > 0
        {
            Some((
                "pr_changes_requested",
                format!("Changes requested on {label}"),
                format!(
                    "A reviewer asked for changes on \"{}\" by {}; the review on the pull \
                     request page says what is wrong. It cannot merge until a fix is pushed \
                     or the request is withdrawn.",
                    pull.title, pull.author
                ),
                "Read the review and push a fix, or dismiss it",
            ))
        } else if !posture.failing.is_empty() {
            Some((
                "pr_checks_failing",
                format!("Checks failing on {label}"),
                format!(
                    "{} failed on \"{}\" by {}, so it cannot merge. The checks panel on the \
                     pull request page links each failing check's details.",
                    posture.failing.join(", "),
                    pull.title,
                    pull.author
                ),
                "Open the failing check and fix or re-run it",
            ))
        } else if posture.checks_green && posture.approvals < posture.required_approvals {
            Some((
                "pr_awaiting_approval",
                format!("{label} is waiting for approval"),
                format!(
                    "{} of {} required approval(s) on \"{}\" by {}; its checks are green. A \
                     reviewer other than the author has to approve the current head.",
                    posture.approvals, posture.required_approvals, pull.title, pull.author
                ),
                "Review and approve the pull request",
            ))
        } else if posture.can_merge
            && building.is_none()
            && (now - pull.updated_at).num_minutes() >= READY_TO_MERGE_MINUTES
        {
            Some((
                "pr_ready_to_merge",
                format!("{label} is ready to merge"),
                format!(
                    "\"{}\" by {} has passed its merge gate for {} minutes (required checks \
                     green, approvals in) and nothing automatic is going to merge it.{}",
                    pull.title,
                    pull.author,
                    (now - pull.updated_at).num_minutes(),
                    if posture.failing_optional.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " {} failed, which the base branch does not require.",
                            posture.failing_optional.join(", ")
                        )
                    }
                ),
                "Queue the pull request to merge it",
            ))
        } else if !posture.failing_optional.is_empty() {
            // Red, but not what the base branch requires: the pull request can
            // still merge, so this is worth a look and waits on nobody.
            Some((
                "pr_checks_failing",
                format!("A check that is not required failed on {label}"),
                format!(
                    "{} failed on \"{}\" by {}. The base branch does not require it, so it \
                     does not block the merge.",
                    posture.failing_optional.join(", "),
                    pull.title,
                    pull.author
                ),
                "Open the failing check",
            ))
        } else {
            None
        };
        if let Some((kind, title, reason, step)) = verdict {
            let blocks = kind != "pr_checks_failing" || !posture.failing.is_empty();
            // A pull request that has passed its gate lands by joining the
            // merge queue; a required check that failed is gated again on the
            // same head, which is what "re-run it" means now that a route does
            // it. A check the base does not require is not the gate's own
            // context, so re-gating would not re-run it and is not offered.
            // Every other posture waits on a review or a push.
            let api = match kind {
                "pr_ready_to_merge" => Some(enqueue_call(&pull.repo, pull.number)),
                "pr_checks_failing" if !posture.failing.is_empty() => {
                    Some(regate_call(&pull.repo, pull.number))
                }
                _ => None,
            };
            let mut item = Draft {
                id: format!("{}:{}:{}", kind.replace('_', "-"), pull.repo, pull.number),
                kind,
                severity: if blocks {
                    Severity::Action
                } else {
                    Severity::Watch
                },
                title,
                reason,
                href: pull_href(&pull.repo, pull.number),
                label: step,
                api,
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

/// Merge-queue entries that failed, were dropped, or were never admitted in
/// the last day while their pull request is still open: an approved PR that
/// will not land alone.
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
        let reason = capitalized(
            entry
                .reason
                .as_deref()
                .unwrap_or("no reason was recorded")
                .trim_end_matches('.'),
        );
        // A refused enqueue never built a queue commit, so queueing it again
        // is refused again: the step is whatever makes the PR replayable.
        // A refused enqueue is replayable only where queueing again is the
        // step: a conflict or a stale head has to be fixed on the branch first.
        let (kind, title, body, step, api) = match entry.refusal_code.as_deref() {
            Some(code) => (
                "queue_refused",
                format!(
                    "{}#{} was refused by the merge queue",
                    entry.repo, entry.number
                ),
                format!(
                    "{reason}. The merge queue could not build a commit for this approved \
                     pull request on top of {} ({code}), so it never joined the queue and \
                     queueing it again changes nothing.",
                    entry.base
                ),
                match code {
                    "queue_conflict" | "queue_merge_commits" => format!(
                        "Open a replacement PR from {base} with this PR's commits \
                         cherry-picked ({base} requires linear history)",
                        base = entry.base
                    ),
                    "queue_mismatch" => "Push a new head".to_string(),
                    _ => "Read what the queue reported, then queue the pull request again"
                        .to_string(),
                },
                match code {
                    "queue_conflict" | "queue_merge_commits" | "queue_mismatch" => None,
                    _ => Some(enqueue_call(&entry.repo, entry.number)),
                },
            ),
            None => (
                "queue_failed",
                format!(
                    "{}#{} fell out of the merge queue",
                    entry.repo, entry.number
                ),
                format!(
                    "{reason}. The merge queue gave up landing this approved pull request \
                     onto {}; it is not retried until somebody queues it again, usually \
                     after a rebase or a fix.",
                    entry.base
                ),
                "Fix what the reason names, then queue the pull request again".to_string(),
                Some(enqueue_call(&entry.repo, entry.number)),
            ),
        };
        let mut item = Draft {
            id: format!("{}:{}:{}", kind.replace('_', "-"), entry.repo, entry.number),
            kind,
            severity: Severity::Action,
            title,
            reason: body,
            href: pull_href(&entry.repo, entry.number),
            label: &step,
            api,
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
    hosts: &Hosts,
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
        let Some(pr) = last
            .pr
            .filter(|pr| open_pulls.contains(&(last.repo.clone(), *pr)))
        else {
            continue;
        };
        let mut item = Draft {
            id: format!("reviewer-stuck:{}:{}", last.repo, pr),
            kind: "reviewer_stuck",
            severity: Severity::Action,
            title: format!("The automated reviewer stopped on {}#{}", last.repo, pr),
            reason: format!(
                "{} ended with \"{}\": {explained}. It will not approve this head, so a \
                 person has to review it or the author has to change it.",
                record.heartbeat.runner_id, last.conclusion
            ),
            href: pull_href(&last.repo, pr),
            label: "Review the pull request by hand",
            api: None,
            command: None,
        }
        .build();
        item.since = Some(last.finished_at.to_rfc3339());
        item.repo = Some(last.repo.clone());
        item.pr = Some(pr);
        item.sha = Some(last.sha.clone());
        items.push(item);
    }
    // A background timer (auto-pin, auto-stage) beating is not a gate slot.
    let gate_slot =
        |r: &&GateRunnerRecord| crate::web::control_plane::holds_gate_slot(&r.heartbeat);
    let gate_online = runners.iter().filter(gate_slot).any(|r| is_online(r, now));
    if !gate_online && (!open_pulls.is_empty() || queue_building) {
        let last_seen = runners
            .iter()
            .filter(gate_slot)
            .map(|r| r.received_at)
            .max();
        let mut item = Draft {
            id: "gate-runner-down".to_string(),
            kind: "gate_runner_down",
            severity: Severity::Critical,
            title: "No PR gate runner is reporting".to_string(),
            reason: format!(
                "{} pull request(s) are open{} and no gate runner slot has sent a heartbeat \
                 in the last 3 minutes, so no required check is posted and nothing can \
                 merge. The runners are systemd user timers on the gate host.",
                open_pulls.len(),
                if queue_building {
                    " and the merge queue is waiting on a gate"
                } else {
                    ""
                }
            ),
            href: "/runners".to_string(),
            label: "Check the gate runner timers on the gate host",
            api: None,
            command: Some(Shell {
                line: "systemctl --user list-timers 'pr-gate-runner@*'".to_string(),
                run_in: Hosts::anywhere(&hosts.gate),
            }),
        }
        .build();
        item.since = last_seen.map(|seen| seen.to_rfc3339());
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
    /// The live deployment's release name, when its payload names one.
    pub current_release: Option<String>,
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

/// " The last deploy of `release` failed: <why>." when the newest production
/// deployment is a failed attempt at this release, else empty. The deploy
/// script puts the log's last meaningful line in the status description.
fn failed_attempt(facts: Option<&ProductionFacts>, release: &str) -> String {
    facts
        .and_then(|facts| facts.latest.as_ref())
        .filter(|latest| {
            matches!(latest.state.as_deref(), Some("failure" | "error"))
                && latest.release.as_deref() == Some(release)
        })
        .map(|latest| {
            let why = latest
                .description
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .unwrap_or("no reason recorded");
            format!(
                " The last deploy of it failed: {}.",
                why.trim_end_matches('.')
            )
        })
        .unwrap_or_default()
}

/// Whether the failed attempt left production as it was: the live deployment
/// is a successful one of the very release the attempt tried to deploy, as when
/// the release that is already live is deployed a second time. Release names
/// decide it when both are known, else the commits do.
fn left_production_alone(facts: &ProductionFacts, latest: &LatestDeployment) -> bool {
    let Some((live_sha, _)) = facts.current.as_ref() else {
        return false;
    };
    match (facts.current_release.as_deref(), latest.release.as_deref()) {
        (Some(live), Some(failed)) => live == failed,
        _ => live_sha == &latest.sha,
    }
}

/// A staged release nobody deployed, a staging that gave up, a failed deploy.
///
/// `staged` and `stage_failed` are the newest such event of every repository,
/// not one newest event over all of them: a release staged in one repository
/// must not hide one staged in another, so each repository is asked on its own
/// and brings its own item. An event that names no repository is its own scope.
pub(crate) fn release_items(
    staged: &[Event],
    stage_failed: &[Event],
    production: &[ProductionFacts],
    hosts: &Hosts,
) -> Vec<Item> {
    let mut scopes: Vec<Option<&str>> = Vec::new();
    for event in staged.iter().chain(stage_failed) {
        let scope = event.repo.as_deref();
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    let mut items = Vec::new();
    for scope in scopes {
        items.extend(staging_items(
            newest_in(staged, scope),
            newest_in(stage_failed, scope),
            production,
            hosts,
        ));
    }
    items.extend(deploy_failure_items(production));
    items
}

/// The newest event of one repository scope, by sequence.
fn newest_in<'a>(events: &'a [Event], scope: Option<&str>) -> Option<&'a Event> {
    events
        .iter()
        .filter(|event| event.repo.as_deref() == scope)
        .max_by_key(|event| event.seq)
}

/// What one repository's staging says: a release waiting for a deploy, or a
/// staging that gave up on it.
fn staging_items(
    staged: Option<&Event>,
    stage_failed: Option<&Event>,
    production: &[ProductionFacts],
    hosts: &Hosts,
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
                .map(|line| Shell {
                    line: line.to_string(),
                    // The command is a path inside the repository that staged
                    // the release, which the event names.
                    run_in: match event.repo.as_deref() {
                        Some(repo) => Hosts::checkout(&hosts.release, repo),
                        None => format!(
                            "{}, in a checkout of the repository that staged it",
                            hosts.release
                        ),
                    },
                });
            let mut item = Draft {
                // The release name alone is not an identity across
                // repositories, and the id is what an acknowledgement hides.
                id: match event.repo.as_deref() {
                    Some(repo) => format!("release-staged:{repo}:{release}"),
                    None => format!("release-staged:{release}"),
                },
                kind: "release_staged",
                severity: Severity::Action,
                title: format!("{release} is staged and waiting for a deploy"),
                reason: format!(
                    "Production still runs {}. This release is built from a commit whose \
                     gate is green and is already on the production host; nothing switches \
                     until the deploy command is run.{}",
                    live.map_or_else(
                        || "an earlier build".to_string(),
                        |(sha, _)| sha.chars().take(10).collect()
                    ),
                    failed_attempt(facts, release)
                ),
                href: "/releases".to_string(),
                label: "Deploy the staged release",
                api: None,
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
                "{}The auto-stager stopped retrying {}, so nothing newer than the last \
                 staged release can be deployed. The event's log tail on the Activity page \
                 shows where the build stopped.",
                event
                    .reason
                    .as_deref()
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                    .map(|reason| format!("{}. ", reason.trim_end_matches('.')))
                    .unwrap_or_default(),
                event.sha.as_deref().unwrap_or("the newest green commit")
            ),
            href: "/activity?kind=release.".to_string(),
            label: "Read the staging log and fix the build",
            api: None,
            command: None,
        }
        .build();
        item.since = Some(event.ts.clone());
        item.repo = event.repo.clone();
        item.sha = event.sha.clone();
        items.push(item);
    }
    items
}

/// A production deploy that ended in a failure, one item per repository.
fn deploy_failure_items(production: &[ProductionFacts]) -> Vec<Item> {
    let mut items = Vec::new();
    for facts in production {
        let Some(latest) = &facts.latest else {
            continue;
        };
        if !matches!(latest.state.as_deref(), Some("failure" | "error")) {
            continue;
        }
        let what = latest.release.clone().unwrap_or_else(|| latest.sha.clone());
        let why = latest
            .description
            .as_ref()
            .map(|text| format!(": {text}"))
            .unwrap_or_default();
        let state = latest.state.as_deref().unwrap_or("failure");
        let draft = if left_production_alone(facts, latest) {
            Draft {
                id: format!("deploy-failed:{}:{what}", facts.repo),
                kind: "deploy_failed",
                severity: Severity::Watch,
                title: format!("A deploy of {what} failed, and production still runs it"),
                reason: format!(
                    "The newest production deployment of {} ended in {}{}, but it is the \
                     release production already runs and the live deployment of it \
                     succeeded: nothing changed. Read the status log if the attempt is a \
                     surprise; production needs no deploy.",
                    facts.repo, state, why
                ),
                href: "/releases".to_string(),
                label: "Read the failed attempt; production is unchanged",
                api: None,
                command: None,
            }
        } else {
            Draft {
                id: format!("deploy-failed:{}:{what}", facts.repo),
                kind: "deploy_failed",
                severity: Severity::Critical,
                title: format!("The production deploy of {what} failed"),
                reason: format!(
                    "The newest production deployment of {} ended in {}{}. The deploy script \
                     rolls back on failure, so production probably runs the previous release: \
                     confirm what is live before deploying again.",
                    facts.repo, state, why
                ),
                href: "/releases".to_string(),
                label: "Check what production runs, then redeploy or roll back",
                api: None,
                command: None,
            }
        };
        let mut item = draft.build();
        item.since = Some(latest.created_at.to_rfc3339());
        item.repo = Some(facts.repo.clone());
        item.sha = Some(latest.sha.clone());
        items.push(item);
    }
    items
}

/// A reason that opens a sentence starts with a capital.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
