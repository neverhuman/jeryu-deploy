//! The attention inbox: `GET /api/v1/attention`.
//!
//! Everything in the pipeline that is waiting on a person, computed from the
//! current state on each call (cached for a few seconds) and never from old
//! events, so an item disappears when its cause is fixed. One small collector
//! per source gathers plain facts; one pure rule per source turns facts into
//! items, which is what the tests drive.
//!
//! An item somebody deliberately deferred is left out until its date passes;
//! `acks.rs` holds those acknowledgements, keyed by item id whatever the kind.
//!
//! Each item names exactly one next step. `action.command` is set when the
//! step is a shell command to run off-site, and then `action.run_in` says on
//! which machine and in which directory; otherwise the step is to open `href`
//! and do what `action.label` says. `next_step` spells that out in one
//! sentence for a reader, human or agent, with no other context.
//!
//! `action.api` is set as well whenever a route of this API performs the step:
//! it is the call to make, so an agent replays the step instead of reading
//! `next_step` and guessing which endpoint it meant. `href` stays where to go
//! to understand the item.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{DateTime, Utc};
use jeryu_core::CommitStatusState;
use serde::Serialize;
use serde_json::{Value, json};

use super::super::WebState;
use super::super::merge_queue::QueueState;
use super::super::paging::{Page, PageInfo, PageParams};
use super::super::shift::FamilySnapshot;

pub(crate) mod acks;
mod board;
mod flow;
mod hosts;
mod mirror;
mod pins;
mod pushed;
mod scorecard;
mod stranded;
mod summary;
#[cfg(test)]
pub(super) mod web_routes;
mod work;

pub(crate) use acks::AckStore;
pub(crate) use board::{BoardFacts, BoardLane, board_items};
/// The pure path rule behind `pr_changes_own_gate`; `pull_items` is the only
/// caller outside its own tests.
#[cfg(test)]
pub(crate) use flow::gate_definition_paths;
pub(crate) use flow::{
    DraftFacts, LatestDeployment, MainRedFacts, ProductionFacts, PullFacts,
    ReviewerOfflineThresholds, Superseded, SupersededPullFacts, draft_items,
    drop_shadowed_pull_rows, main_red_items, pull_items, queue_items, release_items,
    review_hold_explanation, runner_items, superseded_pull_items,
};
pub(crate) use hosts::Hosts;
pub(crate) use mirror::{MirrorDrift, MirrorFailure, divergence_items, mirror_items};
pub(crate) use pins::{pin_items, web_pin_bump_prs};
pub(crate) use pushed::{PushedAfterMergeFacts, pushed_after_merge_items};
pub(crate) use scorecard::digest_items;
pub(crate) use stranded::{StrandedBranchFacts, stranded_branch_items};
pub(crate) use summary::{FamilyCounts, family_summary};
pub(crate) use work::{budget_items, shift_items, todo_items, worker_items};

/// `v1.1` added `action.api`, `v1.2` the optional `budget`, `v1.3` the
/// optional `pulls`, `v1.4` the optional `assignee`, `v1.5` the page's
/// `families` summary (and a `family` read off `repo` when a collector named
/// none) and `v1.6` the optional `digest`: every earlier field is unchanged,
/// so a client written against `v1` reads a `v1.6` answer as it always did.
pub(crate) const ATTENTION_SCHEMA: &str = "jeryu.attention/v1.6";
const CACHE_FOR: Duration = Duration::from_secs(10);
/// A claim whose lease died this long ago is stuck rather than between renewals.
pub(super) const STUCK_CLAIM_MINUTES: i64 = 10;
/// An untriaged todo a healthy worker has not picked up in this long is no
/// longer waiting on the next pass.
pub(super) const UNTRIAGED_MINUTES: i64 = 30;
/// A mergeable PR left open this long is waiting on somebody to merge it.
pub(super) const READY_TO_MERGE_MINUTES: i64 = 10;
/// Two or more pull requests ready this long are not slow authors: the
/// automated reviewer merges approved work every five minutes, so six of its
/// passes with nothing landing says the reviewer itself stopped merging.
pub(super) const MERGER_STALLED_MINUTES: i64 = 30;
/// A reviewer heartbeat this quiet is not just between ticks. Site
/// configuration can override it with `JERYU_REVIEWER_HEARTBEAT_OFFLINE_MINUTES`.
pub(super) const REVIEWER_HEARTBEAT_OFFLINE_MINUTES: i64 = 10;
/// A review pass this old, while pull requests wait for review, says the
/// reviewer is not making progress. Site configuration can override it with
/// `JERYU_REVIEWER_PASS_OFFLINE_MINUTES`.
pub(super) const REVIEWER_PASS_OFFLINE_MINUTES: i64 = 15;
pub(super) const QUEUE_LOOKBACK_HOURS: i64 = 24;
/// A queue entry still building after this long is no longer waiting for a
/// gate that is about to report.
pub(super) const QUEUE_STUCK_MINUTES: i64 = 30;
/// This many open pull requests on one branch with the same verdict are one
/// decision rather than that many: the same review on every pull request of a
/// campaign is answered on the branch once (live on 2026-10-06:
/// `dev/jankurai-v3-receipts` filled the inbox with about twenty
/// `pr_changes_requested` rows saying the same thing).
pub(super) const BRANCH_CAMPAIGN_PRS: usize = 3;
/// A draft with no push for this many days is waiting on somebody to mark it
/// ready for review; `JERYU_DRAFT_IDLE_DAYS` overrides it per deployment.
pub(super) const DRAFT_IDLE_DAYS: i64 = 3;
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

impl Severity {
    const ALL: [Self; 3] = [Self::Critical, Self::Action, Self::Watch];

