//! Bringing a mirror's pull requests to the forge, and reporting back.
//!
//! The GitHub repositories are read-only copies, so an outside contribution
//! arrives as a pull request there and has to be carried here to be gated,
//! reviewed and merged. For each pull request on an enrolled mirror this
//! module:
//!
//! 1. fetches its commits into the forge repository as `contrib/gh-<number>`,
//!    unchanged, so every original author and committer survives;
//! 2. opens (or updates, after a force-push) a forge pull request against the
//!    mirror's branch, marked `contributed` so the maintainer hold applies;
//! 3. posts the forge gate's answer back as a commit status on the GitHub head
//!    and the maintainer review summary as a comment;
//! 4. closes the GitHub pull request once the forge has settled it — "merged
//!    as `<sha>`" when the merge has reached GitHub through the mirror push
//!    (GitHub marks a pull request merged itself when its commits land on the
//!    branch), or the reason when the forge closed it unmerged.
//!
//! A pull request on a repository nobody enrolled is ignored, and so is every
//! repository while the importer is off — which is the default. Dependabot is
//! not a special case: its pull requests take exactly this path.
//!
//! Everything a contributor wrote (title, body, branch name, login) is data
//! here. It is copied into the forge pull request and never interpreted, and
//! nothing in it chooses a repository, a branch or a command.
//!
//! A webhook delivery is only a nudge: `POST /api/v1/github-mirror/pull-events`
//! takes the repository and the number from the payload and then reads the
//! pull request from the GitHub API, so a forged payload cannot hand the forge
//! a head commit of its own. The poll ([`spawn`]) is the fallback for a
//! delivery that never arrives.

#![cfg(feature = "web")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use jeryu_core::{
    AccountSummary, CheckConclusion, CheckRunStatus, CreatePullRequestRequest, PullRequest,
    PullRequestState, ReviewState, UpdatePullRequestRequest, UserRole, effective_reviews_for_head,
};
use serde::{Deserialize, Serialize};

use super::pull_origin::{self, PullOrigin};
use super::{WebState, api_error};
use crate::github_pull_import::{
    GATE_STATUS_CONTEXT, GithubPull, GithubPullState, GithubStatus, GithubStatusState,
    PullImportTarget, changed_files, contains_commit, contrib_branch, contrib_number,
    fetch_contribution,
};

/// How often the poll looks for pull requests a webhook delivery missed.
const INTERVAL_ENV: &str = "JERYU_GITHUB_PULL_IMPORT_MINUTES";
const DEFAULT_INTERVAL_MINUTES: u64 = 5;

/// Most repositories one pass walks, so a pass cannot grow without bound as
/// more enrol; a rotating start gives later repositories their next pass.
const MAX_REPOS_PER_PASS: usize = 64;

/// The forge login an imported pull request is authored by, derived from the
/// GitHub login so attribution survives and the importer is never the author
/// of what it marks.
fn forge_author(github_login: &str) -> String {
    let sanitized: String = github_login
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "gh-contributor".to_string()
    } else {
        format!("gh-{trimmed}")
    }
}

/// What one pass did with one GitHub pull request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImportAction {
    /// A forge pull request was opened for it.
    Opened,
    /// An existing forge pull request followed a new head.
    Updated,
    /// Already current; only the report back may have moved.
    Unchanged,
    /// The forge merged it and the mirror push carried it to GitHub.
    ClosedAsMerged,
    /// The forge closed it unmerged.
    ClosedOnForge,
    /// Something failed; `detail` says what.
    Failed,
}

/// One GitHub pull request and what the importer did with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ImportOutcome {
    /// `owner/name` on the forge, empty when nothing was enrolled.
    pub(crate) repo: String,
    pub(crate) github_slug: String,
    pub(crate) github_number: u64,
    pub(crate) forge_number: Option<u64>,
    pub(crate) action: ImportAction,
    pub(crate) detail: Option<String>,
}

impl ImportOutcome {
    fn failed(repo: &str, slug: &str, number: u64, detail: String) -> Self {
        Self {
            repo: repo.to_string(),
            github_slug: slug.to_string(),
            github_number: number,
            forge_number: None,
            action: ImportAction::Failed,
            detail: Some(detail),
        }
    }
}

