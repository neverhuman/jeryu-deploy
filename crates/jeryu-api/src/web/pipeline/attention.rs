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

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::super::WebState;
use super::super::merge_queue::QueueState;
use super::super::shift::FamilySnapshot;

pub(crate) mod acks;
mod flow;
mod hosts;
mod mirror;
mod pins;
mod work;

pub(crate) use acks::AckStore;
pub(crate) use flow::{
    DraftFacts, LatestDeployment, ProductionFacts, PullFacts, draft_items, pull_items, queue_items,
    release_items, runner_items,
};
pub(crate) use hosts::Hosts;
pub(crate) use mirror::{MirrorDrift, MirrorFailure, divergence_items, mirror_items};
pub(crate) use pins::pin_items;
pub(crate) use work::{shift_items, todo_items, worker_items};

pub(crate) const ATTENTION_SCHEMA: &str = "jeryu.attention/v1";
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

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Action {
    pub label: String,
    /// A copyable shell line when the step happens off-site.
    pub command: Option<String>,
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
pub(super) struct Draft<'a> {
    pub(super) id: String,
    pub(super) kind: &'static str,
    pub(super) severity: Severity,
    pub(super) title: String,
    pub(super) reason: String,
    pub(super) href: String,
    pub(super) label: &'a str,
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
            repo: None,
            pr: None,
            todo_id: None,
            sha: None,
            shift: None,
            href: self.href,
            action: match self.command {
                Some(shell) => Action {
                    label,
                    command: Some(shell.line),
                    run_in: Some(shell.run_in),
                },
                None => Action {
                    label,
                    command: None,
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

pub(super) fn todo_href(family: &str, id: &str) -> String {
    format!("/work/shift?family={family}&todo={id}")
}

pub(super) fn pull_href(repo: &str, number: u64) -> String {
    format!("/repos/jeryu/{repo}/pulls/{number}")
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
        &hosts,
    ));
    // An acknowledged item is one somebody deliberately deferred: leave it
    // out, and out of the counts, until its date passes (see `acks.rs`).
    let hidden = state
        .attention_acks
        .hidden(now.timestamp_millis())
        .unwrap_or_default();
    items.retain(|item| !hidden.contains(&item.id));
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