    /// The severity as it is spelled on the wire.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Action => "action",
            Self::Watch => "watch",
        }
    }

    /// Every severity as it is spelled on the wire: the closed set
    /// `?severity=` accepts.
    pub(crate) fn names() -> Vec<&'static str> {
        Self::ALL.iter().map(|severity| severity.as_str()).collect()
    }

    fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|severity| severity.as_str() == text)
    }
}

/// Every kind the inbox can emit, sorted. The closed set `?kind=` accepts, and
/// what `docs/pipeline-events.md` publishes: a kind absent here could only ever
/// match nothing, so a reader who filters by it hears so.
pub(crate) const KINDS: &[&str] = &[
    "deploy_failed",
    "gate_runner_down",
    "main_red",
    "merger_stalled",
    "mirror_diverged",
    "mirror_failing",
    "pin_behind",
    "pr_already_landed",
    "pr_awaiting_approval",
    "pr_branch_campaign",
    "pr_changes_own_gate",
    "pr_changes_requested",
    "pr_checks_failing",
    "pr_draft_waiting",
    "pr_ready_to_merge",
    "pr_superseded",
    "pushed_after_merge",
    "queue_failed",
    "queue_refused",
    "queue_stuck",
    "release_board_problem",
    "release_stage_failed",
    "release_staged",
    "reviewer_offline",
    "reviewer_rereviewing",
    "reviewer_stuck",
    "scorecard_digest",
    "shift_budget_spent",
    "shift_stranded_work",
    "shift_without_pr",
    "stranded_branch",
    "todo_blocked",
    "todo_check_outcome",
    "todo_handoff",
    "todo_parked",
    "todo_stuck_claim",
    "todo_untriaged",
    "todo_waiting_on_blocker",
    "workers_down",
];

/// The step as one call on this API: what a caller sends to perform it, not a
/// link to a page about it. Paths are absolute and ready to send.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ApiCall {
    pub method: &'static str,
    pub path: String,
    /// The request body, left out of the JSON when the route takes none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    /// What the confirm step says before the call is sent, when the default
    /// ("<label> now? The forge acts straight away.") would not be true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Action {
    pub label: String,
    /// A copyable shell line when the step happens off-site.
    pub command: Option<String>,
    /// The call that performs the step, when a route of this API does. Left
    /// out of the JSON when no route does it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiCall>,
    /// Where `command` is run, as a short phrase naming the machine and the
    /// directory ("node-0, any directory"). Set exactly when `command` is, and
    /// left out of the JSON otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_in: Option<String>,
}

/// A shell line and where it is run. One value, so that no rule can offer a
/// command without saying where.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Shell {
    pub(super) line: String,
    pub(super) run_in: String,
}

/// One pull request a folded row stands for, and where to open it, so a
/// reader reaches every pull request of a campaign without parsing `reason`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct PullLink {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    pub href: String,
}

/// The money behind a `shift_budget_spent` item, as numbers rather than
/// prose, so a surface showing the spend never parses `reason` back apart.
/// A number the event did not carry is left out of the JSON.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct BudgetSpend {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    /// Claimable todos still waiting when the budget ran out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Item {
    pub id: String,
    pub kind: &'static str,
    pub severity: Severity,
    pub title: String,
    pub reason: String,
    pub since: Option<String>,
    /// Canonical family key (see `crate::web::family`).
    pub family: Option<String>,
    /// What a reader is shown for `family`.
    pub family_label: Option<String>,
    pub repo: Option<String>,
    pub pr: Option<u64>,
    pub todo_id: Option<String>,
    pub sha: Option<String>,
    pub shift: Option<String>,
    /// Set on `shift_budget_spent` only; the key is absent on every other item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetSpend>,
    /// Set on `pr_branch_campaign` only, where one row stands for several pull
    /// requests; the key is absent on every other item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pulls: Option<Vec<PullLink>>,
    /// Set on a todo item when a person has taken the todo on; the key is
    /// absent on every other item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Set on `scorecard_digest` only: the week's headline numbers as the
    /// weekly job recorded them, so a surface shows them without parsing
    /// `reason`. The key is absent on every other kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<crate::web::scorecard::digest::Digest>,
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
    pub branch_discovery: &'static str,
}

/// The few fields an item is built from; the rest default to `None`.
pub(super) struct Draft<'a> {
    pub(super) id: String,
    pub(super) kind: &'static str,
    pub(super) severity: Severity,
    pub(super) title: String,
    pub(super) reason: String,
    pub(super) href: String,
    pub(super) label: &'a str,
    /// The call that performs the step; see [`ApiCall`].
    pub(super) api: Option<ApiCall>,
    pub(super) command: Option<Shell>,
}

impl Draft<'_> {
    pub(super) fn build(self) -> Item {
        let next_step = match &self.command {
            Some(shell) => format!("{}: on {}, run `{}`", self.label, shell.run_in, shell.line),
            None => format!("{}: open {}", self.label, self.href),
        };
        let label = self.label.to_string();
        Item {
            id: self.id,
            kind: self.kind,
            severity: self.severity,
            title: self.title,
            reason: clip(&self.reason),
            since: None,
            family: None,
            family_label: None,
            repo: None,
            pr: None,
            todo_id: None,
            sha: None,
            shift: None,
            budget: None,
            pulls: None,
            assignee: None,
            digest: None,
            href: self.href,
            action: match self.command {
                Some(shell) => Action {
                    label,
                    command: Some(shell.line),
                    api: self.api,
                    run_in: Some(shell.run_in),
                },
                None => Action {
                    label,
                    command: None,
                    api: self.api,
                    run_in: None,
                },
            },
            next_step,
        }
    }
}

pub(super) fn clip(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_REASON_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_REASON_CHARS - 1).collect();
    out.push('…');
    out
}

pub(super) fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

// Every href below is a route of the web app itself, so that opening an item
// never depends on a redirect; `web_routes` holds the route patterns and the
// tests walk every kind's href against them.