/// What the importer last told GitHub about one pull request, so a pass that
/// changes nothing does not post the same comment again. A restart relearns
/// it, which costs at most one repeated comment per pull request.
#[derive(Clone, Debug, Default)]
pub(crate) struct ImportReportStore {
    inner: Arc<Mutex<BTreeMap<String, Reported>>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Reported {
    gate: Option<String>,
    dco: Option<String>,
    review: Option<String>,
    settled: bool,
}

impl ImportReportStore {
    fn get(&self, slug: &str, number: u64) -> Reported {
        self.inner
            .lock()
            .expect("import report mutex poisoned")
            .get(&format!("{slug}#{number}"))
            .cloned()
            .unwrap_or_default()
    }

    fn put(&self, slug: &str, number: u64, reported: Reported) {
        self.inner
            .lock()
            .expect("import report mutex poisoned")
            .insert(format!("{slug}#{number}"), reported);
    }
}

/// Spawn the poll when the importer is on and a cadence is configured.
pub(crate) fn spawn(state: Arc<WebState>) {
    let Some(interval) = interval_from(std::env::var(INTERVAL_ENV).ok().as_deref()) else {
        return;
    };
    if !state.pull_import.is_enabled() || state.github_api.is_none() {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let state = state.clone();
            // Network and git work; keep it off the async runtime's workers.
            let _ = tokio::task::spawn_blocking(move || import_once(&state)).await;
        }
    });
}

/// Parse the configured cadence; `None` means the poll is off.
fn interval_from(raw: Option<&str>) -> Option<Duration> {
    let minutes = match raw.map(str::trim) {
        None | Some("") => DEFAULT_INTERVAL_MINUTES,
        Some(value) => value.parse::<u64>().ok()?,
    };
    (minutes > 0).then(|| Duration::from_secs(minutes * 60))
}

/// Walk every enrolled repository once: import what is open on its mirror and
/// settle what the forge has finished.
pub(crate) fn import_once(state: &WebState) -> Vec<ImportOutcome> {
    // With no maintainer to say ok-to-test or approve, an imported contribution
    // could never be gated or merged, so the importer stays off.
    if state.pull_maintainers.is_empty() {
        return Vec::new();
    }
    let Some(api) = state.github_api.clone() else {
        return Vec::new();
    };
    let mut outcomes = Vec::new();
    let mut enrolled: Vec<_> = state
        .github
        .core()
        .list_repositories(None)
        .into_iter()
        .filter(|repo| state.pull_import.target(&repo.owner, &repo.name).is_some())
        .collect();
    enrolled.sort_by_key(|repo| (repo.owner.clone(), repo.name.clone()));
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if !enrolled.is_empty() {
        let offset = NEXT.fetch_add(MAX_REPOS_PER_PASS, std::sync::atomic::Ordering::Relaxed)
            % enrolled.len();
        enrolled.rotate_left(offset);
    }
    for repo in enrolled.into_iter().take(MAX_REPOS_PER_PASS) {
        let Some(target) = state.pull_import.target(&repo.owner, &repo.name) else {
            continue;
        };
        match api.list_open_pulls(&target.github_slug) {
            Ok(pulls) => {
                for pull in pulls {
                    outcomes.push(import_pull(
                        state,
                        api.as_ref(),
                        &repo.owner,
                        &repo.name,
                        target,
                        &pull,
                    ));
                }
            }
            Err(error) => outcomes.push(ImportOutcome::failed(
                &format!("{}/{}", repo.owner, repo.name),
                &target.github_slug,
                0,
                format!("could not list pull requests: {error}"),
            )),
        }
        outcomes.extend(settle_finished(
            state,
            api.as_ref(),
            &repo.owner,
            &repo.name,
            target,
        ));
    }
    outcomes
}

