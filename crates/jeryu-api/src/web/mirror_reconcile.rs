//! Periodic reconcile of every enrolled repository against its GitHub mirror.
//!
//! The merge-time push is one-shot: if it failed, or the forge moved a ref some
//! other way, GitHub silently falls behind. This loop closes that gap. Every
//! `JERYU_GITHUB_MIRROR_RECONCILE_MINUTES` (default 10, `0` disables) it walks
//! the enrolled repositories, fast-forwards GitHub when it is behind, mirrors
//! tags GitHub does not have, and — when GitHub holds commits the forge does
//! not — pushes nothing and raises an alarm naming those commits.
//!
//! It runs off the request path in its own task, one repository at a time, with
//! the mirror's bounded git calls, so a slow or unreachable GitHub costs wall
//! clock in this loop and nowhere else.
//!
//! The last result per repository is kept in memory ([`MirrorStateStore`]): the
//! repo page reads it for in-sync/behind/diverged and the attention inbox reads
//! it for the alarm. A restart simply relearns it on the next pass.

#![cfg(feature = "web")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use jeryu_core::{
    CheckConclusion, CheckRunOutput, CheckRunStatus, CreateCheckRunRequest, ForgeCore,
};
use serde::Serialize;

use super::WebState;
use crate::github_mirror::{
    MIRROR_CHECK_NAME, MIRROR_DIVERGED_CHECK_NAME, MirrorReconcile, MirrorSync, MirrorTagOutcome,
};

const INTERVAL_ENV: &str = "JERYU_GITHUB_MIRROR_RECONCILE_MINUTES";
const DEFAULT_INTERVAL_MINUTES: u64 = 10;
/// Most repositories one pass touches. The families enrolled today are far
/// under this; the cap only keeps a pass from growing without bound as more
/// enroll, and the next pass picks up where this one stopped.
const MAX_REPOS_PER_PASS: usize = 64;

/// One tag the forge and GitHub disagree about, as the UI shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TagDriftRow {
    pub(crate) tag: String,
    pub(crate) forge_oid: Option<String>,
    pub(crate) github_oid: Option<String>,
    pub(crate) detail: String,
}

/// What the newest reconcile found for one repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct MirrorRepoState {
    /// `owner/name` on the forge.
    pub(crate) repo: String,
    pub(crate) github_slug: String,
    pub(crate) branch: String,
    pub(crate) state: MirrorSync,
    pub(crate) forge_head: Option<String>,
    pub(crate) github_head: Option<String>,
    /// When this pass ran.
    pub(crate) checked_at: String,
    /// When a push last put GitHub level with the forge, as far as this
    /// process has seen.
    pub(crate) last_push_at: Option<String>,
    /// Commits GitHub holds that the forge does not, newest first.
    pub(crate) github_only_commits: Vec<String>,
    pub(crate) tags_pushed: Vec<String>,
    pub(crate) tag_drift: Vec<TagDriftRow>,
    pub(crate) error: Option<String>,
}

impl MirrorRepoState {
    /// True when only a person can settle what was found.
    pub(crate) fn needs_a_person(&self) -> bool {
        matches!(self.state, MirrorSync::Ahead | MirrorSync::Diverged) || !self.tag_drift.is_empty()
    }
}

/// The newest reconcile result per `owner/name`.
#[derive(Clone, Debug, Default)]
pub(crate) struct MirrorStateStore {
    inner: Arc<Mutex<BTreeMap<String, MirrorRepoState>>>,
}

impl MirrorStateStore {
    pub(crate) fn record(&self, state: MirrorRepoState) {
        let mut inner = self.inner.lock().expect("mirror state mutex poisoned");
        inner.insert(state.repo.clone(), state);
    }

    pub(crate) fn get(&self, owner: &str, name: &str) -> Option<MirrorRepoState> {
        let inner = self.inner.lock().expect("mirror state mutex poisoned");
        inner.get(&format!("{owner}/{name}")).cloned()
    }