/// The repository page. Repository URLs name the provider first
/// (`/repos/<provider>/<owner>/<name>`), and the forge's own provider segment
/// is `jeryu`.
pub(super) fn repo_href(repo: &str) -> String {
    format!("/repos/{}/{repo}", crate::links::NATIVE_PROVIDER)
}

pub(super) fn pull_href(repo: &str, number: u64) -> String {
    format!("{}/pulls/{number}", repo_href(repo))
}

/// One todo's own page. Ids are unique across families, so `family` only
/// narrows the lookup; todo ids are slugs and need no escaping.
pub(super) fn todo_href(family: &str, id: &str) -> String {
    format!("/work/{id}?family={family}")
}

/// Work, filtered to one family: the queue of every family is one page, and
/// `#workers` and `#add` are places on it.
pub(super) fn work_href(family: &str) -> String {
    format!("/work?family={family}")
}

/// The workers strip on Work.
pub(super) const WORKERS_HREF: &str = "/work#workers";

/// Releases, scoped to one deploy repository: what each environment runs and,
/// last, what is merged in a dependency and not yet pinned.
pub(super) fn releases_href(repo: &str) -> String {
    format!("/releases?repo={repo}")
}

/// `POST /api/v1/shift/shifts/:family/pr`: open the review pull request of
/// one shift branch, in every repository of the family that has the branch.
pub(super) fn open_shift_pr_call(family: &str, branch: &str) -> ApiCall {
    ApiCall {
        method: "POST",
        path: format!("/api/v1/shift/shifts/{family}/pr"),
        body: Some(json!({ "branch": branch })),
        confirm: None,
    }
}

/// `POST /api/v1/shift/todos/:family/:id/action`: `release`, `done`, `park`
/// and the rest of one todo's transitions.
pub(super) fn todo_action_call(family: &str, id: &str, action: &str) -> ApiCall {
    ApiCall {
        method: "POST",
        path: format!("/api/v1/shift/todos/{family}/{id}/action"),
        body: Some(json!({ "action": action })),
        confirm: None,
    }
}

/// `POST /api/v1/repos/:id/pulls/:number/queue`: join the merge queue. The
/// repository is addressed by `owner/name`, which the API resolves.
pub(super) fn enqueue_call(repo: &str, number: u64) -> ApiCall {
    ApiCall {
        method: "POST",
        path: format!("/api/v1/repos/{repo}/pulls/{number}/queue"),
        body: None,
        confirm: None,
    }
}

/// `POST /api/v1/repos/:id/pulls/:number/regate`: gate the current head
/// again. The gate runner reuses a result it already has for a head, so a
/// failure that was not the head's own fault needs this to be proved again.
pub(super) fn regate_call(repo: &str, number: u64) -> ApiCall {
    ApiCall {
        method: "POST",
        path: format!("/api/v1/repos/{repo}/pulls/{number}/regate"),
        body: None,
        confirm: None,
    }
}

/// `POST /api/v1/repos/:id/pulls/:number/ready`: a draft becomes a pull
/// request that reviews, gates and the queue act on.
pub(super) fn ready_for_review_call(repo: &str, number: u64) -> ApiCall {
    ApiCall {
        method: "POST",
        path: format!("/api/v1/repos/{repo}/pulls/{number}/ready"),
        body: None,
        confirm: None,
    }
}

/// `POST /api/v1/release-actions`: ask the release host to deploy a staged
/// release (`release_actions`). The host runs it, not the forge, so the
/// confirm step says so.
pub(super) fn deploy_call(repo: &str, release: &str) -> ApiCall {
    ApiCall {
        method: "POST",
        path: "/api/v1/release-actions".to_string(),
        body: Some(json!({ "action": "deploy", "repo": repo, "release": release })),
        confirm: Some(format!(
            "Deploy {release} to production? The release host picks this up within 30 \
             seconds and switches the forge, which restarts it; the page reconnects by itself."
        )),
    }
}

/// `POST /api/v1/release-actions`: ask the release host to run auto-pin now,
/// after forgetting its give-up on `head` when one is named.
pub(super) fn auto_pin_call(repo: &str, head: Option<&str>) -> ApiCall {
    let body = match head {
        Some(head) => json!({ "action": "auto_pin_retry", "repo": repo, "head": head }),
        None => json!({ "action": "auto_pin", "repo": repo }),
    };
    ApiCall {
        method: "POST",
        path: "/api/v1/release-actions".to_string(),
        body: Some(body),
        confirm: Some(
            "Run auto-pin now? The release host starts it within 30 seconds; it builds the web \
             bundle and opens the pin bump pull request."
                .to_string(),
        ),
    }
}

pub(super) fn close_pull_call(repo: &str, number: u64, superseded_by: u64) -> ApiCall {
    ApiCall {
        method: "PATCH",
        path: format!("/api/v3/repos/{repo}/pulls/{number}"),
        // One call closes it, records the pull request that replaced it and
        // leaves the pointer comment, so the row's own click is what the
        // replacement link is written by.
        body: Some(json!({"state": "closed", "superseded_by": superseded_by})),
        confirm: None,
    }
}

/// `POST /api/v1/repos/:id/pulls/:number/close`: close a pull request whose
/// change is already on its base branch, after commenting the commit it
/// landed as. It performs that one case only, so the row can carry it.
pub(super) fn close_landed_pull_call(repo: &str, number: u64) -> ApiCall {
    ApiCall {
        method: "POST",
        path: format!("/api/v1/repos/{repo}/pulls/{number}/close"),
        body: None,
        confirm: None,
    }
}

