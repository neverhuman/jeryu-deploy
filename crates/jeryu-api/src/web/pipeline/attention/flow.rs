//! Attention rules for the flow to production: pull requests, the merge
//! queue, runners and releases.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use super::{
    BRANCH_CAMPAIGN_PRS, Draft, Hosts, Item, MERGER_STALLED_MINUTES, PullLink,
    QUEUE_LOOKBACK_HOURS, QUEUE_STUCK_MINUTES, READY_TO_MERGE_MINUTES, Severity, Shell,
    close_landed_pull_call, close_pull_call, enqueue_call, parse_time, pull_href,
    ready_for_review_call, regate_call, web_pin_bump_prs,
};
use crate::web::control_plane::{GateRunnerRecord, is_online, is_reviewer};
use crate::web::merge_queue::{QueueEntry, QueueState};
use crate::web::pipeline::Event;
use crate::web::pulls::PullPosture;
use crate::web::pulls::landed::Landed;

/// The verdicts that leave a review waiting on a person. Shared with Flow.
pub(crate) fn review_hold_explanation(verdict: &str) -> Option<&'static str> {
    match verdict {
        "hold" | "request_changes" => {
            Some("it found a problem serious enough to hold the merge and requested changes")
        }
        "too_large" => Some("the diff is larger than the reviewer accepts, so nothing was posted"),
        "publication_rejected" => Some("the forge refused the review it tried to post"),
        "failed" => Some("the review run ended without a usable verdict"),
        _ => None,
    }
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
    /// The branch the pull request merges from, which is what a campaign of
    /// pull requests shares; see [`fold_branch_campaigns`].
    pub head_ref: String,
    /// The branch it merges into.
    pub base_ref: String,
    pub updated_at: DateTime<Utc>,
    pub posture: PullPosture,
    /// Every path `base...head` touches, for [`gate_definition_paths`].
    pub changed_paths: Vec<String>,
    /// Set when the change is already on the base branch, so the only step
    /// left is to close the pull request ([`landed_item`]).
    pub landed: Option<Landed>,
    /// When the oldest changes-requested review on the current head was
    /// submitted: how long the hold has stood. `None` when none stands.
    pub changes_requested_since: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub(crate) struct SupersededPullFacts {
    pub repo: String,
    pub author: String,
    pub base_ref: String,
    pub source_repository: String,
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub head_ref: String,
    pub created_at: DateTime<Utc>,
    pub status: &'static str,
    /// The replacement a close-as-replaced recorded for this pull request
    /// (`github/pull_replacements.rs`). Set, it is the answer; the branch and
    /// text rules below are only for a replacement nobody has recorded yet.
    pub recorded_replacement: Option<u64>,
}

/// An open pull request and the later pull request that replaced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Superseded {
    pub repo: String,
    pub number: u64,
    pub replacement_repo: String,
    pub replacement_number: u64,
    pub replacement_state: &'static str,
    pub replaced_at: DateTime<Utc>,
}

/// Each older open pull request replaced by a later pull: the one rule
/// behind `pr_superseded` rows and Flow's `superseded` holds.
pub(crate) fn superseded_pulls(pulls: &[SupersededPullFacts]) -> Vec<(usize, Superseded)> {
    pulls
        .iter()
        .enumerate()
        .filter(|(_, old)| old.status == "open")
        .filter_map(|(index, old)| {
            // A recorded link is the record itself: it is read, not re-derived.
            if let Some(number) = old.recorded_replacement
                && let Some(replacement) = pulls
                    .iter()
                    .find(|new| new.repo == old.repo && new.number == number)
            {
                return Some((index, found(old, replacement)));
            }
            let replacement = pulls
                .iter()
                .filter(|new| {
                    new.repo == old.repo
                        && !old.author.trim().is_empty()
                        && new.author.eq_ignore_ascii_case(&old.author)
                        && new.base_ref == old.base_ref
                        && source_repository(new) == source_repository(old)
                        && new.status != "closed"
                        && new.created_at > old.created_at
                        && (explicitly_replaces(new, old.number)
                            || new
                                .head_ref
                                .strip_prefix(&old.head_ref)
                                .is_some_and(|suffix| {
                                    suffix.starts_with("-r") && suffix[2..].parse::<u32>().is_ok()
                                }))
                })
                .max_by_key(|new| new.created_at)?;
            Some((index, found(old, replacement)))
        })
        .collect()
}

/// One replaced pull request and the pull request that replaced it.
fn found(old: &SupersededPullFacts, replacement: &SupersededPullFacts) -> Superseded {
    Superseded {
        repo: old.repo.clone(),
        number: old.number,
        replacement_repo: replacement.repo.clone(),
        replacement_number: replacement.number,
        replacement_state: match replacement.status {
            "merged" => "merged",
            "closed" => "closed",
            _ => "open",
        },
        replaced_at: replacement.created_at,
    }
}