/// Import one GitHub pull request and report the forge's answer back.
fn import_pull(
    state: &WebState,
    api: &dyn crate::github_pull_import::GithubApi,
    owner: &str,
    name: &str,
    target: &PullImportTarget,
    pull: &GithubPull,
) -> ImportOutcome {
    let repo = format!("{owner}/{name}");
    // A pull request closed on GitHub keeps whatever the forge already has:
    // the forge decides what happens to a contribution, not the mirror.
    if pull.state == GithubPullState::Closed {
        return ImportOutcome {
            repo,
            github_slug: target.github_slug.clone(),
            github_number: pull.number,
            forge_number: existing_pull(state, owner, name, pull.number).map(|pr| pr.number),
            action: ImportAction::Unchanged,
            detail: Some("closed on GitHub".to_string()),
        };
    }
    let existing = existing_pull(state, owner, name, pull.number);
    // The forge has already settled this contribution; what happens to the
    // GitHub pull request now is settle_finished's to say, and nothing is
    // fetched or reported in the meantime.
    if let Some(pr) = existing
        .as_ref()
        .filter(|pr| pr.merged || pr.state == PullRequestState::Closed)
    {
        return ImportOutcome {
            repo,
            github_slug: target.github_slug.clone(),
            github_number: pull.number,
            forge_number: Some(pr.number),
            action: ImportAction::Unchanged,
            detail: Some("settled on the forge".to_string()),
        };
    }
    let git_bin = state.repo_manager.config().git_bin.clone();
    let Ok(resolved) = state.repo_manager.resolve_parts(owner, name) else {
        return ImportOutcome::failed(
            &repo,
            &target.github_slug,
            pull.number,
            format!("{repo} has no bare repository on this host"),
        );
    };
    let head_sha = match fetch_contribution(&git_bin, &resolved.path, target, pull.number) {
        Ok(sha) => sha,
        Err(error) => {
            return ImportOutcome::failed(&repo, &target.github_slug, pull.number, error);
        }
    };
    let branch = contrib_branch(pull.number);
    let core = state.github.core();
    let (forge_number, action) = match existing {
        Some(pr) => {
            let moved = pr.head.sha != head_sha;
            if moved {
                // A force-push on GitHub: the forge pull request follows the
                // branch it already points at.
                if let Ok(pulls) =
                    core.refresh_pull_request_heads_for_ref(owner, name, &branch, &head_sha)
                {
                    crate::ci_bridge::follow_pull_bases(
                        core, &git_bin, &resolved, owner, name, &pulls,
                    );
                }
            }
            let _ = core.update_pull_request(
                owner,
                name,
                pr.number,
                UpdatePullRequestRequest {
                    title: Some(pull.title.clone()),
                    body: Some(import_body(target, pull)),
                    changed_files: Some(changed_files(
                        &git_bin,
                        &resolved.path,
                        &target.branch,
                        &head_sha,
                    )),
                    ..UpdatePullRequestRequest::default()
                },
            );
            (
                pr.number,
                if moved {
                    ImportAction::Updated
                } else {
                    ImportAction::Unchanged
                },
            )
        }
        None => {
            let base_sha = base_head(state, owner, name, &target.branch);
            let Some(base_sha) = base_sha.filter(|sha| crate::github::pulls::is_hex40(sha)) else {
                return ImportOutcome::failed(
                    &repo,
                    &target.github_slug,
                    pull.number,
                    format!(
                        "could not resolve {repo}:{} to a 40-hex base commit",
                        target.branch
                    ),
                );
            };
            let created = core.create_pull_request(
                owner,
                name,
                &forge_author(&pull.author),
                CreatePullRequestRequest {
                    title: pull.title.clone(),
                    body: Some(import_body(target, pull)),
                    head: branch.clone(),
                    base: target.branch.clone(),
                    head_sha: Some(head_sha.clone()),
                    base_sha: Some(base_sha),
                    changed_files: changed_files(
                        &git_bin,
                        &resolved.path,
                        &target.branch,
                        &head_sha,
                    ),
                    ..CreatePullRequestRequest::default()
                },
            );
            match created {
                Ok(pr) => (pr.number, ImportAction::Opened),
                Err(error) => {
                    return ImportOutcome::failed(
                        &repo,
                        &target.github_slug,
                        pull.number,
                        format!("could not open the forge pull request: {error}"),
                    );
                }
            }
        }
    };
    // The marking is what holds the contribution for a maintainer. It is
    // written on every pass: a contribution must never read as internal
    // because one earlier write failed.
    if let Err(error) = state.pull_origin.set(
        owner,
        name,
        forge_number,
        PullOrigin::Contributed,
        &importer_login(state),
        chrono::Utc::now().timestamp_millis(),
    ) {
        return ImportOutcome::failed(
            &repo,
            &target.github_slug,
            pull.number,
            format!("could not mark the pull request contributed: {error}"),
        );
    }
    let detail = report_back(state, api, owner, name, target, pull, forge_number);
    ImportOutcome {
        repo,
        github_slug: target.github_slug.clone(),
        github_number: pull.number,
        forge_number: Some(forge_number),
        action,
        detail,
    }
}