    pub(crate) fn all(&self) -> Vec<MirrorRepoState> {
        let inner = self.inner.lock().expect("mirror state mutex poisoned");
        inner.values().cloned().collect()
    }
}

/// Spawn the reconcile loop when a cadence is configured and this server has a
/// mirror to reconcile.
pub(crate) fn spawn(state: Arc<WebState>) {
    let Some(interval) = interval_from(std::env::var(INTERVAL_ENV).ok().as_deref()) else {
        return;
    };
    // The kill switch and an empty manifest leave no target, so there is
    // nothing to reconcile and no loop to run.
    if !state
        .github
        .github_mirror()
        .is_some_and(|mirror| mirror.is_enabled())
    {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let state = state.clone();
            // The pass is blocking git work; keep it off the async runtime's
            // worker threads.
            let _ = tokio::task::spawn_blocking(move || reconcile_once(&state)).await;
        }
    });
}

/// Parse the configured cadence; `None` means the loop is off.
fn interval_from(raw: Option<&str>) -> Option<Duration> {
    let minutes = match raw.map(str::trim) {
        None | Some("") => DEFAULT_INTERVAL_MINUTES,
        Some(value) => value.parse::<u64>().ok()?,
    };
    (minutes > 0).then(|| Duration::from_secs(minutes * 60))
}

/// Reconcile every enrolled repository once and return what was found.
pub(crate) fn reconcile_once(state: &WebState) -> Vec<MirrorRepoState> {
    let Some(mirror) = state.github.github_mirror() else {
        return Vec::new();
    };
    let git_bin = state.repo_manager.config().git_bin.clone();
    let core = state.github.core();
    let mut found = Vec::new();
    for repo in core.list_repositories(None) {
        if found.len() >= MAX_REPOS_PER_PASS {
            break;
        }
        if repo.archived || mirror.target(&repo.owner, &repo.name).is_none() {
            continue;
        }
        let Ok(resolved) = state.repo_manager.resolve_parts(&repo.owner, &repo.name) else {
            continue;
        };
        let Some(report) = mirror.reconcile(&git_bin, &resolved.path, &repo.owner, &repo.name)
        else {
            continue;
        };
        let previous = state.mirror_state.get(&repo.owner, &repo.name);
        let recorded = record(
            core,
            &repo.owner,
            &repo.name,
            report,
            previous.as_ref(),
            Utc::now(),
        );
        state.mirror_state.record(recorded.clone());
        found.push(recorded);
    }
    found
}

/// Mirror the tags a push just created or moved, and record what came of it.
///
/// Called from the post-push bridge, so it is already off the acknowledgement
/// path. A repository that is not enrolled does nothing.
pub(crate) fn mirror_pushed_tags(state: &WebState, owner: &str, repo: &str, tags: &[String]) {
    if tags.is_empty() {
        return;
    }
    let Some(mirror) = state.github.github_mirror() else {
        return;
    };
    if mirror.target(owner, repo).is_none() {
        return;
    }
    let git_bin = state.repo_manager.config().git_bin.clone();
    let Ok(resolved) = state.repo_manager.resolve_parts(owner, repo) else {
        return;
    };
    let outcome = mirror.push_tags(&git_bin, &resolved.path, owner, repo, Some(tags));
    let core = state.github.core();
    let head = default_branch_head(state, owner, repo);
    if !outcome.pushed.is_empty()
        && let Some(sha) = head.clone()
    {
        let names = outcome.pushed.join(", ");
        write_check(
            core,
            owner,
            repo,
            MirrorCheck {
                name: MIRROR_CHECK_NAME,
                head_sha: &sha,
                conclusion: CheckConclusion::Success,
                title: "tag mirror",
                summary: format!("pushed {names} to GitHub"),
            },
        );
    }
    if !outcome.drift.is_empty()
        && let Some(sha) = head
    {
        write_check(
            core,
            owner,
            repo,
            MirrorCheck {
                name: MIRROR_DIVERGED_CHECK_NAME,
                head_sha: &sha,
                conclusion: CheckConclusion::Failure,
                title: "GitHub mirror drift",
                summary: tag_drift_summary(&outcome),
            },
        );
    }
    // Fold the tag outcome into what the repo page shows, without claiming to
    // know the branch state this push did not look at.
    if let Some(mut held) = state.mirror_state.get(owner, repo) {
        merge_tags(&mut held, &outcome);
        held.checked_at = Utc::now().to_rfc3339();
        state.mirror_state.record(held);
    }
}

