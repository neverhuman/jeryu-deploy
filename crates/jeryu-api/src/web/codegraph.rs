//! Codegraph oracle REST facade.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{error::Error, fmt};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use jeryu_codegraph::{
    CodeGraphImpactPack, CodeGraphQuery, CodeGraphRepoIdentity, CodeGraphService, CodeGraphStore,
    CodegraphQuery, query_store,
};
use serde_json::{Value, json};

use super::WebState;
use super::repositories::find_repo;

pub(super) fn query_compat_pack_for_repo(
    state: &Arc<WebState>,
    id: &str,
    request: CodegraphQuery,
) -> Result<jeryu_codegraph::CodegraphImpactPack, String> {
    if find_repo(state, id).is_none() {
        return Err(format!("repository {id} not found for codegraph query"));
    }
    query_store(&state.codegraph_store, &request).map_err(|err| err.to_string())
}

pub(super) async fn query(
    State(state): State<Arc<WebState>>,
    AxumPath(id): AxumPath<String>,
    body: Bytes,
) -> AxumResponse {
    if find_repo(&state, &id).is_none() {
        return codegraph_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "query repository codegraph",
            "repository not found for codegraph query",
            &[
                "verify the repository id or owner/name pair",
                "refresh the local forge import before retrying",
            ],
            "rerun cargo test -p jeryu-api --features web --jobs 40 codegraph",
        );
    }
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
            return codegraph_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "codegraph_invalid_request",
                "query repository codegraph",
                &err.to_string(),
                &[
                    "send a JSON object matching the codegraph query contract",
                    "use changed paths as repo-relative strings",
                ],
                "fix the request body, then rerun the codegraph API proof lane",
            );
        }
    };

    if is_rich_query(&value) {
        let request: CodeGraphQuery = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(err) => {
                return codegraph_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "codegraph_invalid_request",
                    "query repository codegraph",
                    &err.to_string(),
                    &[
                        "send ref and changed_paths fields for the rich codegraph oracle",
                        "use changed_paths as repo-relative paths",
                    ],
                    "fix the request body, then rerun the codegraph API proof lane",
                );
            }
        };
        return match query_pack_for_repo(&state, &id, request) {
            Ok(pack) => Json(pack).into_response(),
            Err(error) => error.into_response(),
        };
    }

    let request: CodegraphQuery = match serde_json::from_value(value) {
        Ok(request) => request,
        Err(err) => {
            return codegraph_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "codegraph_invalid_request",
                "query repository codegraph",
                &err.to_string(),
                &[
                    "send changed_paths as an array of repo-relative strings",
                    "send symbol and crate_name as strings when filtering the oracle pack",
                ],
                "fix the request body, then rerun the codegraph API proof lane",
            );
        }
    };
    match query_compat_pack_for_repo(&state, &id, request) {
        Ok(pack) => Json(pack).into_response(),
        Err(err) => codegraph_error(
            StatusCode::FAILED_DEPENDENCY,
            "codegraph_query_failed",
            "query repository codegraph",
            &err,
            &[
                "rerun jeryu-codegraph index before querying",
                "inspect the auxiliary codegraph SQLite store",
            ],
            "rerun cargo test -p jeryu-codegraph -p jeryu-mcp --jobs 40 code",
        ),
    }
}

fn is_rich_query(value: &Value) -> bool {
    ["ref", "intent", "question", "max_tokens"]
        .iter()
        .any(|key| value.get(*key).is_some())
}

pub(super) fn query_pack_for_repo(
    state: &WebState,
    id: &str,
    query: CodeGraphQuery,
) -> std::result::Result<CodeGraphImpactPack, CodeGraphRouteError> {
    let repo =
        find_repo(state, id).ok_or(CodeGraphRouteError::RepoNotFound { id: id.to_string() })?;
    let bare = bare_repo_path(state, &repo)?;
    let git_bin = state.repo_manager.config().git_bin.clone();
    let commit = resolve_ref(&git_bin, &bare, &query.ref_name)?;
    let checkout = TempCheckout::new(&repo.full_name);
    materialize_checkout(&git_bin, &bare, &checkout.path, &commit)?;

    let store = open_repo_store(&bare)?;
    let service = CodeGraphService::new(checkout.path.clone(), store);
    service
        .query(
            CodeGraphRepoIdentity {
                id: repo.id.to_string(),
                owner: repo.owner.clone(),
                name: repo.name.clone(),
            },
            commit,
            query,
        )
        .map_err(|err| CodeGraphRouteError::IndexFailed(err.to_string()))
}