/// Post the forge gate's answer as a commit status on the GitHub head, and the
/// maintainer review summary as a comment when it has something new to say.
fn report_back(
    state: &WebState,
    api: &dyn crate::github_pull_import::GithubApi,
    owner: &str,
    name: &str,
    target: &PullImportTarget,
    pull: &GithubPull,
    forge_number: u64,
) -> Option<String> {
    let Ok(forge_pr) = state
        .github
        .core()
        .get_pull_request(owner, name, forge_number)
    else {
        return Some("the forge pull request vanished between steps".to_string());
    };
    let gate = gate_status(state, &forge_pr);
    let review = review_summary(state, &forge_pr);
    let dco = dco_status(state, &forge_pr);
    let mut reported = state.import_reports.get(&target.github_slug, pull.number);
    let mut failures = Vec::new();
    let gate_line = format!("{}:{}", gate.state.wire_name(), gate.description);
    if reported.gate.as_deref() != Some(gate_line.as_str()) {
        match api.set_commit_status(&target.github_slug, &pull.head_sha, &gate) {
            Ok(()) => reported.gate = Some(gate_line),
            Err(error) => failures.push(format!("commit status: {error}")),
        }
    }
    let dco_line = format!("{}:{}", dco.state.wire_name(), dco.description);
    if reported.dco.as_deref() != Some(dco_line.as_str()) {
        match api.set_commit_status(&target.github_slug, &pull.head_sha, &dco) {
            Ok(()) => reported.dco = Some(dco_line),
            Err(error) => failures.push(format!("dco status: {error}")),
        }
    }
    if reported.review.as_deref() != Some(review.as_str()) {
        match api.comment(&target.github_slug, pull.number, &review) {
            Ok(()) => reported.review = Some(review),
            Err(error) => failures.push(format!("review comment: {error}")),
        }
    }
    state
        .import_reports
        .put(&target.github_slug, pull.number, reported);
    (!failures.is_empty()).then(|| failures.join("; "))
}

/// Close the GitHub pull request of every contribution the forge has finished
/// with: merged and carried back by the mirror push, or closed unmerged.
fn settle_finished(
    state: &WebState,
    api: &dyn crate::github_pull_import::GithubApi,
    owner: &str,
    name: &str,
    target: &PullImportTarget,
) -> Vec<ImportOutcome> {
    let core = state.github.core();
    let mut outcomes = Vec::new();
    for pr in core
        .list_pull_requests(owner, name, None)
        .unwrap_or_default()
    {
        let Some(number) = contrib_number(&pr.head.ref_name) else {
            continue;
        };
        if !state
            .pull_origin
            .get(owner, name, pr.number)
            .ok()
            .flatten()
            .is_some_and(|origin| {
                origin.origin == PullOrigin::Contributed && origin.set_by == importer_login(state)
            })
        {
            continue;
        }
        if state
            .import_reports
            .get(&target.github_slug, number)
            .settled
        {
            continue;
        }
        let repo = format!("{owner}/{name}");
        let settled = if pr.merged {
            let sha = pr.merge_commit_sha.clone().unwrap_or(pr.head.sha.clone());
            if !mirror_landed(state, owner, name, target, &sha) {
                // The merge has not reached GitHub yet; the next pass looks
                // again. Nothing is said there until the commits are there.
                continue;
            }
            Some((
                ImportAction::ClosedAsMerged,
                format!(
                    "merged as {sha}\n\nThe forge merged this contribution and the mirror push \
                     carried the commits to this branch."
                ),
            ))
        } else if pr.state == PullRequestState::Closed {
            Some((
                ImportAction::ClosedOnForge,
                format!(
                    "closed on the forge without merging: {}",
                    close_reason(state, &pr)
                ),
            ))
        } else {
            None
        };
        let Some((action, body)) = settled else {
            continue;
        };
        let mut detail = None;
        if let Err(error) = api.comment(&target.github_slug, number, &body) {
            detail = Some(format!("comment: {error}"));
        }
        if let Err(error) = api.close_pull(&target.github_slug, number) {
            detail = Some(match detail {
                Some(first) => format!("{first}; close: {error}"),
                None => format!("close: {error}"),
            });
        }
        let mut reported = state.import_reports.get(&target.github_slug, number);
        reported.settled = detail.is_none();
        state
            .import_reports
            .put(&target.github_slug, number, reported);
        outcomes.push(ImportOutcome {
            repo,
            github_slug: target.github_slug.clone(),
            github_number: number,
            forge_number: Some(pr.number),
            action: if detail.is_some() {
                ImportAction::Failed
            } else {
                action
            },
            detail,
        });
    }
    outcomes
}