fn open_pull_facts(state: &WebState) -> Vec<PullFacts> {
    let mut pulls = Vec::new();
    // An archived repository is read-only: nothing on it can be acted on, so
    // it never asks for attention.
    for repo in state.core.list_repositories(None) {
        if repo.archived {
            continue;
        }
        let Ok(listed) = state.core.list_pull_requests(&repo.owner, &repo.name, None) else {
            continue;
        };
        for mut pr in listed {
            if let Some(head) = super::super::pulls::landed::live_head(state, &pr) {
                pr.head.sha = head;
            }
            let Some(posture) = super::super::pulls::attention_posture(state, &pr) else {
                continue;
            };
            pulls.push(PullFacts {
                repo: format!("{}/{}", pr.owner, pr.repo),
                number: pr.number,
                title: pr.title.clone(),
                author: pr.author.clone(),
                head_sha: pr.head.sha.clone(),
                head_ref: pr.head.ref_name.clone(),
                base_ref: pr.base.ref_name.clone(),
                updated_at: pr.updated_at,
                posture,
                changed_paths: super::super::pulls::diff::changed_paths(state, &pr),
                landed: super::super::pulls::landed::already_landed(state, &pr),
            });
        }
    }
    pulls
}

/// The pull requests a later pull request replaced (the facts behind
/// `pr_superseded`), for Flow to hold at the station they stopped at.
///
/// Every replacement is followed to the end of its chain: a lot of `#141`,
/// replaced by `#145` which was itself replaced by `#147`, is held by `#147`,
/// the pull request that is still live. A pull request with a recorded link is
/// included whatever state it is in, so a lot still open at a station under a
/// pull request that was closed as replaced names its replacement too.
pub(crate) fn superseded_open_pulls(state: &WebState) -> Vec<flow::Superseded> {
    let facts = all_pull_facts(state);
    let mut found: Vec<flow::Superseded> = flow::superseded_pulls(&facts)
        .into_iter()
        .map(|(_, found)| found)
        .collect();
    for old in &facts {
        let Some(number) = old.recorded_replacement else {
            continue;
        };
        if found
            .iter()
            .any(|hit| hit.repo == old.repo && hit.number == old.number)
        {
            continue;
        }
        let Some(replacement) = facts
            .iter()
            .find(|new| new.repo == old.repo && new.number == number)
        else {
            continue;
        };
        found.push(flow::Superseded {
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
        });
    }
    for hit in &mut found {
        let Some((owner, name)) = hit.replacement_repo.split_once('/') else {
            continue;
        };
        if let Some(end) =
            state
                .github
                .pull_replacements()
                .chain_end(owner, name, hit.replacement_number)
            && let Some(last) = facts
                .iter()
                .find(|new| new.repo == hit.replacement_repo && new.number == end)
        {
            hit.replacement_number = last.number;
            hit.replacement_state = match last.status {
                "merged" => "merged",
                "closed" => "closed",
                _ => "open",
            };
        }
    }
    found
}

