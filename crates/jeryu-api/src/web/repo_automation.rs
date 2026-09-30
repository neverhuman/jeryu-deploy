//! `GET /api/v1/repos/:id/automation`: everything that acts on one repository,
//! and every mirror it is copied to.
//!
//! A repository page used to say what the code is and nothing about what runs
//! over it. The facts were all on the forge already, each behind a different
//! route: the checks under the GitHub-shaped Actions edge, the required
//! contexts under branch protection, the reviewer and merger under `/runners`,
//! the mirror under the reconcile loop, the grants under the admin routes. A
//! reader asking "what happens when this pull request merges?" had to visit
//! all five and join them by hand.
//!
//! This route does the join once:
//!
//! * `checks` — one row per check name the repository has ever reported, with
//!   the newest run's conclusion and its link, plus a row for every required
//!   context that has never reported (`state: "missing"`).
//! * `actors` — the reviewer, the merger, the gate runners and the deployers,
//!   each with the grant it needs and whether it holds it. A merger with no
//!   write grant is the whole point: its merges answer 403, and the page can
//!   finally say so before someone waits a day for a merge that cannot happen.
//! * `mirrors` — where the repository is copied to, which refs, the sha the
//!   mirror holds, when it was last level with the forge, and the last error.
//! * `grants` — who may read, write and administer the repository, for a
//!   caller who may administer it (core's own rule for listing grants).
//!
//! External actors report through the heartbeat contract they already use
//! (`POST /api/v1/runners/heartbeat`): a deploy timer sends the `deploy` label
//! and a `last` naming the target it deployed to, the sha it deployed, and
//! whether that worked. Deployments recorded through the GitHub-shaped
//! `POST /repos/{owner}/{repo}/deployments` surface here too, so a deployer
//! that writes the forge's own deployment trail needs no heartbeat at all.

use jeryu_core::{CheckRun, CheckRunStatus, RepoAccessLevel, Repository};

use super::control_plane::{
    GateRunnerRecord, gate_runner_target, is_automation, is_deploy, is_online, is_reviewer,
};
use super::*;

/// The identity that reviews pull requests here, `JERYU_REVIEW_IDENTITY`.
const DEFAULT_REVIEW_IDENTITY: &str = "pragent";

/// One check name and its newest run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckSummary {
    pub name: String,
    /// The default branch's protection rule names this context.
    pub required: bool,
    /// `reported` when a run exists, `missing` when a required context has
    /// never reported on this repository.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_conclusion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details_url: Option<String>,
}

/// Whether an identity holds the access its job needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActorGrant {
    /// `read`, `write` or `admin`: what this actor's job needs.
    pub required: &'static str,
    pub present: bool,
    /// What the identity actually holds; absent when it holds nothing, or when
    /// there is no such account on this forge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held: Option<String>,
    /// One sentence naming what the missing grant costs, for the page banner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// The last thing an actor did to this repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActorRun {
    /// `success`, `failure`, `approve`, `deployed`, … in the actor's own words.
    pub conclusion: String,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// Where a deployer put it (`pages-preview`, `staging`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One thing that acts on the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AutomationActor {
    /// `reviewer`, `merger`, `gate-runner` or `deployer`.
    pub kind: &'static str,
    /// The login, runner id or environment name that identifies this actor.
    pub identity: String,
    /// One line saying what it does, so the page needs no glossary.
    pub role: String,
    /// `online`, `offline`, or `configured` for an identity that acts on
    /// demand rather than on a timer.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant: Option<ActorGrant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<ActorRun>,
}

/// One place this repository is copied to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MirrorSummary {
    /// Where the copy lives, as a reader would open it.
    pub target: String,
    /// `push` (the forge writes the target) or `pull`.
    pub direction: &'static str,
    /// Which refs travel, e.g. `refs/heads/main` and tags.
    pub refs: Vec<String>,
    /// `in_sync`, `behind`, `ahead`, `diverged` or `unknown`.
    pub state: String,
    /// True while the target lacks commits the forge holds.
    pub behind: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forge_head: Option<String>,
    /// The sha the target holds: the last thing successfully pushed to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_pushed_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_pushed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// One grant on the repository, as the page lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GrantSummary {
    pub login: String,
    pub access: String,
    pub granted_by: String,
    pub granted_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AutomationView {
    /// `owner/name`.
    pub repo: String,
    pub default_branch: String,
    pub checks: Vec<CheckSummary>,
    /// Contexts the default branch's protection rule requires, in its order.
    pub required_contexts: Vec<String>,
    pub actors: Vec<AutomationActor>,
    pub mirrors: Vec<MirrorSummary>,
    /// Listed for a caller who may administer the repository; empty otherwise.
    pub grants: Vec<GrantSummary>,
    /// True when the caller was allowed to read `grants`, so the page can tell
    /// "nobody holds a grant" from "you may not see them".
    pub grants_visible: bool,
    /// One sentence per thing a person has to fix (a missing merge grant, a
    /// mirror that is not level), newest concern first.
    pub warnings: Vec<String>,
}