fn merge_tags(held: &mut MirrorRepoState, outcome: &MirrorTagOutcome) {
    for tag in &outcome.pushed {
        if !held.tags_pushed.contains(tag) {
            held.tags_pushed.push(tag.clone());
        }
        held.tag_drift.retain(|row| &row.tag != tag);
    }
    for drift in &outcome.drift {
        held.tag_drift.retain(|row| row.tag != drift.tag);
        held.tag_drift.push(TagDriftRow {
            tag: drift.tag.clone(),
            forge_oid: drift.forge_oid.clone(),
            github_oid: drift.github_oid.clone(),
            detail: drift.detail.clone(),
        });
    }
}

/// Turn one reconcile report into the state the UI shows, recording a check-run
/// for a catch-up and for a change in whether a person is needed.
///
/// The check-runs are written on change only: this loop runs every ten minutes
/// forever, and an unchanging posture must not grow the check-run history.
fn record(
    core: &ForgeCore,
    owner: &str,
    name: &str,
    report: MirrorReconcile,
    previous: Option<&MirrorRepoState>,
    now: DateTime<Utc>,
) -> MirrorRepoState {
    let mut recorded = MirrorRepoState {
        repo: format!("{owner}/{name}"),
        github_slug: report.github_slug.clone(),
        branch: report.branch.clone(),
        state: report.state,
        forge_head: report.forge_head.clone(),
        github_head: report.github_head.clone(),
        checked_at: now.to_rfc3339(),
        last_push_at: previous.and_then(|held| held.last_push_at.clone()),
        github_only_commits: report.github_only_commits.clone(),
        tags_pushed: report.tags.pushed.clone(),
        tag_drift: report
            .tags
            .drift
            .iter()
            .map(|drift| TagDriftRow {
                tag: drift.tag.clone(),
                forge_oid: drift.forge_oid.clone(),
                github_oid: drift.github_oid.clone(),
                detail: drift.detail.clone(),
            })
            .collect(),
        error: report.error.clone().or_else(|| report.tags.error.clone()),
    };
    let pushed_anything = report.caught_up || !report.tags.pushed.is_empty();
    if pushed_anything {
        recorded.last_push_at = Some(now.to_rfc3339());
    }
    let head = report
        .forge_head
        .clone()
        .or_else(|| report.github_head.clone());
    let Some(head) = head else {
        return recorded;
    };
    if pushed_anything {
        write_check(
            core,
            owner,
            name,
            MirrorCheck {
                name: MIRROR_CHECK_NAME,
                head_sha: &head,
                conclusion: CheckConclusion::Success,
                title: "GitHub mirror reconcile",
                summary: catch_up_summary(&report),
            },
        );
    }
    let needed_before = previous.is_some_and(MirrorRepoState::needs_a_person);
    if recorded.needs_a_person() != needed_before || previous.is_none() {
        let (conclusion, summary) = if recorded.needs_a_person() {
            (CheckConclusion::Failure, divergence_summary(&report))
        } else {
            (
                CheckConclusion::Success,
                format!("GitHub {} matches the forge", report.github_slug),
            )
        };
        if recorded.needs_a_person() || needed_before {
            write_check(
                core,
                owner,
                name,
                MirrorCheck {
                    name: MIRROR_DIVERGED_CHECK_NAME,
                    head_sha: &head,
                    conclusion,
                    title: "GitHub mirror divergence",
                    summary,
                },
            );
        }
    }
    recorded
}

