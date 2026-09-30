//! Merge queue (docs/merge-queue.md).
//!
//! An approved pull request whose only obstacle is that the base moved joins
//! the queue. The forge replays it onto the base tip into
//! `refs/queue/<base>/<number>`, the gate runner gates that exact commit, and
//! the queue lands it by compare-and-swap fast-forward through the same path as
//! a direct merge (`GithubRouter::land_queued`). The base therefore only ever
//! moves to a commit that passed the gate, and nobody rebases by hand.
//!
//! State lives in the repository itself: the queued commit in `refs/queue/…`
//! and a JSON record (PR head, who approved, attempts) as a blob under
//! `refs/queue-meta/…`, so a restart resumes the queue and the runner fetches
//! the queued commit like any ref. Clients cannot push either namespace (see
//! `git_transport::git_receive_pack`). An in-memory index serves the listings
//! cheaply; it is rebuilt from the refs on first use.

use super::*;
use jeryu_core::{CreateCommentRequest, PullRequest, ReviewState};

mod replay;

pub(crate) use replay::is_queue_owned_ref;

/// Replay `pr_head` onto `base_tip` in the bare repository at `bare`, the way
/// the queue builds its commits, so a direct merge on a linear-history base
/// can land as a fast-forward. Returns the replayed tip (or `pr_head` itself
/// when it already sits on the tip), or why the replay could not be built.
pub(crate) fn rebase_onto(
    git_bin: &str,
    bare: &std::path::Path,
    base_tip: &str,
    pr_head: &str,
) -> Result<String, String> {
    Git { bin: git_bin, bare }
        .replay(base_tip, pr_head, None)
        .map_err(|err| err.to_string())
}
use replay::{Git, ReplayFailure, is_sha};