/// Whether the mirror push has carried `sha` to the GitHub branch. Asked of
/// GitHub and answered locally: the commit is in the forge repository, so
/// ancestry is decided there rather than guessed from a push's bookkeeping.
fn mirror_landed(
    state: &WebState,
    owner: &str,
    name: &str,
    target: &PullImportTarget,
    sha: &str,
) -> bool {
    let Some(api) = state.github_api.as_ref() else {
        return false;
    };
    let Ok(Some(github_head)) = api.branch_head(&target.github_slug, &target.branch) else {
        return false;
    };
    if github_head == sha {
        return true;
    }
    let Ok(resolved) = state.repo_manager.resolve_parts(owner, name) else {
        return false;
    };
    let git_bin = state.repo_manager.config().git_bin.clone();
    contains_commit(&git_bin, &resolved.path, &github_head, sha)
}

/// Why the forge closed a contribution, as far as the forge knows: the last
/// review that asked for changes, else the plain fact that it was closed.
fn close_reason(state: &WebState, pr: &PullRequest) -> String {
    let reviews = state
        .github
        .core()
        .list_reviews(&pr.owner, &pr.repo, pr.number)
        .unwrap_or_default();
    reviews
        .iter()
        .rev()
        .find(|review| review.state == ReviewState::ChangesRequested)
        .map(|review| {
            format!(
                "{} asked for changes on the forge pull request",
                review.author
            )
        })
        .unwrap_or_else(|| "a maintainer closed the forge pull request".to_string())
}

/// The forge gate's answer for a head, as a GitHub commit status.
fn gate_status(state: &WebState, pr: &PullRequest) -> GithubStatus {
    let checks = state
        .github
        .core()
        .list_check_runs(&pr.owner, &pr.repo, Some(&pr.head.sha))
        .map(|list| list.check_runs)
        .unwrap_or_default();
    // A gate result the head already has is reported as it is: a run that
    // failed is never hidden behind the ok-to-test wait.
    let (state_value, description) =
        if checks.is_empty() && !pull_origin::posture(state, pr).gate_may_start() {
            (
                GithubStatusState::Pending,
                "waiting for a maintainer to say ok-to-test".to_string(),
            )
        } else if checks.is_empty() {
            (
                GithubStatusState::Pending,
                "the forge gate has not run on this head yet".to_string(),
            )
        } else if let Some(failed) = checks.iter().find(|check| {
            check.status == CheckRunStatus::Completed
                && check.conclusion != Some(CheckConclusion::Success)
        }) {
            (
                GithubStatusState::Failure,
                format!("the forge gate failed: {}", failed.name),
            )
        } else if checks
            .iter()
            .any(|check| check.status != CheckRunStatus::Completed)
        {
            (
                GithubStatusState::Pending,
                "the forge gate is running".to_string(),
            )
        } else {
            (
                GithubStatusState::Success,
                format!("the forge gate passed ({} checks)", checks.len()),
            )
        };
    GithubStatus {
        state: state_value,
        context: GATE_STATUS_CONTEXT.to_string(),
        description,
    }
}

/// The DCO sign-off check as a GitHub commit status, so the contributor sees
/// what to fix where they opened the pull request.
fn dco_status(state: &WebState, pr: &PullRequest) -> GithubStatus {
    let (state_value, description) = match super::pull_contrib::dco_verdict(state, pr) {
        super::pull_contrib::DcoVerdict::Pass => (
            GithubStatusState::Success,
            "every commit is signed off".to_string(),
        ),
        super::pull_contrib::DcoVerdict::Fail(message) => {
            // GitHub truncates a status description at 140 characters.
            let short: String = message.chars().take(140).collect();
            (GithubStatusState::Failure, short)
        }
    };
    GithubStatus {
        state: state_value,
        context: super::pull_contrib::DCO_CONTEXT.to_string(),
        description,
    }
}