/// The bare repository Jeryu git storage holds for `repo`.
fn bare_repo_path(
    state: &WebState,
    repo: &jeryu_core::Repository,
) -> std::result::Result<PathBuf, CodeGraphRouteError> {
    state
        .repo_manager
        .open_parts(&repo.owner, &repo.name)
        .map(|managed| managed.path)
        .map_err(|err| CodeGraphRouteError::RepoNotFound {
            id: format!("{} ({err})", repo.full_name),
        })
}

/// Where one repository's own codegraph index lives.
///
/// The symbol, reference and crate-dependency tables carry no repository
/// column, so one database per repository is what scopes a symbol read to the
/// repository the caller named. `WebState::codegraph_store` stays the
/// cross-repo store the tool-build cluster reads use.
pub(super) fn repo_store_path(bare: &Path) -> PathBuf {
    bare.join("jeryu").join("codegraph.sqlite")
}

/// Open (creating the file and schema when absent) one repository's index.
pub(super) fn open_repo_store(
    bare: &Path,
) -> std::result::Result<CodeGraphStore, CodeGraphRouteError> {
    CodeGraphStore::open(repo_store_path(bare))
        .map_err(|err| CodeGraphRouteError::IndexFailed(err.to_string()))
}

/// Build or refresh `id`'s index from `ref_name`, returning the symbol count
/// the refresh persisted.
///
/// The refresh writes the whole snapshot in one transaction, so an interrupted
/// run leaves the previous index in place rather than a half-written one.
pub(super) fn refresh_repo_index(
    state: &WebState,
    id: &str,
    ref_name: &str,
) -> std::result::Result<usize, CodeGraphRouteError> {
    let pack = query_pack_for_repo(
        state,
        id,
        CodeGraphQuery {
            ref_name: ref_name.to_string(),
            ..CodeGraphQuery::default()
        },
    )?;
    Ok(pack.graph_stats.symbol_count)
}