fn all_pull_facts(state: &WebState) -> Vec<SupersededPullFacts> {
    let mut pulls = Vec::new();
    for repo in state.core.list_repositories(None) {
        if repo.archived {
            continue;
        }
        let Ok(listed) = state.core.list_pull_requests(&repo.owner, &repo.name, None) else {
            continue;
        };
        pulls.extend(listed.into_iter().map(|pr| {
            SupersededPullFacts {
                recorded_replacement: state
                    .github
                    .pull_replacements()
                    .superseded_by(&pr.owner, &pr.repo, pr.number)
                    .map(|link| link.number),
                repo: format!("{}/{}", pr.owner, pr.repo),
                author: pr.author,
                base_ref: pr.base.ref_name,
                source_repository: pr.source_repository,
                number: pr.number,
                title: pr.title,
                body: pr.body,
                head_ref: pr.head.ref_name,
                created_at: pr.created_at,
                status: if pr.merged {
                    "merged"
                } else if matches!(
                    pr.state,
                    jeryu_core::PullRequestState::Open
                        | jeryu_core::PullRequestState::Draft
                        | jeryu_core::PullRequestState::ReadyForReview
                        | jeryu_core::PullRequestState::Approved
                        | jeryu_core::PullRequestState::Mergeable
                        | jeryu_core::PullRequestState::Queued
                        | jeryu_core::PullRequestState::BlockedByChecks
                        | jeryu_core::PullRequestState::BlockedByPolicy
                        | jeryu_core::PullRequestState::SpeculativeMergeTesting
                ) {
                    "open"
                } else {
                    "closed"
                },
            }
        }));
    }
    pulls
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MainHealthFailure {
    pub(crate) first_red: String,
    pub(crate) first_step: String,
    pub(crate) log_id: String,
}

pub(crate) fn main_head(state: &WebState, owner: &str, name: &str) -> Option<String> {
    let repo = state.github.core().get_repository(owner, name).ok()?;
    let bare = state.repo_manager.open_parts(owner, name).ok()?;
    jeryu_gitd::refs::RefService::new((*state.repo_manager).clone())
        .resolve_commit(&bare, &format!("refs/heads/{}", repo.default_branch))
        .ok()
        .flatten()
}

pub(crate) fn main_health_at(
    state: &WebState,
    owner: &str,
    name: &str,
    sha: &str,
) -> Option<MainHealthFailure> {
    let context = format!("{name}/main-health");
    let statuses = state.core.combined_status(owner, name, sha).ok()?.statuses;
    let mut context_statuses = statuses
        .iter()
        .filter(|status| status.context == context)
        .collect::<Vec<_>>();
    context_statuses.sort_by_key(|status| status.updated_at);
    let latest = context_statuses.last()?;
    if !matches!(
        latest.state,
        CommitStatusState::Failure | CommitStatusState::Error
    ) {
        return None;
    }
    let mut first_red = latest.created_at;
    for status in context_statuses.iter().rev() {
        if !matches!(
            status.state,
            CommitStatusState::Failure | CommitStatusState::Error
        ) {
            break;
        }
        first_red = status.created_at;
    }
    let first_red = first_red.to_rfc3339();
    let first_step = latest
        .description
        .as_deref()
        .and_then(|description| description.split("; cause: ").nth(1))
        .map(str::to_string)
        .unwrap_or_else(|| "main-health failed".to_string());
    let log_id = latest
        .description
        .as_deref()
        .and_then(|description| description.split("; log ").nth(1))
        .and_then(|tail| tail.split(';').next())
        .map(str::trim)
        .map(|log| log.strip_prefix("sha256 ").unwrap_or(log).trim())
        .filter(|log| !log.is_empty())
        .unwrap_or("unknown")
        .to_string();
    Some(MainHealthFailure {
        first_red,
        first_step,
        log_id,
    })
}

/// A red main read from the `main.status` events of the gate that runs on
/// every push to the default branch, for a repository that posts no
/// `main-health` commit status.
fn main_gate_failure(
    state: &WebState,
    owner: &str,
    name: &str,
    sha: &str,
) -> Option<MainHealthFailure> {
    let repo = format!("{owner}/{name}");
    // Only a verdict about the head that is there now is news: a newer head
    // the gate has not judged yet is unknown, not red. That is one query, so
    // it comes first; the full projection (three event reads) is built only
    // for a head that is red.
    if !state_head_red(state, &repo, sha) {
        return None;
    }
    let health = super::main_health::for_repo(state, Some(&repo), Utc::now()).ok()?;
    let open = health
        .into_iter()
        .next()?
        .intervals
        .into_iter()
        .find(|interval| interval.until.is_none())?;
    Some(gate_failure(&repo, &open))
}

/// The attention row's facts for an open red interval: since when, and the one
/// sentence that says why -- the change that turned main red, and the tests
/// that failed when the runner named them. The gate reports main through
/// events rather than an attempt log, so there is no log id to name.
pub(crate) fn gate_failure(
    repo: &str,
    open: &super::main_health::RedInterval,
) -> MainHealthFailure {
    let mut first_step = "the required gate failed on main".to_string();
    if let Some(pr) = open.culprit_pr {
        first_step.push_str(&format!("; turned red by {repo}#{pr}"));
    }
    if !open.tests.is_empty() {
        first_step.push_str(&format!("; failing: {}", open.tests.join(", ")));
    }
    MainHealthFailure {
        first_red: open.since.clone(),
        first_step,
        log_id: "unknown".to_string(),
    }
}

/// Read the latest completed verdict for the actual head. A started event
/// is not a verdict, and an event-store failure cannot establish red state.
fn state_head_red(state: &WebState, repo: &str, sha: &str) -> bool {
    // The newest verdict about this exact head, chosen in SQL: `started`
    // events are no verdict and cannot push it out of a fixed page.
    state
        .events
        .with_conn(|conn| {
            conn.query_row(
                "SELECT outcome FROM pipeline_events \
                 WHERE kind = 'main.status' AND repo = ?1 AND sha = ?2 \
                   AND outcome IN ('red', 'green') \
                 ORDER BY seq DESC LIMIT 1",
                rusqlite::params![repo, sha],
                |row| row.get::<_, String>(0),
            )
            .map(Some)
            .or_else(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.to_string()),
            })
        })
        .ok()
        .flatten()
        .is_some_and(|outcome| outcome == "red")
}

#[cfg(test)]
mod main_gate_tests;
#[cfg(test)]
mod summary_tests;

fn main_red_facts(state: &WebState) -> Vec<MainRedFacts> {
    let mut facts = Vec::new();
    for repo in state.core.list_repositories(None) {
        if repo.archived {
            continue;
        }
        let Some(sha) = main_head(state, &repo.owner, &repo.name) else {
            continue;
        };
        let Some(health) = main_health_at(state, &repo.owner, &repo.name, &sha)
            .or_else(|| main_gate_failure(state, &repo.owner, &repo.name, &sha))
        else {
            continue;
        };
        facts.push(MainRedFacts {
            repo: format!("{}/{}", repo.owner, repo.name),
            sha,
            since: health.first_red,
            first_step: health.first_step,
            log_id: health.log_id,
        });
    }
    facts
}

fn waits_for_review(pull: &PullFacts) -> bool {
    // An already-landed pull request asks to be closed, not reviewed.
    pull.landed.is_none()
        && pull.posture.changes_requested == 0
        && pull.posture.failing.is_empty()
        && pull.posture.checks_green
        && pull.posture.approvals < pull.posture.required_approvals
}

/// How many idle days make a draft an attention item. A value that is not a
/// positive whole number of days is ignored in favour of [`DRAFT_IDLE_DAYS`],
/// so a typo in the environment cannot silence the rule or fire it at once.
pub(super) fn draft_idle_days() -> i64 {
    std::env::var("JERYU_DRAFT_IDLE_DAYS")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|days| *days > 0)
        .unwrap_or(DRAFT_IDLE_DAYS)
}

fn positive_minutes_env(name: &str, fallback: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|minutes| *minutes > 0)
        .unwrap_or(fallback)
}

pub(super) fn reviewer_offline_thresholds() -> ReviewerOfflineThresholds {
    ReviewerOfflineThresholds {
        heartbeat_minutes: positive_minutes_env(
            "JERYU_REVIEWER_HEARTBEAT_OFFLINE_MINUTES",
            REVIEWER_HEARTBEAT_OFFLINE_MINUTES,
        ),
        pass_minutes: positive_minutes_env(
            "JERYU_REVIEWER_PASS_OFFLINE_MINUTES",
            REVIEWER_PASS_OFFLINE_MINUTES,
        ),
    }
}