fn catch_up_summary(report: &MirrorReconcile) -> String {
    let mut parts = Vec::new();
    if report.caught_up {
        parts.push(format!(
            "fast-forwarded GitHub {} {} to {}",
            report.github_slug,
            report.branch,
            report.forge_head.as_deref().unwrap_or("the forge tip")
        ));
    }
    if !report.tags.pushed.is_empty() {
        parts.push(format!("pushed tags {}", report.tags.pushed.join(", ")));
    }
    parts.join("; ")
}

fn divergence_summary(report: &MirrorReconcile) -> String {
    let mut parts = Vec::new();
    match report.state {
        MirrorSync::Ahead => parts.push(format!(
            "GitHub {} is AHEAD of the forge at {}: {}",
            report.github_slug,
            report.github_head.as_deref().unwrap_or("an unknown commit"),
            named_commits(&report.github_only_commits)
        )),
        MirrorSync::Diverged => parts.push(format!(
            "GitHub {} has diverged from the forge at {}: {}",
            report.github_slug,
            report.github_head.as_deref().unwrap_or("an unknown commit"),
            named_commits(&report.github_only_commits)
        )),
        _ => {}
    }
    if !report.tags.drift.is_empty() {
        parts.push(
            report
                .tags
                .drift
                .iter()
                .map(|drift| drift.detail.clone())
                .collect::<Vec<_>>()
                .join("; "),
        );
    }
    parts.push(
        "The forge is the truth and nothing was forced; a person decides what happens to these."
            .to_string(),
    );
    parts.join(". ")
}

fn tag_drift_summary(outcome: &MirrorTagOutcome) -> String {
    let detail = outcome
        .drift
        .iter()
        .map(|drift| drift.detail.clone())
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "{detail}. The mirror does not move or delete a GitHub tag; a person decides what happens \
         to these."
    )
}

fn named_commits(commits: &[String]) -> String {
    if commits.is_empty() {
        return "no commit could be listed".to_string();
    }
    commits.join(", ")
}

/// One bookkeeping check-run: which check, on which commit, and what it says.
struct MirrorCheck<'a> {
    name: &'a str,
    head_sha: &'a str,
    conclusion: CheckConclusion,
    title: &'a str,
    summary: String,
}

fn write_check(core: &ForgeCore, owner: &str, repo: &str, check: MirrorCheck<'_>) {
    let _ = core.create_check_run(
        owner,
        repo,
        CreateCheckRunRequest {
            name: check.name.to_string(),
            head_sha: check.head_sha.to_string(),
            status: Some(CheckRunStatus::Completed),
            conclusion: Some(check.conclusion),
            details_url: None,
            output: Some(CheckRunOutput {
                title: check.title.to_string(),
                summary: check.summary,
                text: None,
            }),
        },
    );
}

fn default_branch_head(state: &WebState, owner: &str, name: &str) -> Option<String> {
    let repo = state.github.core().get_repository(owner, name).ok()?;
    let bare = state.repo_manager.open_parts(owner, name).ok()?;
    jeryu_gitd::refs::RefService::new((*state.repo_manager).clone())
        .resolve_commit(&bare, &format!("refs/heads/{}", repo.default_branch))
        .ok()
        .flatten()
}

/// The repositories whose mirror needs a person, for the attention inbox.
pub(crate) fn drift_rows(state: &WebState) -> Vec<super::pipeline::attention::MirrorDrift> {
    state
        .mirror_state
        .all()
        .into_iter()
        .filter(MirrorRepoState::needs_a_person)
        .map(|held| super::pipeline::attention::MirrorDrift {
            repo: held.repo,
            github_slug: held.github_slug,
            branch_state: match held.state {
                MirrorSync::Ahead => Some("ahead of the forge".to_string()),
                MirrorSync::Diverged => Some("diverged from the forge".to_string()),
                _ => None,
            },
            github_head: held.github_head,
            github_only_commits: held.github_only_commits,
            tag_drift: held.tag_drift.into_iter().map(|row| row.detail).collect(),
            since: DateTime::parse_from_rfc3339(&held.checked_at)
                .ok()
                .map(|at| at.with_timezone(&Utc)),
        })
        .collect()
}

#[cfg(test)]
#[path = "mirror_reconcile_tests.rs"]
mod tests;