/// The graph a code tool reads for the repository it was given.
///
/// Reads that repository's own index. When it holds no symbols yet -- a
/// repository whose first push predates indexing, or a forge that has not run
/// a refresh -- it indexes `ref_name` (the repository's default branch when
/// the caller named none) once and reads the result back, so a symbol search
/// answers instead of returning nothing. A caller that names a ref the index
/// was not built from gets a refresh from that ref, and so does a caller that
/// names none after a read of another ref left the index on it.
///
/// A rebuild is a full checkout and walk of the repository, and the ref comes
/// from the caller, so rebuilds are bounded: one at a time per repository
/// ([`rebuild_lock`]), at most [`REBUILD_PARALLEL`] across the process, and a
/// repository is rebuilt for a ref other than its default branch at most once
/// per [`OTHER_REF_REBUILD_EVERY`]. A read of the default branch is never
/// throttled, so it always restores the index the scheduler and push refreshes
/// keep.
///
/// A read never waits for a rebuild. It is served on a thread of the async
/// runtime that also carries git and the rest of the API, so a read that finds
/// this repository already being rebuilt, or the process at its rebuild limit,
/// answers with a retryable error instead of holding that thread for a clone
/// and a workspace walk.
pub(super) fn repo_graph(
    state: &WebState,
    id: &str,
    ref_name: Option<&str>,
) -> std::result::Result<jeryu_codegraph::CodeGraph, CodeGraphRouteError> {
    let repo =
        find_repo(state, id).ok_or(CodeGraphRouteError::RepoNotFound { id: id.to_string() })?;
    let bare = bare_repo_path(state, &repo)?;
    let store = open_repo_store(&bare)?;
    let snapshot = store
        .load_snapshot()
        .map_err(|err| CodeGraphRouteError::IndexFailed(err.to_string()))?;
    // An index built from another ref than this read is about answers for the
    // wrong code, so it is rebuilt. The ref the last refresh recorded is the
    // receipt of what the rows hold, and a caller that named no ref is asking
    // about the default branch.
    let wanted_ref = ref_name.unwrap_or(repo.default_branch.as_str());
    let indexed_ref = snapshot
        .index_runs
        .last()
        .map(|run| run.ref_name.as_str())
        .unwrap_or_default();
    if !snapshot.symbols.is_empty() && indexed_ref == wanted_ref {
        return Ok(jeryu_codegraph::CodeGraph::from_snapshot(snapshot));
    }
    let default_branch = wanted_ref == repo.default_branch;
    // Only this repository's rebuilds are serialized, and the read does not
    // wait: a rebuild already running here means a retryable answer now.
    let lock = rebuild_lock(&bare);
    let Some(_one_at_a_time) = try_claim(&lock) else {
        return Err(CodeGraphRouteError::RebuildInProgress {
            retry_after_secs: REBUILD_RETRY_AFTER_SECS,
        });
    };
    // Another read or a refresh may have rebuilt this very ref since the
    // snapshot above was loaded, so the claim is spent on what is still stale.
    let snapshot = store
        .load_snapshot()
        .map_err(|err| CodeGraphRouteError::IndexFailed(err.to_string()))?;
    let indexed_ref = snapshot
        .index_runs
        .last()
        .map(|run| run.ref_name.as_str())
        .unwrap_or_default()
        .to_string();
    if !snapshot.symbols.is_empty() && indexed_ref == wanted_ref {
        return Ok(jeryu_codegraph::CodeGraph::from_snapshot(snapshot));
    }
    // Restoring the default branch over another ref's index is always allowed:
    // it is already bounded by the other-ref throttle. Rebuilding an index that
    // is about the default branch but holds no symbol (nothing the analyzer
    // reads, or a failed run) is throttled, and the read gets that empty answer.
    let restoring_default = default_branch && !indexed_ref.is_empty() && indexed_ref != wanted_ref;
    if !restoring_default && let Err(wait) = claim_rebuild(&bare, default_branch) {
        if default_branch {
            // An index that was never built (or whose build failed) records no
            // ref and holds no symbol; answering from it would say "no such
            // symbol" for what is really "not indexed yet".
            if snapshot.symbols.is_empty() && indexed_ref.is_empty() {
                return Err(CodeGraphRouteError::NotIndexed {
                    retry_after_secs: wait.as_secs().max(1),
                });
            }
            return Ok(jeryu_codegraph::CodeGraph::from_snapshot(snapshot));
        }
        return Err(CodeGraphRouteError::RebuildThrottled {
            retry_after_secs: wait.as_secs().max(1),
        });
    }
    // Rebuilds of different repositories are bounded too: each is a clone and
    // a workspace walk, so the process runs only so many at once and a read
    // that finds them all busy is told to retry rather than queued.
    let Some(_slot) = REBUILD_SLOTS.try_take() else {
        return Err(CodeGraphRouteError::RebuildCapacity {
            retry_after_secs: REBUILD_RETRY_AFTER_SECS,
        });
    };
    refresh_repo_index(state, id, wanted_ref)?;
    let snapshot = store
        .load_snapshot()
        .map_err(|err| CodeGraphRouteError::IndexFailed(err.to_string()))?;
    Ok(jeryu_codegraph::CodeGraph::from_snapshot(snapshot))
}

/// How long a caller is asked to wait when a rebuild it wanted is already
/// running, or the process is at its rebuild limit.
const REBUILD_RETRY_AFTER_SECS: u64 = 5;

/// How many index rebuilds this process runs at once. Each is a clone and a
/// full workspace walk, so the whole host is not handed to indexing.
const REBUILD_PARALLEL: usize = 2;

/// One index rebuild at a time per repository, by its bare path.
///
/// Per repository rather than per process: a rebuild clones and walks a whole
/// workspace, so a single process-wide lock made a read of one repository wait
/// out an unrelated repository's rebuild.
static REBUILD_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// How many repository locks are kept before the unheld ones are dropped. The
/// map holds one entry per repository read in this process, so it is pruned
/// rather than left to grow with every repository that ever appears.
const REBUILD_LOCKS_KEPT: usize = 1024;

/// This repository's rebuild lock, shared by the read and the refresh paths.
fn rebuild_lock(bare: &Path) -> Arc<Mutex<()>> {
    rebuild_lock_in(&REBUILD_LOCKS, bare)
}

