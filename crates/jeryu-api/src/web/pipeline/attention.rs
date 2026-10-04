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

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use super::super::WebState;
use super::super::merge_queue::QueueState;
use super::super::shift::FamilySnapshot;

pub(crate) mod acks;
mod board;
mod flow;
mod hosts;
mod mirror;
mod pins;
#[cfg(test)]
pub(super) mod web_routes;
mod work;

pub(crate) use acks::AckStore;
pub(crate) use board::{BoardFacts, BoardLane, board_items};
pub(crate) use flow::{
    DraftFacts, LatestDeployment, ProductionFacts, PullFacts, draft_items, pull_items, queue_items,
    release_items, runner_items,
};
pub(crate) use hosts::Hosts;
pub(crate) use mirror::{MirrorDrift, MirrorFailure, divergence_items, mirror_items};
pub(crate) use pins::pin_items;
pub(crate) use work::{budget_items, shift_items, todo_items, worker_items};

/// `v1.1` added `action.api` and `v1.2` the optional `budget`: every earlier
/// field is unchanged, so a client written against `v1` reads a `v1.2` answer
/// as it always did.
pub(crate) const ATTENTION_SCHEMA: &str = "jeryu.attention/v1.2";
const CACHE_FOR: Duration = Duration::from_secs(10);
/// A claim whose lease died this long ago is stuck rather than between renewals.
pub(super) const STUCK_CLAIM_MINUTES: i64 = 10;
/// An untriaged todo a healthy worker has not picked up in this long is no
/// longer waiting on the next pass.
pub(super) const UNTRIAGED_MINUTES: i64 = 30;
/// A mergeable PR left open this long is waiting on somebody to merge it.
pub(super) const READY_TO_MERGE_MINUTES: i64 = 10;
pub(super) const QUEUE_LOOKBACK_HOURS: i64 = 24;
/// A queue entry still building after this long is no longer waiting for a
/// gate that is about to report.
pub(super) const QUEUE_STUCK_MINUTES: i64 = 30;
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
/// what `docs/attention.md` publishes: a kind absent here could only ever
/// match nothing, so a reader who filters by it hears so.
pub(crate) const KINDS: &[&str] = &[
    "deploy_failed",
    "gate_runner_down",
    "mirror_diverged",
    "mirror_failing",
    "pin_behind",
    "pr_awaiting_approval",
    "pr_changes_requested",
    "pr_checks_failing",
    "pr_draft_waiting",
    "pr_ready_to_merge",
    "queue_failed",
    "queue_refused",
    "queue_stuck",
    "release_board_problem",
    "release_stage_failed",
    "release_staged",
    "reviewer_stuck",
    "shift_budget_spent",
    "shift_stranded_work",
    "shift_without_pr",
    "todo_blocked",
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
    /// directory ("xbabe0, any directory"). Set exactly when `command` is, and
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
    format!("/repos/jeryu/{repo}")
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
    }
}

/// `POST /api/v1/shift/todos/:family/:id/action`: `release`, `done`, `park`
/// and the rest of one todo's transitions.
pub(super) fn todo_action_call(family: &str, id: &str, action: &str) -> ApiCall {
    ApiCall {
        method: "POST",
        path: format!("/api/v1/shift/todos/{family}/{id}/action"),
        body: Some(json!({ "action": action })),
    }
}

/// `POST /api/v1/repos/:id/pulls/:number/queue`: join the merge queue. The
/// repository is addressed by `owner/name`, which the API resolves.
pub(super) fn enqueue_call(repo: &str, number: u64) -> ApiCall {
    ApiCall {
        method: "POST",
        path: format!("/api/v1/repos/{repo}/pulls/{number}/queue"),
        body: None,
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
    }
}

/// `POST /api/v1/repos/:id/pulls/:number/ready`: a draft becomes a pull
/// request that reviews, gates and the queue act on.
pub(super) fn ready_for_review_call(repo: &str, number: u64) -> ApiCall {
    ApiCall {
        method: "POST",
        path: format!("/api/v1/repos/{repo}/pulls/{number}/ready"),
        body: None,
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
    let hosts = Hosts::from_env();
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
    let gave_up = state
        .events
        .query(&super::types::EventsQuery {
            kind: Some("pin.bump_failed".to_string()),
            needs_human: Some(true),
            limit: Some(20),
            ..Default::default()
        })
        .unwrap_or_default();
    items.extend(pin_items(
        &super::pins::snapshot(state).consumers,
        &gave_up,
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
    let pulls = open_pull_facts(state);
    let open: BTreeSet<(String, u64)> = pulls.iter().map(|p| (p.repo.clone(), p.number)).collect();
    let entries = state.merge_queue.entries(state, |_| true);
    let building = entries.iter().any(|e| e.state == QueueState::Building);
    items.extend(pull_items(&pulls, &entries, now));
    items.extend(draft_items(&draft_facts(state), draft_idle_days(), now));
    items.extend(queue_items(&entries, &open, now));
    items.extend(runner_items(
        &state.gate_runners.snapshot(),
        &open,
        building,
        now,
        &hosts,
    ));
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
    // An acknowledged item is one somebody deliberately deferred: leave it
    // out, and out of the counts, until its date passes (see `acks.rs`).
    let hidden = state
        .attention_acks
        .hidden(now.timestamp_millis())
        .unwrap_or_default();
    items.retain(|item| !hidden.contains(&item.id));
    // One spelling per family, whatever a collector's source called it.
    for item in &mut items {
        if let Some(family) = &item.family {
            let key = super::super::family::canonical(family);
            item.family_label = Some(super::super::family::label(&key));
            item.family = Some(key);
        }
    }
    order(&mut items);
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
}

impl super::super::strict_query::StrictFields for AttentionQuery {
    const KEYS: &'static [&'static str] = &["family", "severity", "kind"];

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
    let answer = |response: AttentionResponse| {
        if filter.any() {
            Json(only_matching(&response, &filter)).into_response()
        } else {
            Json(response).into_response()
        }
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