/// What the maintainers have said, as the comment posted on GitHub.
fn review_summary(state: &WebState, pr: &PullRequest) -> String {
    let posture = pull_origin::posture(state, pr);
    let reviews = state
        .github
        .core()
        .list_reviews(&pr.owner, &pr.repo, pr.number)
        .unwrap_or_default();
    let effective = effective_reviews_for_head(&reviews, &pr.head.sha);
    let approvals = effective
        .iter()
        .filter(|review| review.state == ReviewState::Approved)
        .count();
    let changes: Vec<&str> = effective
        .iter()
        .filter(|review| review.state == ReviewState::ChangesRequested)
        .map(|review| review.author.as_str())
        .collect();
    let mut lines = vec![format!(
        "Forge review of `{}/{}` pull request #{} (head {}):",
        pr.owner, pr.repo, pr.number, pr.head.sha
    )];
    lines.push(format!(
        "- approvals on this head: {approvals}{}",
        if posture.maintainer_approvals > 0 {
            format!(
                ", including {} from a maintainer",
                posture.maintainer_approvals
            )
        } else {
            String::new()
        }
    ));
    if !changes.is_empty() {
        lines.push(format!("- changes requested by: {}", changes.join(", ")));
    }
    if !posture.gate_may_start() {
        lines.push(
            "- the gate has not started: a configured maintainer has to say ok-to-test for this \
             exact head."
                .to_string(),
        );
    }
    if let Some(super::pull_contrib::DcoVerdict::Fail(message)) = &posture.dco {
        lines.push(format!("- {message}"));
    }
    lines.push(if posture.maintainer_approvals > 0 {
        "- a maintainer has approved this head; it can merge on the forge once the gate is green."
            .to_string()
    } else {
        "- held for a maintainer: a contribution merges only after a configured maintainer \
         approves this exact head."
            .to_string()
    });
    lines.join("\n")
}

/// The forge pull request on this contribution's branch, whatever state it is
/// in. The branch carries the GitHub number, so a re-import finds its own.
fn existing_pull(state: &WebState, owner: &str, name: &str, number: u64) -> Option<PullRequest> {
    let branch = contrib_branch(number);
    state
        .github
        .core()
        .list_pull_requests(owner, name, None)
        .ok()?
        .into_iter()
        .find(|pr| pr.head.ref_name == branch)
}

/// The forge branch tip a contribution is proposed against.
fn base_head(state: &WebState, owner: &str, name: &str, branch: &str) -> Option<String> {
    let bare = state.repo_manager.open_parts(owner, name).ok()?;
    jeryu_gitd::refs::RefService::new((*state.repo_manager).clone())
        .resolve_commit(&bare, &format!("refs/heads/{branch}"))
        .ok()
        .flatten()
}

/// The login the marking is recorded under: the first configured importer, or
/// a plain name when a site named none.
fn importer_login(state: &WebState) -> String {
    state
        .pull_importers
        .first()
        .cloned()
        .unwrap_or_else(|| "github-import".to_string())
}

/// The forge pull request's body: a link back, the contributor's words quoted
/// as the data they are, and nothing the forge acts on.
fn import_body(target: &PullImportTarget, pull: &GithubPull) -> String {
    let mut body = format!(
        "Imported from the GitHub mirror: https://github.com/{}/pull/{}\n\nOpened there by \
         `{}`. The text below is the contributor's, quoted as written.\n\n",
        target.github_slug, pull.number, pull.author
    );
    match pull
        .body
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        Some(text) => {
            for line in text.lines() {
                body.push_str("> ");
                body.push_str(line);
                body.push('\n');
            }
        }
        None => body.push_str("> (no description)\n"),
    }
    body
}

/// What a webhook delivery carries. Only the repository and the number are
/// read; the action and everything else are the relay's word for it, and the
/// pull request itself is read from the API afterwards.
#[derive(Debug, Deserialize)]
struct PullEvent {
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    pull_request: Option<PullEventPull>,
    #[serde(default)]
    repository: Option<PullEventRepo>,
}

