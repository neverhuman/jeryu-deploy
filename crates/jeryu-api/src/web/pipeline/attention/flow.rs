//! Attention rules for the flow to production: pull requests, the merge
//! queue, runners and releases.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use super::{
    Draft, Item, QUEUE_LOOKBACK_HOURS, READY_TO_MERGE_MINUTES, Severity, parse_time, pull_href,
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