pub(super) async fn show(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    let Some(repo) = super::repositories::find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    Json(automation_view(&state, &repo, &account)).into_response()
}

pub(crate) fn automation_view(
    state: &WebState,
    repo: &Repository,
    account: &AccountSummary,
) -> AutomationView {
    let full = format!("{}/{}", repo.owner, repo.name);
    let required = required_contexts(state, repo);
    let checks = check_summaries(state, repo, &required);
    let actors = actors(state, repo);
    let mirrors = mirrors(state, repo);
    let may_admin = account.role == UserRole::Admin
        || state
            .core
            .user_can_admin_repo(&account.login, &repo.owner, &repo.name);
    let grants = if may_admin {
        state
            .core
            .list_repo_access(&repo.owner, &repo.name)
            .into_iter()
            .map(|grant| GrantSummary {
                login: grant.login,
                access: access_word(grant.access).to_string(),
                granted_by: grant.granted_by,
                granted_at: grant.granted_at.to_rfc3339(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let warnings = warnings(&actors, &mirrors);
    AutomationView {
        repo: full,
        default_branch: repo.default_branch.clone(),
        checks,
        required_contexts: required,
        actors,
        mirrors,
        grants,
        grants_visible: may_admin,
        warnings,
    }
}

fn access_word(access: RepoAccessLevel) -> &'static str {
    match access {
        RepoAccessLevel::Read => "read",
        RepoAccessLevel::Write => "write",
        RepoAccessLevel::Admin => "admin",
    }
}

/// The contexts the default branch's protection rule requires. A repository
/// with no rule requires nothing, which is a fact worth showing, not an error.
fn required_contexts(state: &WebState, repo: &Repository) -> Vec<String> {
    state
        .core
        .get_branch_protection(&repo.owner, &repo.name, &repo.default_branch)
        .map(|rule| rule.required_status_checks)
        .unwrap_or_default()
}

/// One row per check name, newest run first by name, plus a `missing` row for
/// every required context that has never reported.
fn check_summaries(state: &WebState, repo: &Repository, required: &[String]) -> Vec<CheckSummary> {
    let runs = state
        .core
        .list_check_runs(&repo.owner, &repo.name, None)
        .map(|list| list.check_runs)
        .unwrap_or_default();
    let mut newest: BTreeMap<String, CheckRun> = BTreeMap::new();
    for run in runs {
        match newest.entry(run.name.clone()) {
            std::collections::btree_map::Entry::Occupied(mut held) => {
                if run_time(&run) >= run_time(held.get()) {
                    held.insert(run);
                }
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(run);
            }
        }
    }
    let mut summaries: Vec<CheckSummary> = newest
        .into_values()
        .map(|run| CheckSummary {
            required: required.contains(&run.name),
            state: "reported",
            last_conclusion: Some(conclusion_word(&run)),
            last_run_at: Some(run_time(&run).to_rfc3339()),
            last_head_sha: Some(run.head_sha.clone()),
            details_url: run.details_url.clone(),
            name: run.name,
        })
        .collect();
    for context in required {
        if !summaries.iter().any(|check| &check.name == context) {
            summaries.push(CheckSummary {
                name: context.clone(),
                required: true,
                state: "missing",
                last_conclusion: None,
                last_run_at: None,
                last_head_sha: None,
                details_url: None,
            });
        }
    }
    summaries.sort_by(|a, b| b.required.cmp(&a.required).then(a.name.cmp(&b.name)));
    summaries
}

fn run_time(run: &CheckRun) -> chrono::DateTime<chrono::Utc> {
    run.completed_at.unwrap_or(run.started_at)
}

/// The run's conclusion, or its status while it has none yet.
fn conclusion_word(run: &CheckRun) -> String {
    match &run.conclusion {
        Some(conclusion) => jeryu_core::check_conclusion_wire_value(conclusion).to_string(),
        None => match run.status {
            CheckRunStatus::Queued => "queued".to_string(),
            CheckRunStatus::InProgress => "in_progress".to_string(),
            CheckRunStatus::Completed => "completed".to_string(),
        },
    }
}

fn review_identity() -> String {
    std::env::var("JERYU_REVIEW_IDENTITY")
        .ok()
        .map(|login| login.trim().to_string())
        .filter(|login| !login.is_empty())
        .unwrap_or_else(|| DEFAULT_REVIEW_IDENTITY.to_string())
}

/// The grant `identity` needs to do `job`, and whether it holds it. `None` when
/// this forge has no such account: there is no actor to warn about.
fn actor_grant(
    state: &WebState,
    repo: &Repository,
    identity: &str,
    required: &'static str,
    cost: &str,
) -> Option<ActorGrant> {
    let account = state.core.get_account(identity).ok()?;
    let held = if account.role == UserRole::Admin {
        Some(RepoAccessLevel::Admin)
    } else {
        state
            .core
            .repo_access_for(identity, &repo.owner, &repo.name)
    };
    let enough = held.is_some_and(|level| match required {
        "admin" => level.allows_admin(),
        "write" => level.allows_write(),
        _ => true,
    });
    Some(ActorGrant {
        required,
        present: enough,
        held: held.map(|level| access_word(level).to_string()),
        warning: (!enough).then(|| {
            format!(
                "{identity} has no {required} grant on {}/{}; {cost}",
                repo.owner, repo.name
            )
        }),
    })
}

fn actors(state: &WebState, repo: &Repository) -> Vec<AutomationActor> {
    let full = format!("{}/{}", repo.owner, repo.name);
    let mut actors = Vec::new();
    let reviewer = review_identity();
    if let Some(grant) = actor_grant(state, repo, &reviewer, "write", "its reviews answer 403") {
        actors.push(AutomationActor {
            kind: "reviewer",
            role: "reviews pull requests and approves the head it read".to_string(),
            state: "configured",
            grant: Some(grant),
            last_run: None,
            identity: reviewer,
        });
    }
    let merger = super::merge_attempts::merge_identity();
    if let Some(grant) = actor_grant(state, repo, &merger, "write", "its merges answer 403") {
        actors.push(AutomationActor {
            kind: "merger",
            role: "merges approved pull requests".to_string(),
            state: "configured",
            grant: Some(grant),
            last_run: None,
            identity: merger,
        });
    }
    let now = chrono::Utc::now();
    for record in state.gate_runners.snapshot() {
        if !touches(&record, &full) {
            continue;
        }
        actors.push(runner_actor(&record, now));
    }
    actors.extend(deployment_actors(state, repo));
    actors
}

/// A heartbeat is about this repository when its running task or its last
/// result names it.
fn touches(record: &GateRunnerRecord, full: &str) -> bool {
    let beat = &record.heartbeat;
    beat.current.as_ref().is_some_and(|task| task.repo == full)
        || beat.last.as_ref().is_some_and(|last| last.repo == full)
}

fn runner_actor(record: &GateRunnerRecord, now: chrono::DateTime<chrono::Utc>) -> AutomationActor {
    let beat = &record.heartbeat;
    let (kind, role) = if is_deploy(beat) {
        ("deployer", "deploys the forge branch to its target")
    } else if is_reviewer(beat) {
        ("reviewer", "reviews pull requests off the forge")
    } else if is_automation(beat) {
        ("automation", "a background timer acting on this repository")
    } else {
        (
            "gate-runner",
            "runs the required gate on pull request heads",
        )
    };
    AutomationActor {
        kind,
        identity: beat.runner_id.clone(),
        role: role.to_string(),
        state: if is_online(record, now) {
            "online"
        } else {
            "offline"
        },
        grant: None,
        last_run: beat.last.as_ref().map(|last| ActorRun {
            conclusion: last.conclusion.clone(),
            at: last.finished_at.to_rfc3339(),
            sha: Some(last.sha.clone()),
            pr: last.pr,
            target: gate_runner_target(last).map(str::to_string),
            detail: last.reason.clone(),
        }),
    }
}

/// One actor per environment the forge has a deployment trail for: whoever
/// recorded the newest deployment is the deployer of that target.
fn deployment_actors(state: &WebState, repo: &Repository) -> Vec<AutomationActor> {
    let Ok(environments) = state.core.deployment_environments(&repo.owner, &repo.name) else {
        return Vec::new();
    };
    environments
        .into_iter()
        .filter_map(|environment| {
            let latest = environment.latest?;
            Some(AutomationActor {
                kind: "deployer",
                identity: latest.deployment.creator.clone(),
                role: format!("deploys this repository to {}", environment.environment),
                state: "configured",
                grant: None,
                last_run: Some(ActorRun {
                    conclusion: latest
                        .status
                        .as_ref()
                        .map(|status| status.state.as_str().to_string())
                        .unwrap_or_else(|| "pending".to_string()),
                    at: latest
                        .status
                        .as_ref()
                        .map_or(latest.deployment.created_at, |status| status.created_at)
                        .to_rfc3339(),
                    sha: Some(latest.deployment.sha.clone()),
                    pr: None,
                    target: Some(environment.environment.clone()),
                    detail: latest
                        .status
                        .as_ref()
                        .and_then(|status| status.description.clone()),
                }),
            })
        })
        .collect()
}

fn mirrors(state: &WebState, repo: &Repository) -> Vec<MirrorSummary> {
    let Some(target) = state
        .github
        .github_mirror()
        .and_then(|mirror| mirror.target(&repo.owner, &repo.name).cloned())
    else {
        return Vec::new();
    };
    let live = state.mirror_state.get(&repo.owner, &repo.name);
    let attempts = state
        .core
        .list_check_runs(&repo.owner, &repo.name, None)
        .map(|list| list.check_runs)
        .unwrap_or_default();
    let posture = super::repositories::mirror_status(&attempts);
    let sync = live
        .as_ref()
        .map(|state| mirror_word(state.state).to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let summary = MirrorSummary {
        target: format!("https://github.com/{}", target.github_slug),
        direction: "push",
        refs: vec![
            format!("refs/heads/{}", target.branch),
            "refs/tags/*".to_string(),
        ],
        behind: sync == "behind" || sync == "diverged",
        state: sync,
        forge_head: live.as_ref().and_then(|state| state.forge_head.clone()),
        last_pushed_sha: live.as_ref().and_then(|state| state.github_head.clone()),
        last_pushed_at: live
            .as_ref()
            .and_then(|state| state.last_push_at.clone())
            .or_else(|| posture.as_ref().and_then(|p| p.last_success_at.clone())),
        last_checked_at: live
            .as_ref()
            .map(|state| state.checked_at.clone())
            .or_else(|| posture.as_ref().and_then(|p| p.last_attempt_at.clone())),
        last_error: live
            .as_ref()
            .and_then(|state| state.error.clone())
            .or_else(|| {
                posture
                    .as_ref()
                    .filter(|p| !p.last_attempt_ok)
                    .and_then(|p| p.last_attempt_conclusion.clone())
                    .map(|conclusion| format!("last push attempt: {conclusion}"))
            }),
    };
    vec![summary]
}

fn mirror_word(sync: crate::github_mirror::MirrorSync) -> &'static str {
    use crate::github_mirror::MirrorSync;
    match sync {
        MirrorSync::InSync => "in_sync",
        MirrorSync::Behind => "behind",
        MirrorSync::Ahead => "ahead",
        MirrorSync::Diverged => "diverged",
        MirrorSync::Unknown => "unknown",
    }
}

/// One sentence per thing only a person can settle.
fn warnings(actors: &[AutomationActor], mirrors: &[MirrorSummary]) -> Vec<String> {
    let mut warnings: Vec<String> = actors
        .iter()
        .filter_map(|actor| actor.grant.as_ref().and_then(|grant| grant.warning.clone()))
        .collect();
    for mirror in mirrors {
        if mirror.behind {
            warnings.push(format!(
                "{} is {} the forge; its last pushed sha is not the forge head",
                mirror.target, mirror.state
            ));
        }
        if let Some(error) = &mirror.last_error {
            warnings.push(format!("{}: {error}", mirror.target));
        }
    }
    warnings
}