/// [`rebuild_lock`] over a given lock map, so a test can prune its own map
/// instead of the process-wide one other tests share.
fn rebuild_lock_in(map: &Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>, bare: &Path) -> Arc<Mutex<()>> {
    let mut locks = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if locks.len() >= REBUILD_LOCKS_KEPT {
        // An entry no caller holds guards nothing: dropping it loses no claim,
        // and the next rebuild of that repository makes a fresh one.
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
    Arc::clone(locks.entry(bare.to_path_buf()).or_default())
}

/// Claim a rebuild lock without waiting, or `None` when a rebuild holds it.
///
/// A lock a panicking rebuild poisoned is claimable: one failed run must not
/// leave a repository unindexable for the life of the process.
fn try_claim(lock: &Mutex<()>) -> Option<std::sync::MutexGuard<'_, ()>> {
    match lock.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// The rebuilds running in this process, at most [`REBUILD_PARALLEL`].
static REBUILD_SLOTS: RebuildSlots = RebuildSlots::new();

struct RebuildSlots {
    in_flight: Mutex<usize>,
    freed: Condvar,
}

impl RebuildSlots {
    const fn new() -> Self {
        Self {
            in_flight: Mutex::new(0),
            freed: Condvar::new(),
        }
    }

    fn count(&self) -> std::sync::MutexGuard<'_, usize> {
        self.in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Take a slot if one is free. For reads, which must not wait.
    fn try_take(&'static self) -> Option<RebuildSlot> {
        let mut in_flight = self.count();
        if *in_flight >= REBUILD_PARALLEL {
            return None;
        }
        *in_flight += 1;
        Some(RebuildSlot(self))
    }

    /// Wait for a slot. For push- and timer-driven refreshes, which run on a
    /// blocking thread and must not drop the work they were asked to do.
    fn take(&'static self) -> RebuildSlot {
        let mut in_flight = self.count();
        while *in_flight >= REBUILD_PARALLEL {
            in_flight = self
                .freed
                .wait(in_flight)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *in_flight += 1;
        RebuildSlot(self)
    }
}

/// A slot held for one rebuild, returned on drop so a panicking rebuild does
/// not retire it.
struct RebuildSlot(&'static RebuildSlots);

impl Drop for RebuildSlot {
    fn drop(&mut self) {
        let mut in_flight = self.0.count();
        *in_flight = in_flight.saturating_sub(1);
        self.0.freed.notify_one();
    }
}

/// How often a read may rebuild one repository's index: once per period for
/// its default branch and once per period for all its other refs together.
pub(super) const OTHER_REF_REBUILD_EVERY: Duration = Duration::from_secs(30);

/// When each repository (by its bare path) was last rebuilt by a read, for its
/// default branch (`true`) or for any other ref (`false`).
static READ_REBUILDS: LazyLock<Mutex<HashMap<(PathBuf, bool), Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Claim this repository's next read-driven rebuild of this kind of ref, or
/// say how long to wait. A repository whose index stays empty (nothing the
/// analyzer reads) therefore costs one checkout per period, not one per read.
fn claim_rebuild(bare: &Path, default_branch: bool) -> std::result::Result<(), Duration> {
    let now = Instant::now();
    let mut last = READ_REBUILDS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = (bare.to_path_buf(), default_branch);
    if let Some(at) = last.get(&key)
        && now.duration_since(*at) < OTHER_REF_REBUILD_EVERY
    {
        return Err(OTHER_REF_REBUILD_EVERY - now.duration_since(*at));
    }
    last.insert(key, now);
    Ok(())
}

/// A push- or timer-driven refresh, under the same per-repository lock as the
/// read path. A repository whose rebuild is already running is skipped, not
/// queued: the running one reads the newest ref, and the next push or tick
/// catches anything later. `None` when skipped.
///
/// The lock is released on drop and a lock a panicking run poisoned is still
/// claimable, so one failed indexing run cannot stop every later refresh of
/// that repository.
pub(super) fn refresh_scheduled(
    state: &WebState,
    id: &str,
    ref_name: &str,
) -> Option<std::result::Result<usize, CodeGraphRouteError>> {
    let Some(bare) = find_repo(state, id).and_then(|repo| bare_repo_path(state, &repo).ok()) else {
        // Nothing to lock and nothing to index: let the refresh report why.
        return Some(refresh_repo_index(state, id, ref_name));
    };
    let lock = rebuild_lock(&bare);
    let _one_at_a_time = try_claim(&lock)?;
    // Unlike a read, a refresh waits for a slot: it runs on a blocking thread,
    // and dropping it would leave this repository stale until the next tick.
    let _slot = REBUILD_SLOTS.take();
    Some(refresh_repo_index(state, id, ref_name))
}

/// Whether `bare`'s index holds any symbol at all. Cheaper than loading the
/// snapshot: the scheduler asks it once per repository per tick.
pub(super) fn repo_index_is_empty(bare: &Path) -> std::result::Result<bool, String> {
    let store = CodeGraphStore::open(repo_store_path(bare)).map_err(|err| err.to_string())?;
    store
        .search_symbols("", 1)
        .map(|rows| rows.is_empty())
        .map_err(|err| err.to_string())
}

#[derive(Debug)]
pub(super) enum CodeGraphRouteError {
    RepoNotFound {
        id: String,
    },
    InvalidRef {
        ref_name: String,
        reason: String,
    },
    MaterializeFailed(String),
    IndexFailed(String),
    /// A ref other than the default branch was indexed for this repository
    /// too recently to rebuild again.
    RebuildThrottled {
        retry_after_secs: u64,
    },
    /// The default branch has no index yet and a rebuild was attempted too
    /// recently to try again.
    NotIndexed {
        retry_after_secs: u64,
    },
    /// This repository's index is being rebuilt right now, and a read does not
    /// wait for a rebuild.
    RebuildInProgress {
        retry_after_secs: u64,
    },
    /// The process is already running as many index rebuilds as it allows.
    RebuildCapacity {
        retry_after_secs: u64,
    },
}

impl CodeGraphRouteError {
    fn into_response(self) -> AxumResponse {
        match self {
            Self::RepoNotFound { id } => codegraph_error(
                StatusCode::NOT_FOUND,
                "not_found",
                "load repository for codegraph query",
                &format!("repository {id} is not registered or lacks a materialized bare repo"),
                &[
                    "verify the repository id or owner/name pair",
                    "import or create the repository before querying codegraph",
                ],
                "rerun cargo test -p jeryu-api --features web --jobs 40 codegraph",
            ),
            Self::InvalidRef { ref_name, reason } => codegraph_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_ref",
                "resolve codegraph query ref to a commit",
                &format!("ref {ref_name} did not resolve to a commit: {reason}"),
                &[
                    "use a branch, tag, or commit reachable from the managed repository",
                    "refresh the local import before retrying",
                ],
                "rerun cargo test -p jeryu-api --features web --jobs 40 codegraph_invalid_ref",
            ),
            Self::MaterializeFailed(reason) => codegraph_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "codegraph_materialize_failed",
                "materialize isolated checkout for codegraph indexing",
                &reason,
                &[
                    "verify git is installed and the bare repo is healthy",
                    "retry after importing the repository into Jeryu git storage",
                ],
                "rerun cargo test -p jeryu-api --features web --jobs 40 codegraph",
            ),
            Self::RebuildThrottled { retry_after_secs } => codegraph_error(
                StatusCode::TOO_MANY_REQUESTS,
                "codegraph_rebuild_throttled",
                "rebuild a repository index for a ref other than its default branch",
                &format!(
                    "this repository was indexed for another ref under {} seconds ago; retry in {retry_after_secs} seconds",
                    OTHER_REF_REBUILD_EVERY.as_secs()
                ),
                &[
                    "read the default branch, which is always available",
                    "retry the other ref after the wait",
                ],
                "rerun cargo test -p jeryu-api --features web codegraph",
            ),
            Self::NotIndexed { retry_after_secs } => codegraph_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "codegraph_not_indexed",
                "read a repository index that has not been built",
                &format!(
                    "this repository has no codegraph index yet and the last attempt to build it was under {} seconds ago; retry in {retry_after_secs} seconds",
                    OTHER_REF_REBUILD_EVERY.as_secs()
                ),
                &[
                    "retry after the wait; an empty answer would not mean the symbol is absent",
                    "check that the repository holds Rust/Cargo sources the analyzer reads",
                ],
                "rerun cargo test -p jeryu-api --features web codegraph",
            ),
            Self::RebuildInProgress { retry_after_secs } => codegraph_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "codegraph_rebuild_in_progress",
                "read a repository index another rebuild is writing",
                &format!(
                    "this repository's index is being rebuilt; retry in {retry_after_secs} seconds"
                ),
                &[
                    "retry after the wait; the rebuild in flight is writing the answer",
                    "name no ref to read the default branch once the rebuild lands",
                ],
                "rerun cargo test --offline -p jeryu-api --features web codegraph",
            ),
            Self::RebuildCapacity { retry_after_secs } => codegraph_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "codegraph_rebuild_capacity",
                "start a repository index rebuild",
                &format!(
                    "this server is already running the {REBUILD_PARALLEL} index rebuilds it allows; retry in {retry_after_secs} seconds"
                ),
                &[
                    "retry after the wait",
                    "let the scheduled refresh keep this repository indexed so reads need no rebuild",
                ],
                "rerun cargo test --offline -p jeryu-api --features web codegraph",
            ),
            Self::IndexFailed(reason) => codegraph_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "codegraph_index_failed",
                "build Rust/Cargo codegraph impact pack",
                &reason,
                &[
                    "verify the repository has a Cargo workspace for v1 Rust/Cargo indexing",
                    "inspect malformed Jankurai governance files before retrying",
                ],
                "rerun cargo test -p jeryu-codegraph --jobs 40",
            ),
        }
    }
}