/// Every open draft, whatever its base branch. A draft is not part of the
/// merge flow, so it has no [`PullFacts`] posture; the one question about it is
/// how long it has sat.
fn draft_facts(state: &WebState) -> Vec<DraftFacts> {
    let mut drafts = Vec::new();
    for repo in state.core.list_repositories(None) {
        if repo.archived {
            continue;
        }
        let Ok(listed) = state.core.list_pull_requests(&repo.owner, &repo.name, None) else {
            continue;
        };
        for pr in listed {
            if !pr.draft || pr.merged {
                continue;
            }
            if matches!(
                pr.state,
                jeryu_core::PullRequestState::Closed | jeryu_core::PullRequestState::Merged
            ) {
                continue;
            }
            drafts.push(DraftFacts {
                repo: format!("{}/{}", pr.owner, pr.repo),
                number: pr.number,
                title: pr.title.clone(),
                author: pr.author.clone(),
                base_ref: pr.base.ref_name.clone(),
                head_sha: pr.head.sha.clone(),
                updated_at: pr.updated_at,
            });
        }
    }
    drafts
}

fn production_facts(state: &WebState) -> Vec<ProductionFacts> {
    let mut all = Vec::new();
    for repo in state.core.list_repositories(None) {
        if repo.archived {
            continue;
        }
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
            current_release: production.current.as_ref().and_then(|current| {
                current.deployment.payload["release"]
                    .as_str()
                    .map(str::to_string)
            }),
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

/// Every branch with patch-unique commits and no open pull request carrying it.
fn stranded_branch_facts(state: &WebState, now: DateTime<Utc>) -> Vec<StrandedBranchFacts> {
    super::super::stranded_branches::all_at_time(state, now)
        .into_iter()
        .map(|branch| StrandedBranchFacts {
            repo: branch.repo,
            branch: branch.branch,
            default_branch: branch.default_branch,
            commit_count: branch.commit_count,
            newest_sha: branch.newest_sha,
            newest_subject: branch.newest_subject,
            newest_at: branch.newest_at,
            closed_pr: branch.closed_pr,
        })
        .collect()
}

fn same_repo_name(full_or_short: &str, short: &str) -> bool {
    full_or_short == short || full_or_short.ends_with(&format!("/{short}"))
}

/// Every release board as the board rules need it: which lanes the page draws
/// red, and which sources the collector could not read.
fn board_facts(state: &WebState) -> Vec<BoardFacts> {
    use super::super::release_board::StageState;
    state
        .release_boards
        .all()
        .into_iter()
        .map(|board| BoardFacts {
            family: board.family,
            observed_at: board.observed_at,
            lanes: board
                .lanes
                .into_iter()
                .map(|lane| BoardLane {
                    id: lane.id,
                    name: lane.name,
                    red_stages: lane
                        .stages
                        .into_iter()
                        .filter(|stage| stage.state == StageState::Bad)
                        .map(|stage| format!("{}: {}", stage.name, stage.status))
                        .collect(),
                })
                .collect(),
            problems: board
                .problems
                .into_iter()
                .map(|problem| format!("{}: {}", problem.source, problem.message))
                .collect(),
        })
        .collect()
}

/// Everything waiting on a person right now, most urgent first.
pub(crate) fn collect(state: &WebState, now: DateTime<Utc>) -> AttentionResponse {
    let mut items = Vec::new();
    let mut hosts = Hosts::from_env();
    hosts.release_queue = crate::web::release_actions::ReleaseQueue::snapshot(state, now);
    let families: Vec<FamilySnapshot> = super::super::shift::attention_snapshot(state, now);
    let workers = super::super::shift::worker_rows(state, now);
    let mut waiting = Vec::new();
    for family in &families {
        // A healthy worker slot is what makes an untriaged todo somebody
        // else's job: it triages the todo on its next pass.
        let has_worker = workers.iter().any(|w| {
            w.healthy && w.heartbeat.family == family.name && w.heartbeat.slot != "supervisor"
        });
        items.extend(todo_items(&family.name, &family.todos, has_worker, now));
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
    items.extend(worker_items(&waiting, &workers, &hosts));
    // A spent shift budget is only visible in the event log: the operator's
    // cap is not state the forge holds, so this one rule reads events.
    let page = |kind: &str, needs_human| super::types::EventsQuery {
        kind: Some(kind.to_string()),
        needs_human,
        limit: Some(50),
        ..Default::default()
    };
    items.extend(budget_items(
        &state
            .events
            .query(&page("shift.exhausted", Some(true)))
            .unwrap_or_default(),
        &state
            .events
            .query(&page("todo.claimed", None))
            .unwrap_or_default(),
    ));
    let mut pin_events = state
        .events
        .query(&super::types::EventsQuery {
            source: Some("auto-pin".to_string()),
            limit: Some(20),
            ..Default::default()
        })
        .unwrap_or_default();
    // Other timer events must not evict the latest give-up receipts.
    pin_events.extend(
        state
            .events
            .query(&super::types::EventsQuery {
                source: Some("auto-pin".to_string()),
                kind: Some("pin.bump_failed".to_string()),
                needs_human: Some(true),
                limit: Some(20),
                ..Default::default()
            })
            .unwrap_or_default(),
    );
    pin_events.sort_by_key(|event| std::cmp::Reverse(event.seq));
    pin_events.dedup_by_key(|event| event.seq);
    items.extend(pin_items(
        &super::pins::snapshot(state).consumers,
        &pin_events,
        &hosts,
        now,
    ));
    items.extend(mirror_items(
        &super::super::repositories::mirror_failures(state),
        &hosts,
    ));
    items.extend(divergence_items(
        &super::super::mirror_reconcile::drift_rows(state),
    ));
    // Branches holding patch-unique commits with no open pull request. Shift
    // branches the shift feed already names stay there; this sweep fills the
    // rest, including dayshift branches that the shift-specific rule may not
    // see.
    let covered_shift_branches: Vec<(String, String)> = items
        .iter()
        .filter(|item| item.kind == "shift_without_pr")
        .filter_map(|item| Some((item.repo.clone()?, item.shift.clone()?)))
        .collect();
    let mut stranded = stranded_branch_facts(state, now);
    stranded.retain(|fact| {
        !covered_shift_branches
            .iter()
            .any(|(repo, branch)| fact.branch == *branch && same_repo_name(&fact.repo, repo))
    });
    let pushed: Vec<_> = stranded
        .iter()
        .filter_map(|branch| {
            let (owner, name) = branch.repo.split_once('/')?;
            super::super::pushed_after_merge::stranded(state, owner, name, &branch.branch, now)
        })
        .map(|push| PushedAfterMergeFacts {
            repo: push.repo,
            branch: push.branch,
            number: push.number,
            title: push.title,
            base_ref: push.base_ref,
            merged: push.merged,
            closed_at: push.closed_at,
            commits: push.commits,
            commit_count: push.commit_count,
        })
        .collect();
    stranded.retain(|branch| {
        !pushed
            .iter()
            .any(|push| push.repo == branch.repo && push.branch == branch.branch)
    });
    items.extend(pushed_after_merge_items(&pushed));
    items.extend(stranded_branch_items(&stranded));
    let pulls = open_pull_facts(state);
    let superseded_pulls = all_pull_facts(state);
    let open: BTreeSet<(String, u64)> = pulls.iter().map(|p| (p.repo.clone(), p.number)).collect();
    // Every open pull request and the head it is at now, so a rule that
    // judges a head-anchored verdict can tell a current one from a stale one.
    let open_heads: BTreeMap<(String, u64), String> = pulls
        .iter()
        .map(|p| ((p.repo.clone(), p.number), p.head_sha.clone()))
        .collect();
    let waiting_review_count = pulls.iter().filter(|pull| waits_for_review(pull)).count();
    let entries = state.merge_queue.entries(state, |_| true);
    let building = entries.iter().any(|e| e.state == QueueState::Building);
    items.extend(main_red_items(&main_red_facts(state)));
    items.extend(pull_items(&pulls, &entries, now, &hosts));
    items.extend(superseded_pull_items(&superseded_pulls));
    items.extend(draft_items(&draft_facts(state), draft_idle_days(), now));
    items.extend(queue_items(&entries, &open, now));
    items.extend(runner_items(
        &state.gate_runners.snapshot(),
        &open_heads,
        waiting_review_count,
        building,
        reviewer_offline_thresholds(),
        now,
        &hosts,
    ));
    // A pull request the reviewer gave up on carries the review it posted:
    // one row says both, so the row the posture would add is dropped.
    drop_shadowed_pull_rows(&mut items);
    // Per repository, not the newest over all of them: two repositories with a
    // release staged are two things waiting on somebody.
    let staged = state
        .events
        .newest_of_kind_per_repo("release.staged")
        .unwrap_or_default();
    let stage_failed = state
        .events
        .newest_of_kind_per_repo("release.stage_failed")
        .unwrap_or_default();
    items.extend(release_items(
        &staged,
        &stage_failed,
        &production_facts(state),
        &hosts,
    ));
    // What `/releases` is showing: a red lane, a source its collector could
    // not read, a board that stopped arriving.
    items.extend(board_items(&board_facts(state), now));
    // The week's Scorecard numbers, as the weekly job recorded them: a week
    // that ended is not state the collectors can recompute, so this rule
    // reads the recorded digest.
    items.extend(digest_items(
        &state
            .events
            .query(&super::types::EventsQuery {
                kind: Some(crate::web::scorecard::weekly::DIGEST_KIND.to_string()),
                limit: Some(1),
                ..Default::default()
            })
            .unwrap_or_default(),
        now,
    ));
    // An acknowledged item is one somebody deliberately deferred: leave it
    // out, and out of the counts, until its date passes (see `acks.rs`).
    let hidden = state
        .attention_acks
        .hidden(now.timestamp_millis())
        .unwrap_or_default();
    items.retain(|item| !hidden.contains(&item.id));
    // One spelling per family, whatever a collector's source called it. An
    // item whose collector named only its repository takes that repository's
    // family, the one /repos shows for it, so `?family=` and `families` keep it.
    let mut repo_families: HashMap<String, Option<String>> = HashMap::new();
    for item in &mut items {
        let family = match (&item.family, &item.repo) {
            (Some(family), _) => Some(super::super::family::canonical(family)),
            (None, Some(repo)) => repo_families
                .entry(repo.clone())
                .or_insert_with(|| super::super::repositories::family_for_slug(state, repo))
                .clone(),
            (None, None) => None,
        };
        item.family_label = family.as_deref().map(super::super::family::label);
        item.family = family;
    }
    order(&mut items);
    let count = |severity| items.iter().filter(|i| i.severity == severity).count();
    AttentionResponse {
        schema_version: ATTENTION_SCHEMA,
        branch_discovery: super::super::stranded_branches::COVERAGE_NOTICE,
        generated_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        counts: Counts {
            critical: count(Severity::Critical),
            action: count(Severity::Action),
            watch: count(Severity::Watch),
        },
        items,
    }
}

/// The kept answer while it is fresh, else a fresh one, kept. Blocking: the
/// collectors run git and read every open pull request. The inbox route and
/// the work trace both read the inbox through this, so one computation serves
/// both.
pub(super) fn cached(state: &WebState, now: DateTime<Utc>) -> AttentionResponse {
    let lock = || {
        state
            .attention
            .inner
            .lock()
            .expect("attention cache mutex poisoned")
    };
    let kept = lock()
        .as_ref()
        .filter(|(at, _)| at.elapsed() < CACHE_FOR)
        .map(|(_, response)| response.clone());
    if let Some(response) = kept {
        return response;
    }
    let response = collect(state, now);
    *lock() = Some((Instant::now(), response.clone()));
    response
}

/// The last answer, reused for [`CACHE_FOR`]: the inbox badge polls this.
#[derive(Clone, Default)]
pub(crate) struct AttentionCache {
    inner: Arc<Mutex<Option<(Instant, AttentionResponse)>>>,
}

impl AttentionCache {
    /// Drop the kept answer, so the next read recomputes. Called when
    /// something that changes what the inbox hides is written.
    pub(crate) fn invalidate(&self) {
        *self.inner.lock().expect("attention cache mutex poisoned") = None;
    }
}

/// `GET /api/v1/attention` (admin-only by path, see `auth::admin_only_request`).
/// `GET /api/v1/attention?family=&severity=&kind=`: the family filter takes
/// either spelling of a family key, and `severity` and `kind` come from closed
/// sets, so a filter that could never match is refused rather than answered
/// with an empty inbox.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct AttentionQuery {
    pub family: Option<String>,
    pub severity: Option<String>,
    pub kind: Option<String>,
    #[serde(flatten)]
    pub paging: PageParams,
}

impl super::super::strict_query::StrictFields for AttentionQuery {
    const KEYS: &'static [&'static str] =
        &["family", "severity", "kind", "limit", "per_page", "page"];

    fn check_values(&self) -> Result<(), String> {
        use super::super::strict_query::filter_one_of;
        filter_one_of("severity", self.severity.as_ref(), &Severity::names())?;
        filter_one_of("kind", self.kind.as_ref(), KINDS)
    }
}

/// What a filtered inbox keeps. A field left `None` keeps every item.
#[derive(Debug, Default)]
pub(super) struct Filter {
    pub(super) family: Option<String>,
    pub(super) severity: Option<Severity>,
    pub(super) kind: Option<String>,
}

impl Filter {
    pub(super) fn keeps(&self, item: &Item) -> bool {
        self.family
            .as_ref()
            .is_none_or(|family| item.family.as_deref() == Some(family.as_str()))
            && self
                .severity
                .is_none_or(|severity| item.severity == severity)
            && self.kind.as_deref().is_none_or(|kind| item.kind == kind)
    }

    pub(super) fn any(&self) -> bool {
        self.family.is_some() || self.severity.is_some() || self.kind.is_some()
    }
}

/// One page of the inbox. `counts` and `total` stay over every item the filter
/// kept, so a page 2 reader still sees how much is waiting; `families` stays
/// over the whole inbox, filter or not, so a family strip counts every family
/// from one call; `page` echoes what was applied, the way every paged
/// `/api/v1` listing does (`docs/pagination.md`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct AttentionPage {
    pub schema_version: &'static str,
    pub generated_at: String,
    pub items: Vec<Item>,
    pub counts: Counts,
    /// Items the filter kept before paging.
    pub total: usize,
    /// Rows waiting in each family, over the whole inbox (see `summary.rs`).
    pub families: Vec<FamilyCounts>,
    pub page: PageInfo,
}