#[derive(Debug, Deserialize)]
struct PullEventPull {
    #[serde(default)]
    number: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PullEventRepo {
    #[serde(default)]
    full_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct EventResponse {
    /// `imported` or `ignored`.
    status: &'static str,
    reason: Option<String>,
    outcome: Option<ImportOutcome>,
}

/// `POST /api/v1/github-mirror/pull-events`: one GitHub pull request event.
///
/// The site's webhook relay calls this as the importer identity; the payload
/// it carries is untrusted and is used only to name which pull request to go
/// and read.
pub(super) async fn events(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    body: Bytes,
) -> AxumResponse {
    if !may_import(&account, &state.pull_importers) {
        return api_error(
            StatusCode::FORBIDDEN,
            "pull_import_importer_required",
            "only a global admin or a JERYU_PULL_IMPORTERS login may deliver mirror pull \
             request events",
        );
    }
    let Ok(event) = serde_json::from_slice::<PullEvent>(&body) else {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "pull_import_invalid_request",
            "send the GitHub pull_request event payload",
        );
    };
    let slug = event
        .repository
        .and_then(|repo| repo.full_name)
        .unwrap_or_default();
    let number = event
        .number
        .or_else(|| event.pull_request.and_then(|pull| pull.number));
    let (Some(number), false) = (number, slug.is_empty()) else {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "pull_import_invalid_request",
            "the event names no repository or no pull request number",
        );
    };
    if state.pull_maintainers.is_empty() {
        return ignored("the importer is off: no JERYU_PULL_MAINTAINERS are configured");
    }
    let Some(api) = state.github_api.clone() else {
        return ignored("the importer is off on this forge");
    };
    let Some((owner, name)) = state.pull_import.forge_repo_for(&slug) else {
        return ignored(&format!("{slug} does not import its pull requests"));
    };
    let blocking = tokio::task::spawn_blocking(move || {
        let target = state
            .pull_import
            .target(&owner, &name)
            .expect("the enrolled repository has a target")
            .clone();
        let pull = api.get_pull(&target.github_slug, number)?;
        let mut outcomes = vec![import_pull(
            &state,
            api.as_ref(),
            &owner,
            &name,
            &target,
            &pull,
        )];
        outcomes.extend(settle_finished(
            &state,
            api.as_ref(),
            &owner,
            &name,
            &target,
        ));
        Ok::<_, String>(outcomes)
    })
    .await;
    match blocking {
        Ok(Ok(outcomes)) => Json(EventResponse {
            status: "imported",
            reason: None,
            outcome: outcomes.into_iter().next(),
        })
        .into_response(),
        Ok(Err(error)) => api_error(
            StatusCode::BAD_GATEWAY,
            "pull_import_github_unreachable",
            &format!("could not read the pull request from GitHub: {error}"),
        ),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pull_import_failed",
            &format!("the import did not run: {error}"),
        ),
    }
}

/// Ignoring an event is a normal answer, not a refusal: a mirror nobody
/// enrolled may still deliver, and the forge says so plainly.
fn ignored(reason: &str) -> AxumResponse {
    Json(EventResponse {
        status: "ignored",
        reason: Some(reason.to_string()),
        outcome: None,
    })
    .into_response()
}

/// Who may deliver an event: the same trust as marking an origin, because an
/// event delivery is what makes the forge write that marking.
fn may_import(account: &AccountSummary, importers: &[String]) -> bool {
    account.role == UserRole::Admin || importers.iter().any(|login| login == &account.login)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_github_login_becomes_a_forge_login_that_is_never_the_importer() {
        assert_eq!(forge_author("octo-dev"), "gh-octo-dev");
        assert_eq!(forge_author("dependabot[bot]"), "gh-dependabot-bot");
        assert_eq!(forge_author(""), "gh-contributor");
    }

    #[test]
    fn the_poll_cadence_can_be_turned_off() {
        assert_eq!(
            interval_from(None),
            Some(Duration::from_secs(DEFAULT_INTERVAL_MINUTES * 60))
        );
        assert_eq!(interval_from(Some("2")), Some(Duration::from_secs(120)));
        assert_eq!(interval_from(Some("0")), None);
        assert_eq!(interval_from(Some("soon")), None);
    }
}