impl fmt::Display for CodeGraphRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepoNotFound { id } => write!(f, "repository not found: {id}"),
            Self::InvalidRef { ref_name, reason } => {
                write!(f, "ref {ref_name} did not resolve to a commit: {reason}")
            }
            Self::MaterializeFailed(reason) => write!(f, "materialize checkout failed: {reason}"),
            Self::IndexFailed(reason) => write!(f, "codegraph index failed: {reason}"),
            Self::RebuildThrottled { retry_after_secs } => write!(
                f,
                "this repository was just indexed for another ref; retry in {retry_after_secs} seconds"
            ),
            Self::NotIndexed { retry_after_secs } => write!(
                f,
                "this repository is not indexed yet; retry in {retry_after_secs} seconds"
            ),
            Self::RebuildInProgress { retry_after_secs } => write!(
                f,
                "this repository's index is being rebuilt; retry in {retry_after_secs} seconds"
            ),
            Self::RebuildCapacity { retry_after_secs } => write!(
                f,
                "this server is already running the {REBUILD_PARALLEL} index rebuilds it allows; retry in {retry_after_secs} seconds"
            ),
        }
    }
}

impl Error for CodeGraphRouteError {}

fn resolve_ref(
    git_bin: &str,
    bare_repo: &Path,
    ref_name: &str,
) -> std::result::Result<String, CodeGraphRouteError> {
    // The ref is caller-supplied and becomes a git argument, so a name git
    // could read as an option is refused before it gets there.
    if let Some(reason) = ref_name_refusal(ref_name) {
        return Err(CodeGraphRouteError::InvalidRef {
            ref_name: ref_name.to_string(),
            reason: reason.to_string(),
        });
    }
    let git_dir = format!("--git-dir={}", bare_repo.display());
    let rev = format!("{ref_name}^{{commit}}");
    run_git(git_bin, &[&git_dir, "rev-parse", "--verify", &rev], None).map_err(|reason| {
        CodeGraphRouteError::InvalidRef {
            ref_name: ref_name.to_string(),
            reason,
        }
    })
}