/// One action row for each older open pull request replaced by a later pull.
pub(crate) fn superseded_pull_items(pulls: &[SupersededPullFacts]) -> Vec<Item> {
    superseded_pulls(pulls).into_iter().map(|(index, found)| {
        let old = &pulls[index];
        let replacement = pulls.iter().find(|new| new.repo == found.replacement_repo && new.number == found.replacement_number).unwrap_or(old);
        let label = format!("{}#{}", old.repo, old.number);
        let mut item = Draft { id: format!("pr-superseded:{}:{}", old.repo, old.number), kind: "pr_superseded", severity: Severity::Action,
            title: format!("{label} was replaced by {}#{}", found.replacement_repo, found.replacement_number),
            reason: format!("\"{}\" was superseded by \"{}\" ({}). Close the older pull request and leave a comment pointing to its replacement.", old.title, replacement.title, found.replacement_state),
            href: pull_href(&old.repo, old.number), label: "Close it and point to the replacement", api: Some(close_pull_call(&old.repo, old.number, found.replacement_number)), command: None }.build();
        item.repo = Some(old.repo.clone()); item.pr = Some(old.number); item
    }).collect()
}

fn source_repository(pr: &SupersededPullFacts) -> &str {
    if pr.source_repository.trim().is_empty() {
        &pr.repo
    } else {
        &pr.source_repository
    }
}

fn explicitly_replaces(pr: &SupersededPullFacts, number: u64) -> bool {
    let text = format!("{} {}", pr.title, pr.body.as_deref().unwrap_or("")).to_ascii_lowercase();
    ["replaces", "supersedes"].iter().any(|verb| {
        let needle = format!("{verb} #{number}");
        text.match_indices(&needle).any(|(start, _)| {
            text[start + needle.len()..]
                .chars()
                .next()
                .is_none_or(|next| !next.is_ascii_digit())
        })
    })
}

#[derive(Clone, Debug)]
pub(crate) struct MainRedFacts {
    /// `owner/name`.
    pub repo: String,
    pub sha: String,
    pub since: String,
    pub first_step: String,
    pub log_id: String,
}

/// How many gate-defining paths one item names before it counts the rest.
const GATE_PATHS_NAMED: usize = 6;

