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

/// The pull requests the queue is building on `base` in the bare repository
/// at `bare`, read from the persisted `refs/queue-meta/<base>/*` entries. A
/// direct merge that moved `base` now would make each of them rebuild and gate
/// again, so the direct path defers to the queue while this is not empty.
pub(crate) fn building_on_base(git_bin: &str, bare: &std::path::Path, base: &str) -> Vec<u64> {
    Git { bin: git_bin, bare }
        .read_blob_refs(&format!("refs/queue-meta/{base}/"))
        .into_iter()
        .filter_map(|(_, body)| serde_json::from_str::<QueueEntry>(&body).ok())
        .filter(|entry| entry.base == base && entry.state == QueueState::Building)
        .map(|entry| entry.number)
        .collect()
}
use replay::{Git, ReplayFailure, is_sha};

/// Identities whose approvals count as automation, not a person
/// (`JERYU_AUTOMATION_IDENTITIES`). They belong to one installation, so this
/// public source carries no default: unset, every approval counts as a person's.
/// A failed queue gate is rebuilt (a fresh commit, so a fresh gate) once.
/// Only the entry's own red gates count: a rebuild because the base or the
/// entries below it moved spends nothing.
const MAX_ATTEMPTS: usize = 2;

/// An attempt replaced before its gate decided anything, because the base or
/// the queue below it changed. It counts against nobody's retry.
const SUPERSEDED: &str = "superseded";

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

impl QueueState {
    const ALL: [Self; 4] = [Self::Building, Self::Landed, Self::Failed, Self::Dequeued];

    /// The state as it is spelled on the wire, the same string the JSON
    /// carries, so `?state=` and the listed entries never drift apart.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Landed => "landed",
            Self::Failed => "failed",
            Self::Dequeued => "dequeued",
        }
    }

    /// Every state as it is spelled on the wire.
    pub(crate) fn names() -> Vec<&'static str> {
        Self::ALL.iter().map(|state| state.as_str()).collect()
    }
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
    /// The forge code of an enqueue the queue refused (`queue_conflict`,
    /// `queue_mismatch`, `queue_merge_commits`, `queue_git_error`). Set only on
    /// a `Dequeued` entry that never built, and it is what tells the inbox the
    /// refusal apart from an entry that was queued and then dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) refusal_code: Option<String>,
    pub(crate) landed_sha: Option<String>,
    /// The queued pull request whose queue commit this one is built on, while
    /// both are queued on the same base; `None` when it is built on the base
    /// tip. Entries form one stack per base, and a green gate lands its entry
    /// together with every entry below it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) after: Option<u64>,
    /// The entry whose green gate landed this one, when that was an entry
    /// above it in the stack rather than its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) landed_with: Option<u64>,
}

type Key = (String, String, u64);

/// Stack order on one base: first queued first.
fn stack_order(entry: &QueueEntry) -> (String, u64) {
    (entry.enqueued_at.clone(), entry.number)
}

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