/// Why a requested ref name cannot be passed to git, or `None` when it can.
fn ref_name_refusal(ref_name: &str) -> Option<&'static str> {
    if ref_name.is_empty() {
        return Some("a ref name is required");
    }
    if ref_name.starts_with('-') {
        return Some("a ref name must not start with '-'");
    }
    if ref_name
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Some("a ref name must not contain whitespace or control characters");
    }
    None
}

fn materialize_checkout(
    git_bin: &str,
    bare_repo: &Path,
    checkout: &Path,
    commit: &str,
) -> std::result::Result<(), CodeGraphRouteError> {
    let bare = bare_repo.display().to_string();
    let checkout_path = checkout.display().to_string();
    run_git(
        git_bin,
        &["clone", "--quiet", "--no-checkout", &bare, &checkout_path],
        None,
    )
    .map_err(CodeGraphRouteError::MaterializeFailed)?;
    run_git(
        git_bin,
        &["-C", &checkout_path, "checkout", "--quiet", commit],
        None,
    )
    .map_err(CodeGraphRouteError::MaterializeFailed)?;
    Ok(())
}

fn run_git(
    git_bin: &str,
    args: &[&str],
    cwd: Option<&Path>,
) -> std::result::Result<String, String> {
    let mut command = Command::new(git_bin);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .map_err(|err| format!("git invocation failed: {err}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

struct TempCheckout {
    path: PathBuf,
}

impl TempCheckout {
    fn new(repo_id: &str) -> Self {
        let safe_repo = repo_id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' {
                    ch
                } else {
                    '-'
                }
            })
            .collect::<String>();
        let path = std::env::temp_dir().join(format!(
            "jeryu-codegraph-{safe_repo}-{}-{}",
            std::process::id(),
            epoch_nanos()
        ));
        Self { path }
    }
}