/// The paths of a diff that define the gate the pull request is judged by. The
/// gate runner runs the recipe at the pull request's own head
/// (jeryu-ci-runner `ops/pr-gate`), so a diff touching one of these rewrote the
/// check that then reports on it.
///
/// The set is narrow on purpose (owner decision, 2026-10-02): the recipe entry
/// point, the CI scripts it calls, and the jankurai and policy files those
/// read. Ordinary test directories are left out, because nearly every pull
/// request touches tests and a flag that fires on all of them says nothing;
/// so is documentation about the gate, which is not the gate. It is the same
/// set as `gate_definition_paths` in jeryu-ci-runner's `pr-gate-state.sh`,
/// which annotates the check run from it.
pub(crate) fn gate_definition_paths(changed: &[String]) -> Vec<String> {
    let mut paths: Vec<String> = changed
        .iter()
        .filter(|path| defines_the_gate(path))
        .cloned()
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn defines_the_gate(path: &str) -> bool {
    if path.starts_with("docs/") {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "Justfile"
        || path.starts_with("ops/ci/")
        || path.starts_with("ops/jankurai/")
        || path.starts_with("policies/")
        || name.to_ascii_lowercase().contains("jankurai")
}

/// `a, b and 3 more`: enough of the list to recognise the change, and a count
/// for the rest, so a diff that rewrites the whole gate stays one sentence.
fn named_paths(paths: &[String]) -> String {
    let shown = paths
        .iter()
        .take(GATE_PATHS_NAMED)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    match paths.len().saturating_sub(GATE_PATHS_NAMED) {
        0 => shown,
        rest => format!("{shown} and {rest} more"),
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReviewerOfflineThresholds {
    pub(crate) heartbeat_minutes: i64,
    pub(crate) pass_minutes: i64,
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

/// The one posture a pull request is listed for: kind, title, reason and the
/// label of its step. The step is a `String` because a failing gate names the
/// cause the runner published in it.
type Verdict = Option<(&'static str, String, String, String)>;

/// Where the attempt that failed left its evidence: the gate runner keeps one
/// directory per attempt under its home on the gate host
/// (`gate_lock`/`gate_begin` in jeryu-ci-runner's `pr-gate-state.sh`, keyed by
/// `<owner>-<repo>-<sha>`). The directory is mode 0700, so this names the path
/// rather than linking it: whoever can read it knows where to look, and the
/// inbox repeats no line of the log.
fn gate_evidence(attempt: Option<&str>, pull: &PullFacts, hosts: &Hosts) -> String {
    match attempt {
        Some(attempt) => format!(
            " Attempt {attempt}: its log is \
             `~/gate-runner/attempts/{}-{}/{attempt}/build.log` on {}, readable by the \
             runner's own user.",
            pull.repo.replace('/', "-"),
            pull.head_sha,
            hosts.gate
        ),
        None => String::new(),
    }
}

/// An open pull request whose change is already on its base branch. The row
/// carries the call that closes it: the change has shipped, so closing it is
/// one safe step rather than a judgement, and the close comments the commit
/// the work landed as before it happens, so the reason outlives the pull
/// request. The branch is left alone.
pub(crate) fn landed_item(pull: &PullFacts, landed: Landed) -> Item {
    let label = format!("{}#{}", pull.repo, pull.number);
    let how = match landed {
        Landed::Ancestor => format!(
            "its head commit {} is already in the history of {}",
            short_sha(&pull.head_sha),
            pull.base_ref
        ),
        Landed::Content => format!(
            "merging it into {} would change nothing: every change it makes is already \
             there, so it landed by a squash, a cherry-pick or a replacement pull request",
            pull.base_ref
        ),
    };
    let mut item = Draft {
        id: format!("pr-already-landed:{}:{}", pull.repo, pull.number),
        kind: "pr_already_landed",
        severity: Severity::Action,
        title: format!("{label} is already on {}", pull.base_ref),
        reason: format!(
            "\"{}\" by {} is still open, but {how}. Its checks and reviews no longer \
             matter; it only clutters the flow until somebody closes it.",
            pull.title, pull.author
        ),
        href: pull_href(&pull.repo, pull.number),
        label: "Close the pull request: its change is already on the base branch",
        api: Some(close_landed_pull_call(&pull.repo, pull.number)),
        command: None,
    }
    .build();
    item.since = Some(pull.updated_at.to_rfc3339());
    item.repo = Some(pull.repo.clone());
    item.pr = Some(pull.number);
    item.sha = Some(pull.head_sha.clone());
    item
}

fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Open pull requests waiting on a person: changes requested, red checks,
/// missing approvals, or mergeable and simply not merged.
/// `entries` is the merge queue: a pull request the queue is holding is not
/// waiting on anybody to merge it, however long its gate has been green.
pub(crate) fn pull_items(
    pulls: &[PullFacts],
    entries: &[QueueEntry],
    now: DateTime<Utc>,
    hosts: &Hosts,
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
        // A change to the gate's own definition waits on a person whatever the
        // merge posture says, so it is its own row rather than one branch of
        // the verdict below: a green gate is exactly the case worth seeing.
        let gate_paths = gate_definition_paths(&pull.changed_paths);
        if !gate_paths.is_empty() {
            let mut item = Draft {
                id: format!("pr-changes-own-gate:{}:{}", pull.repo, pull.number),
                kind: "pr_changes_own_gate",
                severity: Severity::Action,
                title: format!("{label} changes the gate that judges it"),
                reason: format!(
                    "\"{}\" by {} changes {}. The gate runs the recipe at the pull request's \
                     own head, so this diff rewrote the check that reports on it: a green \
                     gate here is the change proving itself. The `pr-gate-runner` check run \
                     on the pull request says the same. Read the gate change against the base \
                     branch before approving or merging it.",
                    pull.title,
                    pull.author,
                    named_paths(&gate_paths)
                ),
                href: pull_href(&pull.repo, pull.number),
                label: "Read the gate change against the base branch, then approve it or ask for changes",
                api: None,
                command: None,
            }
            .build();
            item.since = Some(pull.updated_at.to_rfc3339());
            item.repo = Some(pull.repo.clone());
            item.pr = Some(pull.number);
            item.sha = Some(pull.head_sha.clone());
            items.push(item);
        }
        // Gate-definition changes stay visible even when the content landed.
        if let Some(landed) = pull.landed {
            items.push(landed_item(pull, landed));
            continue;
        }
        let verdict: Verdict = if posture.changes_requested > 0 {
            Some((
                "pr_changes_requested",
                format!("Changes requested on {label}"),
                format!(
                    "A reviewer asked for changes on \"{}\" by {}; the review on the pull \
                     request page says what is wrong. It cannot merge until a fix is pushed \
                     or the request is withdrawn.",
                    pull.title, pull.author
                ),
                "Read the review and push a fix, or dismiss it".to_string(),
            ))
        } else if !posture.failing.is_empty() {
            // The gate runner's own check run has no details_url and its log
            // is unreadable off the gate host, so "open the failing check"
            // sent a reader nowhere. When the runner published a cause, that
            // cause and the attempt holding the log are the next step.
            let (cause, step) = match &posture.gate_failure {
                Some(failure) => (
                    format!(
                        " The `pr-gate-runner` check run on this head says: {}.{}",
                        failure.title.trim_end_matches('.'),
                        gate_evidence(failure.attempt.as_deref(), pull, hosts)
                    ),
                    format!("Fix {}, then re-run the gate", failure.cause()),
                ),
                None => (
                    " The checks panel on the pull request page links each failing check's \
                     details."
                        .to_string(),
                    "Open the failing check and fix or re-run it".to_string(),
                ),
            };
            let main_red = posture
                .main_failure
                .as_ref()
                .map_or_else(String::new, |failure| {
                    format!(
                        " current main is red too: {}. Log {}.",
                        failure.title.trim_end_matches('.'),
                        failure.attempt.as_deref().unwrap_or("unknown")
                    )
                });
            Some((
                "pr_checks_failing",
                format!("Checks failing on {label}"),
                format!(
                    "{} failed on \"{}\" by {}, so it cannot merge.{cause}{main_red}",
                    posture.failing.join(", "),
                    pull.title,
                    pull.author
                ),
                step,
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
                "Review and approve the pull request".to_string(),
            ))
        } else if posture.can_merge
            && building.is_none()
            && !queue_refused_for_head(entries, pull, now)
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
                "Queue the pull request to merge it".to_string(),
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
                "Open the failing check".to_string(),
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
                label: &step,
                api,
                command: None,
            }
            .build();
            item.since = Some(
                match (kind, pull.changes_requested_since) {
                    ("pr_changes_requested", Some(held)) => held,
                    _ => pull.updated_at,
                }
                .to_rfc3339(),
            );
            item.repo = Some(pull.repo.clone());
            item.pr = Some(pull.number);
            item.sha = Some(pull.head_sha.clone());
            items.push(item);
        }
    }
    fold_merger_stalled(&mut items, now);
    fold_branch_campaigns(&mut items, pulls);
    items
}

/// What every pull request of a campaign is waiting on, as a phrase for the
/// folded row's title and reason.
fn campaign_verdict(kind: &str) -> &'static str {
    match kind {
        "pr_changes_requested" => "blocked by a review asking for changes",
        "pr_checks_failing" => "failing a check",
        "pr_awaiting_approval" => "waiting for approval",
        "pr_ready_to_merge" => "ready to merge",
        "pr_changes_own_gate" => "changing the gate that judges it",
        "queue_stuck" => "stuck building in the merge queue",
        _ => "waiting on the same thing",
    }
}

/// Replace the rows of one campaign — [`BRANCH_CAMPAIGN_PRS`] or more open
/// pull requests that merge from the same branch and got the same verdict,
/// whatever repositories they are in — with one row per branch and verdict.
///
/// A campaign is one change pushed to one branch in many repositories, so the
/// same review on each of them is one decision for the owner: answer the
/// branch, and every pull request on it follows. As a row apiece it reads like
/// that many decisions and buries everything else in the inbox (live on
/// 2026-10-06: about twenty `pr_changes_requested` rows for
/// `dev/jankurai-v3-receipts`). A single pull request, or two, keeps its own
/// row: there is nothing to collapse.
fn fold_branch_campaigns(items: &mut Vec<Item>, pulls: &[PullFacts]) {
    let branch_of: BTreeMap<(&str, u64), &str> = pulls
        .iter()
        .map(|pull| ((pull.repo.as_str(), pull.number), pull.head_ref.as_str()))
        .collect();
    let mut groups: BTreeMap<(&str, &'static str), Vec<usize>> = BTreeMap::new();
    for (index, item) in items.iter().enumerate() {
        let Some((repo, number)) = item.repo.as_deref().zip(item.pr) else {
            continue;
        };
        let Some(branch) = branch_of.get(&(repo, number)) else {
            continue;
        };
        groups.entry((branch, item.kind)).or_default().push(index);
    }
    let mut folded: BTreeSet<usize> = BTreeSet::new();
    let mut rows: Vec<Item> = Vec::new();
    for ((branch, kind), members) in groups {
        if members.len() < BRANCH_CAMPAIGN_PRS {
            continue;
        }
        rows.push(campaign_item(
            branch,
            kind,
            &members
                .iter()
                .map(|index| &items[*index])
                .collect::<Vec<_>>(),
        ));
        folded.extend(members);
    }
    if rows.is_empty() {
        return;
    }
    let mut index = 0;
    items.retain(|_| {
        let keep = !folded.contains(&index);
        index += 1;
        keep
    });
    items.extend(rows);
}

/// The one row a campaign's rows fold into. Every pull request it stands for
/// is in `pulls` as well as in the reason, so a reader opens each one without
/// reading prose, and `href` is the search for the branch: the one page that
/// lists exactly these pull requests across repositories.
fn campaign_item(branch: &str, kind: &'static str, members: &[&Item]) -> Item {
    let mut links: Vec<PullLink> = members
        .iter()
        .filter_map(|item| {
            let (repo, number) = item.repo.clone().zip(item.pr)?;
            Some(PullLink {
                href: pull_href(&repo, number),
                repo,
                number,
            })
        })
        .collect();
    links.sort_by(|a, b| a.repo.cmp(&b.repo).then(a.number.cmp(&b.number)));
    let repos: BTreeSet<&str> = links.iter().map(|link| link.repo.as_str()).collect();
    let listed = links
        .iter()
        .map(|link| format!("{}#{}", link.repo, link.number))
        .collect::<Vec<_>>()
        .join(", ");
    let verdict = campaign_verdict(kind);
    // `Severity` orders the most severe first, so the whole campaign is as
    // severe as its worst row: folding never makes the inbox quieter.
    let severity = members
        .iter()
        .map(|item| item.severity)
        .min()
        .unwrap_or(Severity::Action);
    // Every row of a campaign asks for the same step, so the first one's is
    // the campaign's; the folded row keeps no `api`, because one call would
    // only perform the step on one of them.
    let label = members
        .first()
        .map(|item| item.action.label.clone())
        .unwrap_or_default();
    let mut item = Draft {
        id: format!("pr-branch-campaign:{}:{branch}", kind.replace('_', "-")),
        kind: "pr_branch_campaign",
        severity,
        title: format!("{} pull requests on {branch} are {verdict}", links.len()),
        reason: format!(
            "{} open pull requests across {} repositories merge from the branch {branch} \
             and every one of them is {verdict}. That is one branch answered once, not \
             {} separate decisions: {listed}.",
            links.len(),
            repos.len(),
            links.len()
        ),
        href: format!("/search?q={branch}"),
        label: &label,
        api: None,
        command: None,
    }
    .build();
    // The oldest row's time: how long the campaign has been waiting.
    item.since = members.iter().filter_map(|row| row.since.clone()).min();
    item.pulls = Some(links);
    item
}

/// How long one `pr_ready_to_merge` row has been ready, from the pull request
/// time the rule put in `since`.
fn ready_minutes(item: &Item, now: DateTime<Utc>) -> Option<i64> {
    item.since
        .as_deref()
        .and_then(parse_time)
        .map(|at| (now - at).num_minutes())
}

/// Replace the long-ready `pr_ready_to_merge` rows with one critical row when
/// there are two or more of them. The reviewer merges approved pull requests
/// every five minutes, so several sitting ready for half an hour is one
/// failure in the reviewer, not a queue of owner decisions — and as separate
/// `action` rows it reads like routine work (live on 2026-10-06: the reviewer
/// merged nothing for a day and the inbox showed 26 ordinary rows).
fn fold_merger_stalled(items: &mut Vec<Item>, now: DateTime<Utc>) {
    let mut stalled: Vec<(i64, String)> = items
        .iter()
        .filter(|item| item.kind == "pr_ready_to_merge")
        .filter_map(|item| {
            let minutes = ready_minutes(item, now)?;
            let repo = item.repo.as_deref()?;
            let number = item.pr?;
            (minutes >= MERGER_STALLED_MINUTES).then(|| (minutes, format!("{repo}#{number}")))
        })
        .collect();
    if stalled.len() < 2 {
        return;
    }
    // Oldest first: the one that says how long merging has been stopped.
    stalled.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let oldest = stalled[0].0;
    let shown = stalled
        .iter()
        .take(6)
        .map(|(_, label)| label.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let more = stalled.len().saturating_sub(6);
    items.retain(|item| {
        item.kind != "pr_ready_to_merge"
            || ready_minutes(item, now).is_none_or(|minutes| minutes < MERGER_STALLED_MINUTES)
    });
    let mut item = Draft {
        id: "merger-stalled".to_string(),
        kind: "merger_stalled",
        severity: Severity::Critical,
        title: format!(
            "{} pull requests have been ready to merge for up to {oldest} minutes",
            stalled.len()
        ),
        reason: format!(
            "{} pull requests have passed their merge gate and nothing merged them; the              oldest has been ready for {oldest} minutes. The automated reviewer merges              approved work on every pass, so this is the reviewer having stopped rather              than {} owner decisions. Waiting: {shown}{}. What it last said:              `journalctl --user -u pr-redteam | grep \"waiting to merge\"`.",
            stalled.len(),
            stalled.len(),
            if more == 0 {
                String::new()
            } else {
                format!(" and {more} more")
            },
        ),
        href: "/repos".to_string(),
        label: "Read what the reviewer logged, then merge the pull requests by hand",
        api: None,
        command: None,
    }
    .build();
    item.since = Some((now - chrono::Duration::minutes(oldest)).to_rfc3339());
    items.push(item);
}

/// A `reviewer_stuck` row ages from the oldest changes-requested review on the
/// head it names, like the `pr_changes_requested` row it shadows: the hold's
/// age, not when the reviewer's run ended. A row for a head that has since
/// moved on, or a pull request with no such review, keeps its own time.
pub(crate) fn stamp_hold_since(items: &mut [Item], pulls: &[PullFacts]) {
    for item in items
        .iter_mut()
        .filter(|item| item.kind == "reviewer_stuck")
    {
        let held = pulls
            .iter()
            .find(|pull| {
                item.repo.as_deref() == Some(pull.repo.as_str())
                    && item.pr == Some(pull.number)
                    && item.sha.as_deref() == Some(pull.head_sha.as_str())
            })
            .and_then(|pull| pull.changes_requested_since);
        if let Some(held) = held {
            item.since = Some(held.to_rfc3339());
        }
    }
}

/// A pull request the automated reviewer gave up on, where it requested the
/// changes it gave up over: `reviewer_stuck` names the reviewer, the verdict
/// and the review, so the `pr_changes_requested` row for the same pull
/// request would only repeat it one line further down.
pub(crate) fn drop_shadowed_pull_rows(items: &mut Vec<Item>) {
    let mut stuck: BTreeSet<(Option<String>, Option<u64>)> = items
        .iter()
        .filter(|item| item.kind == "reviewer_stuck")
        .map(|item| (item.repo.clone(), item.pr))
        .collect();
    stuck.extend(web_pin_bump_prs(items));
    items.retain(|item| {
        item.kind != "pr_changes_requested" || !stuck.contains(&(item.repo.clone(), item.pr))
    });
}

/// The merge queue refused this pull request's current head and
/// `queue_items` still shows it: that row owns the pull request, so it is not
/// also ready to merge (and cannot count toward a stalled merger).
fn queue_refused_for_head(entries: &[QueueEntry], pull: &PullFacts, now: DateTime<Utc>) -> bool {
    entries.iter().any(|entry| {
        matches!(entry.state, QueueState::Failed | QueueState::Dequeued)
            && entry.refusal_code.is_some()
            && entry.repo == pull.repo
            && entry.number == pull.number
            && entry.pr_head_sha == pull.head_sha
            && parse_time(
                entry
                    .attempts
                    .last()
                    .map_or(entry.enqueued_at.as_str(), |attempt| attempt.at.as_str()),
            )
            .is_some_and(|at| (now - at).num_hours() < QUEUE_LOOKBACK_HOURS)
    })
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
                if code == "queue_conflict_queued" {
                    format!(
                        "{reason}. The merge queue could not stack this approved pull request \
                         on the one ahead of it on {} ({code}); its branch is fine.",
                        entry.base
                    )
                } else {
                    format!(
                        "{reason}. The merge queue could not build a commit for this approved \
                         pull request on top of {} ({code}), so it never joined the queue and \
                         queueing it again changes nothing.",
                        entry.base
                    )
                },
                match code {
                    "queue_conflict_queued" => {
                        "Queue it again once the pull request ahead of it lands or leaves the queue"
                            .to_string()
                    }
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

pub(crate) fn main_red_items(facts: &[MainRedFacts]) -> Vec<Item> {
    facts
        .iter()
        .map(|fact| {
            let mut item = Draft {
                id: format!("main-red:{}", fact.repo),
                kind: "main_red",
                severity: Severity::Critical,
                title: format!("{} main is red", fact.repo),
                reason: format!(
                    "{} main at {} is failing main-health since {}. First failing step: {}. \
                     Log id: {}.",
                    fact.repo, fact.sha, fact.since, fact.first_step, fact.log_id
                ),
                href: super::repo_href(&fact.repo),
                label: "Fix main before merging PRs into it",
                api: None,
                command: None,
            }
            .build();
            item.since = Some(fact.since.clone());
            item.repo = Some(fact.repo.clone());
            item.sha = Some(fact.sha.clone());
            item
        })
        .collect()
}

/// The informational row that replaces a hold whose head is gone: the
/// reviewer is reading the pull request's current head right now, so the
/// stale verdict is history rather than a thing to do.
///
/// `Watch`, never `Action`: nobody is being asked for anything. Returns
/// `None` when no reviewer reports a live review of this pull request at
/// `head` — then there is nothing to say, and the row is simply dropped.
fn rereview_item(
    reviewers: &[&GateRunnerRecord],
    repo: &str,
    pr: u64,
    head: &str,
    held_at: &str,
) -> Option<Item> {
    let (record, task) = reviewers.iter().find_map(|record| {
        let task = record.heartbeat.current.as_ref()?;
        (task.repo == repo && task.pr == Some(pr) && task.sha == head).then_some((record, task))
    })?;
    let mut item = Draft {
        id: format!("reviewer-rereviewing:{repo}:{pr}"),
        kind: "reviewer_rereviewing",
        severity: Severity::Watch,
        title: format!(
            "Re-reviewing {} since {} (previous hold at {} is stale)",
            short_sha(head),
            task.started_at.to_rfc3339(),
            short_sha(held_at)
        ),
        reason: format!(
            "{} is reviewing {repo}#{pr} at its current head {}, which it started at {}. \
             The verdict it left at {} was taken on a head that has been replaced since, so \
             it asks nothing of anybody; wait for the pass on the current head.",
            record.heartbeat.runner_id,
            short_sha(head),
            task.started_at.to_rfc3339(),
            short_sha(held_at)
        ),
        href: pull_href(repo, pr),
        label: "Wait for the review of the current head",
        api: None,
        command: None,
    }
    .build();
    item.since = Some(task.started_at.to_rfc3339());
    item.repo = Some(repo.to_string());
    item.pr = Some(pr);
    item.sha = Some(head.to_string());
    Some(item)
}

/// Reviewers that could not review an open PR, and a gate with no runner.
///
/// `open_heads` is every open pull request and the head it is at now. A
/// verdict is about the head it was taken on, so a hold only holds the pull
/// request up while that head is still current: the fix for a hold is a push,
/// and the push that answers it moves the head. Keyed by the live head rather
/// than by "the pull request is open", `reviewer_stuck` stayed up for
/// 39 minutes after the fix landed (live: jeryu/jeryu-ci-runner#109 held at
/// 91beebd, fixed by edd08c5, which the reviewer was already re-reading), and
/// read as a pull request nobody could move.
pub(crate) fn runner_items(
    runners: &[GateRunnerRecord],
    open_heads: &BTreeMap<(String, u64), String>,
    waiting_review_count: usize,
    queue_building: bool,
    reviewer_thresholds: ReviewerOfflineThresholds,
    now: DateTime<Utc>,
    hosts: &Hosts,
) -> Vec<Item> {
    let mut items = Vec::new();
    let reviewers: Vec<&GateRunnerRecord> = runners
        .iter()
        .filter(|r| is_reviewer(&r.heartbeat))
        .collect();
    items.extend(reviewer_offline_items(
        &reviewers,
        waiting_review_count,
        reviewer_thresholds,
        now,
    ));
    let mut rowed: BTreeSet<(String, u64)> = BTreeSet::new();
    for record in &reviewers {
        let Some(last) = &record.heartbeat.last else {
            continue;
        };
        let Some(explained) = review_hold_explanation(&last.conclusion) else {
            continue;
        };
        let Some((pr, head)) = last
            .pr
            .and_then(|pr| Some((pr, open_heads.get(&(last.repo.clone(), pr))?)))
        else {
            continue;
        };
        if !rowed.insert((last.repo.clone(), pr)) {
            continue;
        }
        if head != &last.sha {
            // The head the verdict was taken on is gone, so the verdict asks
            // nothing of anybody. Say so only while the reviewer is visibly
            // on the new head: that is what a reader who remembers the hold
            // came to find out.
            if let Some(row) = rereview_item(&reviewers, &last.repo, pr, head, &last.sha) {
                items.push(row);
            }
            continue;
        }
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
    if !gate_online && (!open_heads.is_empty() || queue_building) {
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
                open_heads.len(),
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
    } else if !open_heads.is_empty() {
        items.extend(silent_gate_host_items(
            &runners.iter().filter(gate_slot).collect::<Vec<_>>(),
            open_heads.len(),
            now,
        ));
    }
    items
}

/// Gate hosts are partitioned by owner, so a host that hangs while another
/// beats leaves its share ungated with the any-slot-online rule still quiet.
/// One row per host whose slots have all been silent past the offline
/// threshold. A heartbeat names its host but not the owners it gates, so the
/// share is every open pull request and the row says so.
fn silent_gate_host_items(
    gate_slots: &[&GateRunnerRecord],
    open_count: usize,
    now: DateTime<Utc>,
) -> Vec<Item> {
    let mut by_host: BTreeMap<&str, Vec<&GateRunnerRecord>> = BTreeMap::new();
    for record in gate_slots {
        by_host
            .entry(record.heartbeat.host.as_str())
            .or_default()
            .push(record);
    }
    let Some(live) = by_host
        .iter()
        .find(|(_, slots)| slots.iter().any(|r| is_online(r, now)))
        .map(|(host, _)| *host)
    else {
        return Vec::new();
    };
    by_host
        .iter()
        .filter(|(_, slots)| !slots.iter().any(|r| is_online(r, now)))
        .map(|(host, slots)| {
            let last = slots.iter().map(|r| r.received_at).max();
            let seen = last.map_or_else(|| "never".to_string(), |t| t.to_rfc3339());
            let mut item = Draft {
                id: format!("gate-runner-down:{host}"),
                kind: "gate_runner_down",
                severity: Severity::Critical,
                title: format!("Gate host {host} is not reporting"),
                reason: format!(
                    "No gate runner slot on {host} has sent a heartbeat in the last 3 \
                     minutes (last at {seen}) while {live} is alive, so the owners {host} \
                     gates have no required check. Heartbeats do not name those owners: up \
                     to {open_count} open pull request(s) may be waiting on {host}. Move \
                     its owners into GATE_RUNNER_OWNERS on {live}, then restart its \
                     runner timers; the command shows where that is set."
                ),
                href: "/runners".to_string(),
                label: "Move the silent host's owners to a live gate host",
                api: None,
                command: Some(Shell {
                    line: "systemctl --user show 'pr-gate-runner@*' -p Environment -p EnvironmentFiles"
                        .to_string(),
                    run_in: Hosts::anywhere(live),
                }),
            }
            .build();
            item.since = last.map(|t| t.to_rfc3339());
            item
        })
        .collect()
}

fn reviewer_offline_items(
    reviewers: &[&GateRunnerRecord],
    waiting_review_count: usize,
    thresholds: ReviewerOfflineThresholds,
    now: DateTime<Utc>,
) -> Vec<Item> {
    if waiting_review_count == 0 {
        return Vec::new();
    }
    let last_heartbeat = reviewers.iter().map(|record| record.received_at).max();
    let last_pass = reviewers
        .iter()
        .filter_map(|record| record.heartbeat.last.as_ref().map(|last| last.finished_at))
        .max();
    let heartbeat_quiet =
        last_heartbeat.is_none_or(|seen| (now - seen).num_minutes() > thresholds.heartbeat_minutes);
    let pass_quiet =
        last_pass.is_none_or(|finished| (now - finished).num_minutes() > thresholds.pass_minutes);
    if !heartbeat_quiet && !pass_quiet {
        return Vec::new();
    }
    let heartbeat_phrase = last_heartbeat
        .map(|seen| format!("last heartbeat {seen}"))
        .unwrap_or_else(|| "no reviewer heartbeat has arrived since startup".to_string());
    let pass_phrase = last_pass
        .map(|finished| format!("last completed review pass {finished}"))
        .unwrap_or_else(|| "no completed review pass has been reported since startup".to_string());
    let reason = if heartbeat_quiet && pass_quiet {
        format!(
            "{waiting_review_count} pull request(s) wait for review; {heartbeat_phrase}, and \
             {pass_phrase}. The reviewer is not reporting progress, so new approvals will not \
             arrive until its runner reports again."
        )
    } else if heartbeat_quiet {
        format!(
            "{waiting_review_count} pull request(s) wait for review; {heartbeat_phrase}. The \
             reviewer heartbeat is past the {} minute site threshold, so new approvals may not \
             arrive until its runner reports again.",
            thresholds.heartbeat_minutes
        )
    } else {
        format!(
            "{waiting_review_count} pull request(s) wait for review; {pass_phrase}. No review \
             pass has completed within the {} minute site threshold, so approvals are not \
             keeping up.",
            thresholds.pass_minutes
        )
    };
    let mut item = Draft {
        id: "reviewer-offline".to_string(),
        kind: "reviewer_offline",
        severity: Severity::Critical,
        title: format!("{waiting_review_count} pull request(s) wait and no reviewer is current"),
        reason,
        href: "/runners".to_string(),
        label: "Check the reviewer runner and restart it if it is not reporting",
        api: None,
        command: None,
    }
    .build();
    item.since = last_heartbeat.map(|seen| seen.to_rfc3339());
    vec![item]
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

/// Whether a command line names the directory it runs from: it starts with a
/// `cd`, or its program is an absolute or home path. A bare relative path
/// (`scripts/release/...`) only works from the right checkout.
fn is_self_contained(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("cd ") || line.starts_with('/') || line.starts_with('~')
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
                    // A line that names its own directory (a `cd`, or an
                    // absolute or home path) runs as pasted. Only a bare
                    // relative path is a path inside the repository that
                    // staged the release, which the event names.
                    run_in: if is_self_contained(line) {
                        Hosts::anywhere(&hosts.release)
                    } else {
                        match event.repo.as_deref() {
                            Some(repo) => Hosts::checkout(&hosts.release, repo),
                            None => format!(
                                "{}, in a checkout of the repository that staged it",
                                hosts.release
                            ),
                        }
                    },
                });
            // The button: only with a release host polling for asks, a real
            // release id and a repository to name, and not while an ask for
            // this release is already waiting or running.
            let asked = event
                .repo
                .as_deref()
                .and_then(|repo| hosts.release_queue.open_deploy(repo, release));
            let api = match (event.repo.as_deref(), asked) {
                (Some(repo), None)
                    if hosts.release_queue.polls(repo)
                        && crate::web::release_actions::is_release_id(release) =>
                {
                    Some(super::deploy_call(repo, release))
                }
                _ => None,
            };
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
                     until it is deployed.{}{}",
                    live.map_or_else(
                        || "an earlier build".to_string(),
                        |(sha, _)| sha.chars().take(10).collect()
                    ),
                    failed_attempt(facts, release),
                    asked.map_or_else(String::new, |ask| format!(" {}", ask.progress()))
                ),
                href: "/releases".to_string(),
                label: if asked.is_some() {
                    "Deploy requested"
                } else {
                    "Deploy the staged release"
                },
                api,
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