fn automation_identities() -> &'static [String] {
    super::auth::AUTOMATION_IDENTITIES.configured()
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

    /// Seeds the in-memory index, marking it loaded so no git read replaces
    /// what was seeded. A paging assertion needs a queue of a known length and
    /// nothing else the queue does.
    #[cfg(test)]
    pub(in crate::web) fn seed(&self, entries: Vec<QueueEntry>) {
        let mut index = self.lock();
        index.loaded = true;
        for entry in entries {
            let (owner, name) = entry
                .repo
                .split_once('/')
                .expect("a seeded entry names owner/name");
            index.entries.insert(
                (owner.to_string(), name.to_string(), entry.number),
                entry.clone(),
            );
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

/// Build (or rebuild) the queue commit for `entry` on `parent`: the queue
/// commit of the entry below it in the stack, or the base tip. Replayed
/// commits are committed strictly after `committed_after` (unix seconds) when
/// given, so a retry on an unmoved parent is still a new commit.
fn build_on(
    state: &WebState,
    owner: &str,
    repo: &str,
    entry: &mut QueueEntry,
    parent: &str,
    committed_after: Option<i64>,
) -> Result<(), ReplayFailure> {
    let resolved = state
        .repo_manager
        .open_parts(owner, repo)
        .map_err(|err| ReplayFailure::Git(err.to_string()))?;
    let git = git_for(state, &resolved.path);
    let queue_sha = git.replay(parent, &entry.pr_head_sha, committed_after)?;
    git.update_ref(&entry.queue_ref, &queue_sha)?;
    entry.base_sha = parent.to_string();
    entry.queue_sha = queue_sha.clone();
    entry.attempts.push(Attempt {
        queue_sha,
        base_sha: parent.to_string(),
        conclusion: None,
        at: now(),
    });
    // Only the forge knows this commit exists, so only the forge can tell the
    // gate runner to come and gate it instead of waiting for its next tick.
    super::gate_wanted::record_queue_commit(state, &entry.repo, entry.number, &entry.queue_sha);
    Ok(())
}

/// The base tip of `base` in `owner/repo`.
fn base_tip(
    state: &WebState,
    owner: &str,
    repo: &str,
    base: &str,
) -> Result<String, ReplayFailure> {
    let resolved = state
        .repo_manager
        .open_parts(owner, repo)
        .map_err(|err| ReplayFailure::Git(err.to_string()))?;
    git_for(state, &resolved.path)
        .resolve(&format!("refs/heads/{base}"))
        .ok_or_else(|| ReplayFailure::Git(format!("refs/heads/{base} does not resolve")))
}

/// A pull request with a head to act on: not a draft, not closed, not merged.
/// Shared with [`super::regate`], which asks the same question of a re-gate.
pub(super) fn is_open_pull(pr: &PullRequest) -> bool {
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
    if !is_open_pull(&pr) {
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
        refusal_code: None,
        landed_sha: None,
        after: None,
        landed_with: None,
    };
    if !is_sha(&entry.pr_head_sha) {
        return api_error(
            StatusCode::CONFLICT,
            "bad_head",
            "the pull request head is not a commit",
        );
    }
    // Join the stack: build on the queue commit of the last entry already
    // queued on this base, so this entry's gate also proves everything below
    // it, and its landing never has to wait for a rebuild.
    let tail = index
        .entries
        .iter()
        .filter(|((owner, name, number), queued)| {
            *owner == repo.owner
                && *name == repo.name
                && *number != number_of(&key)
                && queued.state == QueueState::Building
                && queued.base == entry.base
                && is_sha(&queued.queue_sha)
        })
        .map(|(_, queued)| queued)
        .max_by_key(|queued| stack_order(queued))
        .map(|queued| (queued.number, queued.queue_sha.clone()));
    let built = match &tail {
        Some((number, parent)) => {
            entry.after = Some(*number);
            build_on(&state, &repo.owner, &repo.name, &mut entry, parent, None)
        }
        None => base_tip(&state, &repo.owner, &repo.name, &entry.base)
            .and_then(|tip| build_on(&state, &repo.owner, &repo.name, &mut entry, &tip, None)),
    };
    if let Err(failure) = built {
        let code = match failure {
            ReplayFailure::Conflict(_) => "queue_conflict",
            ReplayFailure::Mismatch(_) => "queue_mismatch",
            ReplayFailure::MergeCommits => "queue_merge_commits",
            ReplayFailure::Git(_) => "queue_git_error",
        };
        // The PR cannot be replayed onto the base: somebody has to rebase it.
        // The refusal is persisted as a dequeued entry carrying the code, so
        // the attention inbox can name the one step that fixes it instead of
        // the refusal living only in this response and a `queue.refused` event.
        entry.state = QueueState::Dequeued;
        entry.reason = Some(failure.to_string());
        entry.refusal_code = Some(code.to_string());
        persist(&state, &repo.owner, &repo.name, &entry);
        index.entries.insert(key, entry.clone());
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
    #[serde(flatten)]
    paging: super::paging::PageParams,
}

/// `GET /api/v1/repos/:id/merge-queue` lists one repository's whole queue, so
/// it takes the paging keys and no filter of its own.
#[derive(Debug, Deserialize)]
pub(super) struct QueuePageQuery {
    #[serde(flatten)]
    paging: super::paging::PageParams,
}

impl strict_query::StrictFields for QueuePageQuery {
    const KEYS: &'static [&'static str] = &["limit", "per_page", "page"];
}

/// `state=all` asks for every state at once; it is not a state of its own.
const EVERY_STATE: &str = "all";

impl strict_query::StrictFields for QueueListQuery {
    const KEYS: &'static [&'static str] = &["state", "limit", "per_page", "page"];

    fn check_values(&self) -> Result<(), String> {
        let mut allowed = QueueState::names();
        allowed.push(EVERY_STATE);
        strict_query::filter_one_of("state", self.state.as_ref(), &allowed)
    }
}

/// `GET /api/v1/merge-queue?state=building&per_page=&page=` — every repository
/// the caller can read. The runner polls this on each tick, so it reads the
/// index only. A busy family's queue outgrows one response, so the listing is
/// paged like every other `/api/v1` collection (`docs/pagination.md`).
pub(super) async fn list_all(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    strict_query::StrictQuery(query): strict_query::StrictQuery<QueueListQuery>,
) -> AxumResponse {
    let page = match query.paging.page() {
        Ok(page) => page,
        Err(rejection) => return rejection.into_response(),
    };
    let wanted = query
        .state
        .map(|state| state.trim().to_string())
        .filter(|state| !state.is_empty())
        .unwrap_or_else(|| QueueState::Building.as_str().to_string());
    let entries = state.merge_queue.entries(&state, |entry| {
        (wanted == EVERY_STATE || entry.state.as_str() == wanted)
            && entry.repo.split_once('/').is_some_and(|(owner, name)| {
                account.role == UserRole::Admin
                    || state.core.user_can_read_repo(&account.login, owner, name)
            })
    });
    queue_page(page, entries)
}

/// `GET /api/v1/repos/:id/merge-queue?per_page=&page=`
pub(super) async fn list_repo(
    State(state): State<Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    strict_query::StrictQuery(query): strict_query::StrictQuery<QueuePageQuery>,
) -> AxumResponse {
    let page = match query.paging.page() {
        Ok(page) => page,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(repo) = super::repositories::find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    let full = format!("{}/{}", repo.owner, repo.name);
    let entries = state
        .merge_queue
        .entries(&state, |entry| entry.repo == full);
    let (entries, info) = page.apply(entries);
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|entry| with_gate_started_at(&state, &repo.owner, &repo.name, entry))
        .collect();
    Json(json!({ "entries": entries, "total": info.total, "page": info })).into_response()
}

/// A listed entry, and for a building one `gate_started_at`: when a gate first
/// reported on its queue commit after the enqueue, or null while no gate runner
/// has taken it. Without it a reader cannot tell an entry waiting for a free
/// runner from one being gated. Only this per-repository listing carries it:
/// the runner polls `list_all` on every tick, which stays an index read.
fn with_gate_started_at(state: &WebState, owner: &str, repo: &str, entry: QueueEntry) -> Value {
    let building = entry.state == QueueState::Building;
    let started = building
        .then(|| {
            let enqueued = chrono::DateTime::parse_from_rfc3339(&entry.enqueued_at)
                .ok()?
                .with_timezone(&chrono::Utc);
            let pr = state
                .core
                .get_pull_request(owner, repo, entry.number)
                .ok()?;
            let required = super::pulls::required_context_names(state, &pr);
            gate_started_at(
                &state.core,
                owner,
                repo,
                &entry.queue_sha,
                &required,
                enqueued,
            )
        })
        .flatten();
    let mut value = serde_json::to_value(&entry).unwrap_or(Value::Null);
    if building && let Some(object) = value.as_object_mut() {
        object.insert(
            "gate_started_at".to_string(),
            started.map_or(Value::Null, |at| Value::String(at.to_rfc3339())),
        );
    }
    value
}

/// The earliest commit status or check run on `sha` among `required` (among
/// every context when the base requires none, as the gate verdict also counts
/// any reported context then) that was posted at or after `since`, the enqueue.
/// The queue commit is not always the forge's own: a PR already on the base tip
/// lands as itself, so `sha` is the PR head and carries the PR's own CI reports
/// from before the enqueue. Those say nothing about a queue gate, so only a
/// report from the enqueue on counts.
pub(crate) fn gate_started_at(
    core: &jeryu_core::ForgeCore,
    owner: &str,
    repo: &str,
    sha: &str,
    required: &std::collections::BTreeSet<String>,
    since: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let counts = |name: &str| required.is_empty() || required.contains(name);
    let statuses = core.combined_status(owner, repo, sha).ok()?.statuses;
    let checks = core
        .list_check_runs(owner, repo, Some(sha))
        .ok()?
        .check_runs;
    statuses
        .iter()
        .filter(|status| counts(&status.context))
        .map(|status| status.created_at)
        .chain(
            checks
                .iter()
                .filter(|check| counts(&check.name))
                .map(|check| check.started_at),
        )
        .filter(|at| *at >= since)
        .min()
}

/// One page of queue entries, carrying the `total` and `page` object every
/// paged `/api/v1` listing answers with.
fn queue_page(page: super::paging::Page, entries: Vec<QueueEntry>) -> AxumResponse {
    let (entries, info) = page.apply(entries);
    Json(json!({ "entries": entries, "total": info.total, "page": info })).into_response()
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
    gate_verdict(&state.core, owner, repo, sha, required)
}

/// [`gate_result`] against a bare core handle, for callers outside the queue
/// (a direct merge that replays onto a moved base must read the same verdict).
pub(crate) fn gate_verdict(
    core: &jeryu_core::ForgeCore,
    owner: &str,
    repo: &str,
    sha: &str,
    required: &[String],
) -> Option<bool> {
    use jeryu_core::{CheckConclusion, CheckRunStatus, CommitStatusState};
    let statuses = core.combined_status(owner, repo, sha).ok()?.statuses;
    let checks = core
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

/// Advance every building entry once, one stack per base: dequeue stale ones,
/// restack entries whose base or lower entries moved, land the highest green
/// entry together with everything below it, and retry the lowest red one.
/// Returns how many entries changed state.
pub(crate) fn tick(state: &WebState) -> usize {
    let queue = &state.merge_queue;
    let mut index = queue.lock();
    queue.ensure_loaded(state, &mut index);
    let mut stacks: BTreeMap<(String, String, String), Vec<Key>> = BTreeMap::new();
    let mut before = BTreeMap::new();
    for (key, entry) in &index.entries {
        if entry.state == QueueState::Building {
            stacks
                .entry((key.0.clone(), key.1.clone(), entry.base.clone()))
                .or_default()
                .push(key.clone());
            before.insert(key.clone(), entry.state);
        }
    }
    for ((owner, repo, base), mut keys) in stacks {
        keys.sort_by_key(|key| stack_order(&index.entries[key]));
        advance_stack(state, &owner, &repo, &base, &keys, &mut index.entries);
    }
    before
        .iter()
        .filter(|(key, state)| index.entries.get(*key).is_some_and(|e| e.state != **state))
        .count()
}

fn number_of(key: &Key) -> u64 {
    key.2
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

/// The checks that need no other entry: the PR is still open, at the head
/// that was queued, and still passes its merge gate. Returns the base's
/// required contexts while the entry stays building.
fn still_queued(
    state: &WebState,
    owner: &str,
    repo: &str,
    entry: &mut QueueEntry,
) -> Option<Vec<String>> {
    let pr = state
        .core
        .get_pull_request(owner, repo, entry.number)
        .ok()?;
    if !is_open_pull(&pr) {
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
        return None;
    }
    if pr.head.sha != entry.pr_head_sha {
        finish(
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
        return None;
    }
    let (pass, blockers, required) = super::pulls::queue_gate(state, &pr);
    if !pass {
        finish(
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
        return None;
    }
    Some(required)
}

fn advance_stack(
    state: &WebState,
    owner: &str,
    repo: &str,
    base: &str,
    keys: &[Key],
    entries: &mut BTreeMap<Key, QueueEntry>,
) {
    let mut required = BTreeMap::new();
    for key in keys {
        if let Some(entry) = entries.get_mut(key)
            && let Some(contexts) = still_queued(state, owner, repo, entry)
        {
            required.insert(key.clone(), contexts);
        }
    }
    let live = |entries: &BTreeMap<Key, QueueEntry>| -> Vec<Key> {
        keys.iter()
            .filter(|key| {
                required.contains_key(*key)
                    && entries
                        .get(*key)
                        .is_some_and(|e| e.state == QueueState::Building)
            })
            .cloned()
            .collect()
    };
    restack(state, owner, repo, base, &live(entries), entries);
    let stack = live(entries);
    let verdicts: Vec<Option<bool>> = stack
        .iter()
        .map(|key| gate_result(state, owner, repo, &entries[key].queue_sha, &required[key]))
        .collect();
    if let Some(top) = verdicts.iter().rposition(|verdict| *verdict == Some(true)) {
        land_stack(
            state,
            owner,
            repo,
            &stack[..=top],
            &verdicts[..=top],
            entries,
        );
    } else if let Some(red) = verdicts.iter().position(|verdict| *verdict == Some(false))
        && let Some(entry) = entries.get_mut(&stack[red])
    {
        retry_or_fail(state, owner, repo, entry);
    } else {
        return;
    }
    // Whatever landed or failed, the entries above it now sit on a commit
    // that is no longer below them.
    restack(state, owner, repo, base, &live(entries), entries);
}

/// Make the stack a chain again: the first entry built on the base tip, each
/// next one on the queue commit of the one below it. An entry whose parent
/// changed is rebuilt (a new commit, so a new gate); one that no longer
/// replays is dequeued and the next one takes its place.
fn restack(
    state: &WebState,
    owner: &str,
    repo: &str,
    base: &str,
    stack: &[Key],
    entries: &mut BTreeMap<Key, QueueEntry>,
) {
    let Ok(mut parent) = base_tip(state, owner, repo, base) else {
        return;
    };
    let mut after: Option<u64> = None;
    for key in stack {
        let Some(entry) = entries.get_mut(key) else {
            continue;
        };
        if entry.state != QueueState::Building {
            continue;
        }
        if entry.base_sha != parent || !is_sha(&entry.queue_sha) {
            if let Some(last) = entry.attempts.last_mut()
                && last.conclusion.is_none()
            {
                last.conclusion = Some(SUPERSEDED.to_string());
            }
            let why = match after {
                Some(below) => format!("the queue below it changed (now on #{below})"),
                None => "the base moved".to_string(),
            };
            if let Err(failure) = build_on(state, owner, repo, entry, &parent, None) {
                finish(
                    state,
                    owner,
                    repo,
                    entry,
                    QueueState::Dequeued,
                    failure.to_string(),
                );
                continue;
            }
            entry.after = after;
            persist(state, owner, repo, entry);
            emit_queue(
                state,
                entry,
                "queue.building",
                "merge-queue",
                false,
                format!("{}#{} rebuilt because {why}", entry.repo, entry.number),
            );
        }
        parent = entry.queue_sha.clone();
        after = Some(entry.number);
    }
}

/// The lowest red entry of a stack: rebuilt once as a fresh commit on the
/// same parent, then failed. The entries above it are restacked by the caller.
fn retry_or_fail(state: &WebState, owner: &str, repo: &str, entry: &mut QueueEntry) {
    if let Some(last) = entry.attempts.last_mut() {
        last.conclusion = Some("failure".to_string());
    }
    let red = entry
        .attempts
        .iter()
        .filter(|attempt| attempt.conclusion.as_deref() == Some("failure"))
        .count();
    // A PR already on the tip is its own queue commit: no fresh commit to
    // retry with.
    if red < MAX_ATTEMPTS && entry.queue_sha != entry.pr_head_sha {
        let parent = entry.base_sha.clone();
        let rebuilt = state
            .repo_manager
            .open_parts(owner, repo)
            .map_err(|err| ReplayFailure::Git(err.to_string()))
            .and_then(|resolved| git_for(state, &resolved.path).committer_time(&entry.queue_sha))
            .and_then(|previous| build_on(state, owner, repo, entry, &parent, Some(previous)));
        match rebuilt {
            Err(failure) => finish(
                state,
                owner,
                repo,
                entry,
                QueueState::Dequeued,
                failure.to_string(),
            ),
            Ok(()) => {
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
        }
        return;
    }
    let tried: Vec<&str> = entry
        .attempts
        .iter()
        .filter(|attempt| attempt.conclusion.as_deref() == Some("failure"))
        .map(|attempt| attempt.queue_sha.as_str())
        .collect();
    let reason = format!("the gate failed on {}", tried.join(" and "));
    finish(state, owner, repo, entry, QueueState::Failed, reason);
}

/// Land `stack` (bottom first) on one gate: the top entry's, green, which
/// covers every entry below it. `verdicts` are the entries' own gates.
fn land_stack(
    state: &WebState,
    owner: &str,
    repo: &str,
    stack: &[Key],
    verdicts: &[Option<bool>],
    entries: &mut BTreeMap<Key, QueueEntry>,
) {
    let members: Vec<crate::github::pulls::QueuedLanding> = stack
        .iter()
        .map(|key| {
            let entry = &entries[key];
            crate::github::pulls::QueuedLanding {
                number: entry.number,
                pr_head: entry.pr_head_sha.clone(),
                queue_sha: entry.queue_sha.clone(),
            }
        })
        .collect();
    let expected_base = entries[&stack[0]].base_sha.clone();
    let top = members.last().map(|m| m.number).unwrap_or_default();
    match state
        .github
        .land_queued_batch(owner, repo, &members, &expected_base)
    {
        Ok(tip) => {
            for (key, verdict) in stack.iter().zip(verdicts) {
                let Some(entry) = entries.get_mut(key) else {
                    continue;
                };
                landed(state, owner, repo, entry, &tip, top, *verdict == Some(true));
            }
        }
        // Restacking onto the new tip rebuilds the whole stack.
        Err(crate::github::pulls::LandRefusal::BaseMoved(_)) => {}
        Err(crate::github::pulls::LandRefusal::MemberBlocked(number, reason)) => {
            if let Some(entry) = stack
                .iter()
                .find(|key| number_of(key) == number)
                .and_then(|key| entries.get_mut(key))
            {
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
        Err(crate::github::pulls::LandRefusal::Blocked(reason)) => {
            for key in stack {
                if let Some(entry) = entries.get_mut(key) {
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
    }
}

fn landed(
    state: &WebState,
    owner: &str,
    repo: &str,
    entry: &mut QueueEntry,
    tip: &str,
    top: u64,
    own_gate_green: bool,
) {
    let covered = entry.number != top;
    if let Some(last) = entry.attempts.last_mut() {
        last.conclusion = Some(if own_gate_green || !covered {
            "success".to_string()
        } else {
            format!("covered by #{top}")
        });
    }
    let at = if covered {
        entry.queue_sha.clone()
    } else {
        tip.to_string()
    };
    entry.landed_sha = Some(at.clone());
    entry.landed_with = covered.then_some(top);
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
    let approved_by = if approved_by.is_empty() {
        "nobody".to_string()
    } else {
        approved_by.join(", ")
    };
    let reason = if covered {
        format!(
            "landed at {at} with #{top}, whose green gate at {tip} covers it (replay of {}; approved by {approved_by})",
            entry.pr_head_sha
        )
    } else {
        format!(
            "landed at {at} (replay of {} gated green; approved by {approved_by})",
            entry.pr_head_sha
        )
    };
    finish(state, owner, repo, entry, QueueState::Landed, reason);
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