impl Drop for TempCheckout {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn epoch_nanos() -> u128 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos(),
        Err(_) => 0,
    }
}

fn codegraph_error(
    status: StatusCode,
    code: &'static str,
    purpose: &'static str,
    reason: &str,
    common_fixes: &'static [&'static str],
    repair_hint: &'static str,
) -> AxumResponse {
    (
        status,
        Json(json!({
            "code": code,
            "message": reason,
            "purpose": purpose,
            "reason": reason,
            "common_fixes": common_fixes,
            "docs_url": "docs/errors.md#not-found",
            "repair_hint": repair_hint,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use serde_json::{Value, json};

    use std::sync::Arc;

    use super::{CodeGraphRouteError, TempCheckout, codegraph_error, is_rich_query, run_git};
    use axum::http::StatusCode;

    async fn response_json(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    #[test]
    fn rich_query_detection_is_additive() {
        assert!(!is_rich_query(&json!({ "changed_paths": ["src/lib.rs"] })));
        assert!(is_rich_query(&json!({ "ref": "main" })));
        assert!(is_rich_query(&json!({ "intent": "audit" })));
        assert!(is_rich_query(&json!({ "question": "what should I read?" })));
        assert!(is_rich_query(&json!({ "max_tokens": 4096 })));
    }

    #[test]
    fn git_wrapper_reports_success_status_failure_and_spawn_failure() {
        let version = run_git("git", &["--version"], None).expect("git version");
        assert!(version.starts_with("git version"));
        let status_error = run_git("git", &["rev-parse", "--verify", "missing-ref"], None)
            .expect_err("missing ref should fail");
        assert!(!status_error.is_empty());
        let spawn_error = run_git("jeryu-missing-git-binary", &["--version"], None)
            .expect_err("missing binary should fail");
        assert!(spawn_error.contains("git invocation failed"));
    }

    #[test]
    fn route_error_display_strings_are_operator_readable() {
        assert_eq!(
            CodeGraphRouteError::RepoNotFound {
                id: "repo-1".to_string()
            }
            .to_string(),
            "repository not found: repo-1"
        );
        assert!(
            CodeGraphRouteError::InvalidRef {
                ref_name: "main".to_string(),
                reason: "missing".to_string()
            }
            .to_string()
            .contains("main")
        );
        assert!(
            CodeGraphRouteError::MaterializeFailed("clone failed".to_string())
                .to_string()
                .contains("clone failed")
        );
        assert!(
            CodeGraphRouteError::IndexFailed("no Cargo.toml".to_string())
                .to_string()
                .contains("no Cargo.toml")
        );
    }

    #[tokio::test]
    async fn route_errors_render_typed_json_bodies() {
        let errors = [
            (
                CodeGraphRouteError::RepoNotFound {
                    id: "repo-1".to_string(),
                },
                "not_found",
            ),
            (
                CodeGraphRouteError::InvalidRef {
                    ref_name: "main".to_string(),
                    reason: "missing".to_string(),
                },
                "invalid_ref",
            ),
            (
                CodeGraphRouteError::MaterializeFailed("clone failed".to_string()),
                "codegraph_materialize_failed",
            ),
            (
                CodeGraphRouteError::IndexFailed("no Cargo.toml".to_string()),
                "codegraph_index_failed",
            ),
            (
                CodeGraphRouteError::NotIndexed {
                    retry_after_secs: 5,
                },
                "codegraph_not_indexed",
            ),
            (
                CodeGraphRouteError::RebuildInProgress {
                    retry_after_secs: 5,
                },
                "codegraph_rebuild_in_progress",
            ),
            (
                CodeGraphRouteError::RebuildCapacity {
                    retry_after_secs: 5,
                },
                "codegraph_rebuild_capacity",
            ),
        ];
        for (error, code) in errors {
            let body = response_json(error.into_response()).await;
            assert_eq!(body["code"], code);
            assert_eq!(body["docs_url"], "docs/errors.md#not-found");
        }

        let body = response_json(codegraph_error(
            StatusCode::FAILED_DEPENDENCY,
            "codegraph_query_failed",
            "query codegraph",
            "storage unavailable",
            &["rerun index"],
            "rerun focused proof",
        ))
        .await;
        assert_eq!(body["code"], "codegraph_query_failed");
        assert_eq!(body["common_fixes"], json!(["rerun index"]));
    }

    #[test]
    fn a_rebuild_lock_is_per_repository_and_claimed_without_waiting() {
        let one = super::rebuild_lock(std::path::Path::new("/jeryu/lock-scope-one.git"));
        let two = super::rebuild_lock(std::path::Path::new("/jeryu/lock-scope-two.git"));
        let held = super::try_claim(&one).expect("a free lock is claimed");
        // Another repository's rebuild is never behind this one.
        assert!(super::try_claim(&two).is_some());
        // The same repository is: the second caller is told so rather than
        // waiting out a clone and a workspace walk.
        assert!(super::try_claim(&one).is_none());
        drop(held);
        assert!(super::try_claim(&one).is_some());
        // Both callers of the same repository share one lock.
        assert!(Arc::ptr_eq(
            &one,
            &super::rebuild_lock(std::path::Path::new("/jeryu/lock-scope-one.git"))
        ));
    }

    #[test]
    fn a_panicking_rebuild_leaves_the_repository_claimable() {
        let lock = super::rebuild_lock(std::path::Path::new("/jeryu/lock-panic.git"));
        let outcome = std::panic::catch_unwind(|| {
            let claimed = super::rebuild_lock(std::path::Path::new("/jeryu/lock-panic.git"));
            let _held = super::try_claim(&claimed).expect("a free lock is claimed");
            panic!("indexing run panicked");
        });
        assert!(outcome.is_err());
        // The lock is poisoned, not forfeited: the next refresh still runs.
        assert!(lock.try_lock().is_err());
        assert!(super::try_claim(&lock).is_some());
    }

    #[test]
    fn unheld_repository_locks_are_dropped_once_the_map_fills() {
        // A map of its own: the process-wide one is shared with every test
        // running in parallel, so its size is not this test's to assert.
        let map = std::sync::Mutex::default();
        for index in 0..super::REBUILD_LOCKS_KEPT {
            super::rebuild_lock_in(
                &map,
                std::path::Path::new(&format!("/jeryu/pruned-{index}.git")),
            );
        }
        let held = super::rebuild_lock_in(&map, std::path::Path::new("/jeryu/pruned-held.git"));
        let _claim = super::try_claim(&held).expect("a free lock is claimed");
        super::rebuild_lock_in(&map, std::path::Path::new("/jeryu/pruned-trigger.git"));
        let locks = map.lock().expect("lock map");
        assert!(locks.len() < super::REBUILD_LOCKS_KEPT);
        // A lock a rebuild holds is never pruned out from under it.
        assert!(locks.contains_key(std::path::Path::new("/jeryu/pruned-held.git")));
    }

    #[test]
    fn rebuilds_in_flight_are_bounded_and_the_slot_returns_on_panic() {
        // Slots of its own: filling the process-wide ones would refuse a
        // codegraph read another test runs in parallel, and that test's
        // rebuilds would move the count this one asserts.
        let pool: &'static super::RebuildSlots = Box::leak(Box::new(super::RebuildSlots::new()));
        let slots: Vec<_> = (0..super::REBUILD_PARALLEL)
            .map(|_| pool.try_take().expect("a free slot"))
            .collect();
        assert!(pool.try_take().is_none());
        drop(slots);
        let outcome = std::panic::catch_unwind(|| {
            let _slot = pool.try_take().expect("a free slot");
            panic!("indexing run panicked");
        });
        assert!(outcome.is_err());
        assert_eq!(*pool.count(), 0);
    }

    #[test]
    fn a_ref_name_git_could_read_as_an_option_is_refused_before_git_runs() {
        assert!(super::ref_name_refusal("main").is_none());
        assert!(super::ref_name_refusal("release/v5.0.0").is_none());
        assert!(super::ref_name_refusal("").is_some());
        assert!(super::ref_name_refusal("--upload-pack=touch /tmp/x").is_some());
        assert!(super::ref_name_refusal("main head").is_some());
        assert!(super::ref_name_refusal("main\nhead").is_some());
        let refused = super::resolve_ref("git", std::path::Path::new("."), "--version")
            .expect_err("an option-shaped ref is refused");
        assert!(refused.to_string().contains("--version"), "{refused}");
    }

    #[test]
    fn temp_checkout_path_sanitizes_repo_identity() {
        let checkout = TempCheckout::new("alice/repo.name");
        let path = checkout.path.to_string_lossy();
        assert!(path.contains("alice-repo-name"));
    }
}