/// Identities whose approvals count as automation, not a person.
const AUTOMATION_DEFAULT: &str = "gatebot,pragent,jain-merge-bot";
/// A failed queue gate is rebuilt (a fresh commit, so a fresh gate) once.
const MAX_ATTEMPTS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueueState {
    /// Queue commit built; waiting for its gate.
    Building,
    Landed,
    /// The gate failed twice, or the PR stopped passing the merge gate.
    Failed,
    /// Removed by request, a new PR head, or a replay that could not be built.
    Dequeued,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Approver {
    pub(crate) login: String,
    pub(crate) automation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Attempt {
    pub(crate) queue_sha: String,
    pub(crate) base_sha: String,
    pub(crate) conclusion: Option<String>,
    pub(crate) at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct QueueEntry {
    /// `owner/name`.
    pub(crate) repo: String,
    pub(crate) base: String,
    pub(crate) number: u64,
    pub(crate) pr_head_sha: String,
    pub(crate) base_sha: String,
    pub(crate) queue_ref: String,
    pub(crate) queue_sha: String,
    pub(crate) state: QueueState,
    pub(crate) enqueued_at: String,
    pub(crate) enqueued_by: String,
    pub(crate) approvers: Vec<Approver>,
    pub(crate) attempts: Vec<Attempt>,
    pub(crate) reason: Option<String>,
    pub(crate) landed_sha: Option<String>,
}

type Key = (String, String, u64);

#[derive(Default)]
pub(crate) struct MergeQueue {
    inner: Mutex<QueueIndex>,
}

#[derive(Default)]
struct QueueIndex {
    loaded: bool,
    entries: BTreeMap<Key, QueueEntry>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn meta_ref(base: &str, number: u64) -> String {
    format!("refs/queue-meta/{base}/{number}")
}

fn queue_ref(base: &str, number: u64) -> String {
    format!("refs/queue/{base}/{number}")
}

fn automation_identities() -> Vec<String> {
    std::env::var("JERYU_AUTOMATION_IDENTITIES")
        .unwrap_or_else(|_| AUTOMATION_DEFAULT.into())
        .split(',')
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .map(str::to_string)
        .collect()
}

fn git_for<'a>(state: &'a WebState, path: &'a Path) -> Git<'a> {
    Git {
        bin: &state.repo_manager.config().git_bin,
        bare: path,
    }
}

impl MergeQueue {
    fn lock(&self) -> std::sync::MutexGuard<'_, QueueIndex> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Rebuild the index from every repository's `refs/queue-meta/*` once.
    fn ensure_loaded(&self, state: &WebState, index: &mut QueueIndex) {
        if index.loaded {
            return;
        }
        index.loaded = true;
        for repo in state.core.list_repositories(None) {
            let Ok(resolved) = state.repo_manager.open_parts(&repo.owner, &repo.name) else {
                continue;
            };
            let git = git_for(state, &resolved.path);
            for (_, body) in git.read_blob_refs("refs/queue-meta/") {
                if let Ok(entry) = serde_json::from_str::<QueueEntry>(&body) {
                    index
                        .entries
                        .insert((repo.owner.clone(), repo.name.clone(), entry.number), entry);
                }
            }
        }
    }

    pub(crate) fn entries(
        &self,
        state: &WebState,
        filter: impl Fn(&QueueEntry) -> bool,
    ) -> Vec<QueueEntry> {
        let mut index = self.lock();
        self.ensure_loaded(state, &mut index);
        index
            .entries
            .values()
            .filter(|entry| filter(entry))
            .cloned()
            .collect()
    }
}

fn persist(state: &WebState, owner: &str, repo: &str, entry: &QueueEntry) {
    let Ok(resolved) = state.repo_manager.open_parts(owner, repo) else {
        return;
    };
    let git = git_for(state, &resolved.path);
    if let Ok(json) = serde_json::to_string(entry) {
        let _ = git.write_blob_ref(&meta_ref(&entry.base, entry.number), &json);
    }
    if entry.state != QueueState::Building {
        git.delete_ref(&entry.queue_ref);
    }
}

fn comment(state: &WebState, owner: &str, repo: &str, number: u64, body: String) {
    let _ = state.core.add_issue_comment(
        owner,
        repo,
        number,
        "merge-queue",
        CreateCommentRequest { body },
    );
}

/// One pipeline event per queue transition (`queue.enqueued`, `.building`,
/// `.landed`, `.failed`, `.dequeued`, `.refused`), keyed to the PR head so it
/// joins the pull request's other events.
fn emit_queue(
    state: &WebState,
    entry: &QueueEntry,
    kind: &str,
    actor: &str,
    needs_human: bool,
    summary: String,
) {
    let (owner, repo) = entry.repo.split_once('/').unwrap_or(("", &entry.repo));
    let head_ref = state
        .core
        .get_pull_request(owner, repo, entry.number)
        .map(|pr| pr.head.ref_name)
        .unwrap_or_default();
    let (family, shift) = super::shift::shift_context(state, owner, repo, &head_ref);
    let outcome = match entry.state {
        QueueState::Building => None,
        QueueState::Landed => Some("success"),
        QueueState::Failed => Some("failure"),
        QueueState::Dequeued => Some("dequeued"),
    };
    super::pipeline::emit(
        state,
        super::pipeline::NewEvent {
            actor: Some(actor.to_string()),
            family,
            repo: Some(entry.repo.clone()),
            pr: i64::try_from(entry.number).ok(),
            sha: Some(entry.pr_head_sha.clone()),
            shift,
            outcome: outcome.map(str::to_string),
            needs_human,
            reason: entry.reason.clone(),
            detail: Some(json!({
                "base": entry.base,
                "queue_sha": entry.queue_sha,
                "base_sha": entry.base_sha,
                "landed_sha": entry.landed_sha,
                "attempts": entry.attempts.len(),
                "approvers": entry.approvers.iter().map(|a| a.login.as_str()).collect::<Vec<_>>(),
            })),
            ..super::pipeline::NewEvent::forge(kind, summary)
        },
    );
}

pub(super) fn approvers(state: &WebState, pr: &PullRequest) -> Vec<Approver> {
    let automation = automation_identities();
    let reviews = state
        .core
        .list_reviews(&pr.owner, &pr.repo, pr.number)
        .unwrap_or_default();
    jeryu_core::effective_reviews_for_head(&reviews, &pr.head.sha)
        .into_iter()
        .filter(|review| review.state == ReviewState::Approved)
        .map(|review| Approver {
            automation: automation.iter().any(|login| login == &review.author),
            login: review.author.clone(),
        })
        .collect()
}

/// Build (or rebuild) the queue commit for `entry` on the current base tip.
fn build(
    state: &WebState,
    owner: &str,
    repo: &str,
    entry: &mut QueueEntry,
) -> Result<(), ReplayFailure> {
    build_after(state, owner, repo, entry, None)
}

/// [`build`], with replayed commits committed strictly after `committed_after`
/// (unix seconds).
fn build_after(
    state: &WebState,
    owner: &str,
    repo: &str,
    entry: &mut QueueEntry,
    committed_after: Option<i64>,
) -> Result<(), ReplayFailure> {
    let resolved = state
        .repo_manager
        .open_parts(owner, repo)
        .map_err(|err| ReplayFailure::Git(err.to_string()))?;
    let git = git_for(state, &resolved.path);
    let base_sha = git
        .resolve(&format!("refs/heads/{}", entry.base))
        .ok_or_else(|| ReplayFailure::Git(format!("refs/heads/{} does not resolve", entry.base)))?;
    let queue_sha = git.replay(&base_sha, &entry.pr_head_sha, committed_after)?;
    git.update_ref(&entry.queue_ref, &queue_sha)?;
    entry.base_sha = base_sha.clone();
    entry.queue_sha = queue_sha.clone();
    entry.attempts.push(Attempt {
        queue_sha,
        base_sha,
        conclusion: None,
        at: now(),
    });
    Ok(())
}

fn is_open(pr: &PullRequest) -> bool {
    use jeryu_core::PullRequestState::{Closed, Draft, Merged};
    !pr.merged && !matches!(pr.state, Closed | Merged | Draft)
}

fn can_write(state: &WebState, account: &AccountSummary, owner: &str, repo: &str) -> bool {
    account.role == UserRole::Admin || state.core.user_can_write_repo(&account.login, owner, repo)
}

fn entry_response(status: StatusCode, entry: &QueueEntry) -> AxumResponse {
    (status, Json(entry.clone())).into_response()
}

/// `POST /api/v1/repos/:id/pulls/:number/queue`
pub(super) async fn enqueue(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
) -> AxumResponse {
    let Some(repo) = super::repositories::find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    if !can_write(&state, &account, &repo.owner, &repo.name) {
        return api_error(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "enqueueing needs write access to the repository",
        );
    }
    let pr = match state.core.get_pull_request(&repo.owner, &repo.name, number) {
        Ok(pr) => pr,
        Err(_) => return api_error(StatusCode::NOT_FOUND, "not_found", "pull request not found"),
    };
    if !is_open(&pr) {
        // A draft is the one refusal a reader can act on, so the queue says so
        // on the pull request's own timeline instead of only in this response.
        if pr.draft {
            super::pipeline::emit::pull_skipped(&state, &pr, "merge-queue", "draft");
            return api_error(
                StatusCode::CONFLICT,
                "not_open",
                "a draft pull request cannot be queued: mark it ready for review first",
            );
        }
        return api_error(
            StatusCode::CONFLICT,
            "not_open",
            "only open pull requests can be queued",
        );
    }
    let (pass, blockers, _) = super::pulls::queue_gate(&state, &pr);
    if !pass {
        return api_error(
            StatusCode::CONFLICT,
            "merge_gate_blocked",
            &format!(
                "the pull request does not pass its merge gate: {}",
                blockers.join("; ")
            ),
        );
    }
    let key = (repo.owner.clone(), repo.name.clone(), number);
    let queue = &state.merge_queue;
    let mut index = queue.lock();
    queue.ensure_loaded(&state, &mut index);
    if let Some(existing) = index.entries.get(&key)
        && existing.state == QueueState::Building
        && existing.pr_head_sha == pr.head.sha
    {
        return entry_response(StatusCode::OK, existing);
    }
    let mut entry = QueueEntry {
        repo: format!("{}/{}", repo.owner, repo.name),
        base: pr.base.ref_name.clone(),
        number,
        pr_head_sha: pr.head.sha.clone(),
        base_sha: String::new(),
        queue_ref: queue_ref(&pr.base.ref_name, number),
        queue_sha: String::new(),
        state: QueueState::Building,
        enqueued_at: now(),
        enqueued_by: account.login.clone(),
        approvers: approvers(&state, &pr),
        attempts: Vec::new(),
        reason: None,
        landed_sha: None,
    };
    if !is_sha(&entry.pr_head_sha) {
        return api_error(
            StatusCode::CONFLICT,
            "bad_head",
            "the pull request head is not a commit",
        );
    }
    if let Err(failure) = build(&state, &repo.owner, &repo.name, &mut entry) {
        let code = match failure {
            ReplayFailure::Conflict(_) => "queue_conflict",
            ReplayFailure::Mismatch(_) => "queue_mismatch",
            ReplayFailure::MergeCommits => "queue_merge_commits",
            ReplayFailure::Git(_) => "queue_git_error",
        };
        // The PR cannot be replayed onto the base: somebody has to rebase it.
        entry.state = QueueState::Dequeued;
        entry.reason = Some(failure.to_string());
        emit_queue(
            &state,
            &entry,
            "queue.refused",
            &account.login,
            true,
            format!(
                "{}#{} could not join the merge queue ({code})",
                entry.repo, number
            ),
        );
        return api_error(StatusCode::CONFLICT, code, &failure.to_string());
    }
    persist(&state, &repo.owner, &repo.name, &entry);
    index.entries.insert(key, entry.clone());
    emit_queue(
        &state,
        &entry,
        "queue.enqueued",
        &account.login,
        false,
        format!("{}#{} joined the merge queue", entry.repo, number),
    );
    entry_response(StatusCode::CREATED, &entry)
}

/// `DELETE /api/v1/repos/:id/pulls/:number/queue`
pub(super) async fn dequeue(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
) -> AxumResponse {
    let Some(repo) = super::repositories::find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    if !can_write(&state, &account, &repo.owner, &repo.name) {
        return api_error(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "dequeueing needs write access to the repository",
        );
    }
    let queue = &state.merge_queue;
    let mut index = queue.lock();
    queue.ensure_loaded(&state, &mut index);
    let key = (repo.owner.clone(), repo.name.clone(), number);
    match index.entries.get_mut(&key) {
        Some(entry) if entry.state == QueueState::Building => {
            entry.state = QueueState::Dequeued;
            entry.reason = Some(format!("dequeued by {}", account.login));
            persist(&state, &repo.owner, &repo.name, entry);
            emit_queue(
                &state,
                entry,
                "queue.dequeued",
                &account.login,
                false,
                format!("{}#{} left the merge queue", entry.repo, number),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        _ => api_error(
            StatusCode::NOT_FOUND,
            "not_queued",
            "the pull request is not queued",
        ),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct QueueListQuery {
    state: Option<String>,
}

/// `GET /api/v1/merge-queue?state=building` — every repository the caller can
/// read. The runner polls this on each tick, so it reads the index only.
pub(super) async fn list_all(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<QueueListQuery>,
) -> AxumResponse {
    let wanted = query.state.unwrap_or_else(|| "building".to_string());
    let entries = state.merge_queue.entries(&state, |entry| {
        let state_name = serde_json::to_value(entry.state)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default();
        (wanted == "all" || state_name == wanted)
            && entry.repo.split_once('/').is_some_and(|(owner, name)| {
                account.role == UserRole::Admin
                    || state.core.user_can_read_repo(&account.login, owner, name)
            })
    });
    Json(json!({ "entries": entries })).into_response()
}

/// `GET /api/v1/repos/:id/merge-queue`
pub(super) async fn list_repo(
    State(state): State<Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    let Some(repo) = super::repositories::find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    let full = format!("{}/{}", repo.owner, repo.name);
    let entries = state
        .merge_queue
        .entries(&state, |entry| entry.repo == full);
    Json(json!({ "entries": entries })).into_response()
}

/// Gate verdict on the queue commit, reading commit statuses and check runs
/// the way the merge passport does: `Some(true)` every required context
/// succeeded, `Some(false)` one failed, `None` still waiting. A base with no
/// required contexts needs at least one reported context, all green.
fn gate_result(
    state: &WebState,
    owner: &str,
    repo: &str,
    sha: &str,
    required: &[String],
) -> Option<bool> {
    use jeryu_core::{CheckConclusion, CheckRunStatus, CommitStatusState};
    let statuses = state.core.combined_status(owner, repo, sha).ok()?.statuses;
    let checks = state
        .core
        .list_check_runs(owner, repo, Some(sha))
        .ok()?
        .check_runs;
    let verdict = |name: &str| -> Option<bool> {
        if let Some(status) = statuses
            .iter()
            .filter(|status| status.context == name)
            .max_by_key(|status| status.updated_at)
        {
            return match status.state {
                CommitStatusState::Success => Some(true),
                CommitStatusState::Pending => None,
                CommitStatusState::Failure | CommitStatusState::Error => Some(false),
            };
        }
        let check = checks
            .iter()
            .filter(|check| check.name == name)
            .max_by_key(|check| check.completed_at.unwrap_or(check.started_at))?;
        match check.status {
            CheckRunStatus::Completed => Some(check.conclusion == Some(CheckConclusion::Success)),
            _ => None,
        }
    };
    let names: Vec<String> = if required.is_empty() {
        let mut reported: Vec<String> = statuses
            .iter()
            .map(|status| status.context.clone())
            .chain(checks.iter().map(|check| check.name.clone()))
            .collect();
        reported.sort();
        reported.dedup();
        if reported.is_empty() {
            return None;
        }
        reported
    } else {
        required.to_vec()
    };
    let mut all_green = true;
    for name in &names {
        match verdict(name) {
            Some(false) => return Some(false),
            Some(true) => {}
            None => all_green = false,
        }
    }
    all_green.then_some(true)
}

/// Advance every building entry once: dequeue stale ones, rebuild on a moved
/// base, retry a failed gate once, and land a green one. Returns how many
/// entries changed state.
pub(crate) fn tick(state: &WebState) -> usize {
    let queue = &state.merge_queue;
    let mut index = queue.lock();
    queue.ensure_loaded(state, &mut index);
    let keys: Vec<Key> = index
        .entries
        .iter()
        .filter(|(_, entry)| entry.state == QueueState::Building)
        .map(|(key, _)| key.clone())
        .collect();
    let mut changed = 0;
    for key in keys {
        let (owner, repo, number) = key.clone();
        let Some(entry) = index.entries.get_mut(&key) else {
            continue;
        };
        let before = entry.state;
        advance(state, &owner, &repo, number, entry);
        if entry.state != before {
            changed += 1;
        }
    }
    changed
}

fn finish(
    state: &WebState,
    owner: &str,
    repo: &str,
    entry: &mut QueueEntry,
    to: QueueState,
    reason: String,
) {
    entry.state = to;
    entry.reason = Some(reason.clone());
    persist(state, owner, repo, entry);
    let (kind, needs_human, verb) = match to {
        QueueState::Landed => ("queue.landed", false, "landed from the merge queue"),
        QueueState::Failed => ("queue.failed", true, "failed in the merge queue"),
        // Leaving the queue for anything but a landed PR strands an approved
        // PR until somebody re-queues or rebases it.
        _ => ("queue.dequeued", true, "was dropped from the merge queue"),
    };
    emit_queue(
        state,
        entry,
        kind,
        "merge-queue",
        needs_human,
        format!("{}#{} {verb}", entry.repo, entry.number),
    );
    if to == QueueState::Landed
        && let Ok(pr) = state.core.get_pull_request(owner, repo, entry.number)
    {
        super::pipeline::emit::pull_merged(state, &pr, "merge-queue", "merge_queue");
    }
    comment(
        state,
        owner,
        repo,
        entry.number,
        format!("Merge queue: {reason}"),
    );
}

fn advance(state: &WebState, owner: &str, repo: &str, number: u64, entry: &mut QueueEntry) {
    let pr = match state.core.get_pull_request(owner, repo, number) {
        Ok(pr) => pr,
        Err(_) => return,
    };
    if !is_open(&pr) {
        entry.state = QueueState::Dequeued;
        entry.reason = Some("the pull request is no longer open".to_string());
        persist(state, owner, repo, entry);
        emit_queue(
            state,
            entry,
            "queue.dequeued",
            "merge-queue",
            false,
            format!("{}#{} left the merge queue", entry.repo, entry.number),
        );
        return;
    }
    if pr.head.sha != entry.pr_head_sha {
        return finish(
            state,
            owner,
            repo,
            entry,
            QueueState::Dequeued,
            format!(
                "the pull request head moved to {}; enqueue it again",
                pr.head.sha
            ),
        );
    }
    let (pass, blockers, required) = super::pulls::queue_gate(state, &pr);
    if !pass {
        return finish(
            state,
            owner,
            repo,
            entry,
            QueueState::Failed,
            format!(
                "the pull request no longer passes its merge gate: {}",
                blockers.join("; ")
            ),
        );
    }
    match gate_result(state, owner, repo, &entry.queue_sha, &required) {
        None => {}
        Some(false) => {
            if let Some(last) = entry.attempts.last_mut() {
                last.conclusion = Some("failure".to_string());
            }
            // A PR already on the tip is its own queue commit: no fresh commit
            // to retry with.
            if entry.attempts.len() < MAX_ATTEMPTS && entry.queue_sha != entry.pr_head_sha {
                if let Err(failure) = rebuild_fresh(state, owner, repo, entry) {
                    finish(
                        state,
                        owner,
                        repo,
                        entry,
                        QueueState::Dequeued,
                        failure.to_string(),
                    );
                } else {
                    persist(state, owner, repo, entry);
                    emit_queue(
                        state,
                        entry,
                        "queue.building",
                        "merge-queue",
                        false,
                        format!(
                            "{}#{} rebuilt for a second gate after a red one",
                            entry.repo, entry.number
                        ),
                    );
                }
            } else {
                let tried: Vec<&str> = entry
                    .attempts
                    .iter()
                    .map(|a| a.queue_sha.as_str())
                    .collect();
                finish(
                    state,
                    owner,
                    repo,
                    entry,
                    QueueState::Failed,
                    format!("the gate failed on {}", tried.join(" and ")),
                );
            }
        }
        Some(true) => land(state, owner, repo, entry),
    }
}

/// A retry needs a new commit so the runner gates it afresh: rebuild on the
/// current tip with a committer time past the previous attempt's, so even an
/// unmoved tip yields a new sha.
fn rebuild_fresh(
    state: &WebState,
    owner: &str,
    repo: &str,
    entry: &mut QueueEntry,
) -> Result<(), ReplayFailure> {
    let resolved = state
        .repo_manager
        .open_parts(owner, repo)
        .map_err(|err| ReplayFailure::Git(err.to_string()))?;
    let previous = git_for(state, &resolved.path).committer_time(&entry.queue_sha)?;
    build_after(state, owner, repo, entry, Some(previous))
}

fn land(state: &WebState, owner: &str, repo: &str, entry: &mut QueueEntry) {
    match state.github.land_queued(
        owner,
        repo,
        entry.number,
        &entry.pr_head_sha,
        &entry.queue_sha,
        &entry.base_sha,
    ) {
        Ok(sha) => {
            if let Some(last) = entry.attempts.last_mut() {
                last.conclusion = Some("success".to_string());
            }
            entry.landed_sha = Some(sha.clone());
            let approved_by: Vec<String> = entry
                .approvers
                .iter()
                .map(|a| {
                    format!(
                        "{}{}",
                        a.login,
                        if a.automation { " (automation)" } else { "" }
                    )
                })
                .collect();
            finish(
                state,
                owner,
                repo,
                entry,
                QueueState::Landed,
                format!(
                    "landed at {sha} (replay of {} gated green; approved by {})",
                    entry.pr_head_sha,
                    if approved_by.is_empty() {
                        "nobody".to_string()
                    } else {
                        approved_by.join(", ")
                    }
                ),
            );
        }
        Err(crate::github::pulls::LandRefusal::BaseMoved(_)) => {
            if let Err(failure) = build(state, owner, repo, entry) {
                finish(
                    state,
                    owner,
                    repo,
                    entry,
                    QueueState::Dequeued,
                    failure.to_string(),
                );
            } else {
                persist(state, owner, repo, entry);
                emit_queue(
                    state,
                    entry,
                    "queue.building",
                    "merge-queue",
                    false,
                    format!(
                        "{}#{} rebuilt because the base moved",
                        entry.repo, entry.number
                    ),
                );
            }
        }
        Err(crate::github::pulls::LandRefusal::Blocked(reason)) => {
            finish(
                state,
                owner,
                repo,
                entry,
                QueueState::Failed,
                format!("could not land: {reason}"),
            );
        }
    }
}

/// Run `tick` every `every` in the background.
pub(crate) fn spawn_worker(state: Arc<WebState>, every: std::time::Duration) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        loop {
            interval.tick().await;
            let state = state.clone();
            let _ = tokio::task::spawn_blocking(move || tick(&state)).await;
        }
    });
}