/// Cuts a collected inbox down to one page, next to the whole inbox's
/// `families` summary.
pub(super) fn one_page(
    response: AttentionResponse,
    page: Page,
    families: Vec<FamilyCounts>,
) -> AttentionPage {
    let (items, info) = page.apply(response.items);
    AttentionPage {
        schema_version: response.schema_version,
        generated_at: response.generated_at,
        items,
        counts: response.counts,
        total: info.total,
        families,
        page: info,
    }
}

/// The items a filter keeps, with the counts recounted for them.
pub(super) fn only_matching(response: &AttentionResponse, filter: &Filter) -> AttentionResponse {
    let items: Vec<Item> = response
        .items
        .iter()
        .filter(|item| filter.keeps(item))
        .cloned()
        .collect();
    let count = |severity| items.iter().filter(|i| i.severity == severity).count();
    AttentionResponse {
        schema_version: response.schema_version,
        branch_discovery: response.branch_discovery,
        generated_at: response.generated_at.clone(),
        counts: Counts {
            critical: count(Severity::Critical),
            action: count(Severity::Action),
            watch: count(Severity::Watch),
        },
        items,
    }
}

pub(crate) async fn attention(
    State(state): State<Arc<WebState>>,
    super::super::strict_query::StrictQuery(query): super::super::strict_query::StrictQuery<
        AttentionQuery,
    >,
) -> AxumResponse {
    let paging = match query.paging.page() {
        Ok(page) => page,
        Err(rejection) => return rejection.into_response(),
    };
    let family = match super::super::family::filter(&state, query.family.as_deref()) {
        Ok(family) => family,
        Err(response) => return *response,
    };
    let trimmed = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    // `check_values` already refused anything outside the closed sets.
    let filter = Filter {
        family,
        severity: trimmed(&query.severity).and_then(|name| Severity::parse(&name)),
        kind: trimmed(&query.kind),
    };
    let answer = move |response: AttentionResponse| {
        let families = family_summary(&response.items);
        let kept = if filter.any() {
            only_matching(&response, &filter)
        } else {
            response
        };
        Json(one_page(kept, paging, families)).into_response()
    };
    // The collectors run git and read every open pull request: keep them off
    // the async workers.
    let worker_state = state.clone();
    let response = tokio::task::spawn_blocking(move || cached(&worker_state, Utc::now())).await;
    match response {
        Ok(response) => answer(response),
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

/// Most severe first, then the longest-waiting first (undated items lead),
/// then by id so the order never depends on collection order.
pub(super) fn order(items: &mut [Item]) {
    items.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| since_instant(a).cmp(&since_instant(b)))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// When an item started waiting, as an instant. `since` strings come from
/// several sources in several RFC 3339 spellings (whole seconds with `Z`,
/// fractions, `+00:00`), so comparing the text orders them by punctuation.
fn since_instant(item: &Item) -> Option<DateTime<Utc>> {
    item.since.as_deref().and_then(parse_time)
}
