//! System-wide tool-finder surface: live scan trigger with WebSocket progress,
//! the `/tools` pattern-family dashboard, and cluster -> registry proposal.
//!
//! The scan runs the jeryu-codegraph v2 pipeline over every split family the
//! server can reach inside `spawn_blocking`; its progress callback folds into the
//! single-flight [`ToolFinderScanState`] and publishes throttled `WebEvent`s
//! on scope [`SCAN_SCOPE`] through the [`super::WsHub`] push lane. Results
//! persist to the codegraph SQLite store under repo id [`SYSTEM_REPO_ID`],
//! which the dashboard (and MCP) read back as enriched pattern families.
//!
//! Two scan sources exist, tried in that order:
//!
//! - **working-tree**: the `--split-manifest` paths, whose sibling checkouts
//!   the scanner walks directly. This is the local-development source.
//! - **hosted**: the bare repositories this forge serves. [`hosted`] discovers
//!   split families from the repositories themselves and materializes each
//!   member's default branch into a bounded scratch tree. This is what the
//!   hosted forge has, where no checkout exists.
//!
//! When neither source yields a family, `GET /api/v1/tool-finder/source` says
//! so in as many words instead of the scan failing with a bare message.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use jeryu_codegraph::{
    ToolBuildCluster, ToolBuildScanOptions, ToolBuildScanProgress, discover_system_repo_roots,
    enrich_cluster, group_pattern_families, scan_tool_build_system,
};
use jeryu_readmodel::contracts::{
    ToolFinderCluster, ToolFinderDashboard, ToolFinderOccurrence, ToolFinderPatternFamily,
    ToolFinderProposeReceipt, ToolFinderScanMeta, ToolFinderScanStatus, WebEvent,
};
use serde::Deserialize;
use serde_json::json;

use super::workcells_support::{TypedError, typed_error};
use super::{WebState, server_time};

pub(crate) mod hosted;

/// Schema of `GET /api/v1/tool-finder/source`.
const SOURCE_SCHEMA: &str = "jeryu.tool_finder.source/v1";

/// WebSocket scope the scan streams on.
pub(super) const SCAN_SCOPE: &str = "tool_finder.scan";
/// Synthetic repo id the system-wide scan persists under.
pub(super) const SYSTEM_REPO_ID: &str = "system/host";
/// Minimum interval between pushed progress frames (phase changes bypass it).
const PUBLISH_INTERVAL_MS: u128 = 100;

const TOOL_FINDER_DOCS: &str = "docs/tool-finder.md";

/// Single-flight scan state, retained across scans so `/tools` can paint the
/// last result. All transitions happen under one std mutex; nothing async ever
/// holds it.
#[derive(Clone, Default)]
pub(crate) struct ToolFinderScanState {
    inner: Arc<Mutex<ScanInner>>,
}

#[derive(Default)]
struct ScanInner {
    running: bool,
    scan_id: u32,
    phase: String,
    current_repo: Option<String>,
    repos_total: u32,
    repos_done: u32,
    files_scanned: u32,
    files_skipped: u32,
    clusters_found: u32,
    families_found: u32,
    started_at: Option<String>,
    finished_at: Option<String>,
    error: Option<String>,
    last_publish_ms: u128,
}

impl ScanInner {
    fn status(&self) -> ToolFinderScanStatus {
        ToolFinderScanStatus {
            running: self.running,
            scan_id: self.scan_id,
            phase: if self.phase.is_empty() {
                "idle".to_string()
            } else {
                self.phase.clone()
            },
            current_repo: self.current_repo.clone(),
            repos_total: self.repos_total,
            repos_done: self.repos_done,
            files_scanned: self.files_scanned,
            files_skipped: self.files_skipped,
            clusters_found: self.clusters_found,
            families_found: self.families_found,
            started_at: self.started_at.clone(),
            finished_at: self.finished_at.clone(),
            error: self.error.clone(),
        }
    }
}

impl ToolFinderScanState {
    /// Atomically claim the single flight. `Err` carries the running snapshot
    /// for the 409 body.
    pub(super) fn try_begin(&self) -> Result<ToolFinderScanStatus, Box<ToolFinderScanStatus>> {
        let mut inner = self.inner.lock().expect("tool-finder scan mutex poisoned");
        if inner.running {
            return Err(Box::new(inner.status()));
        }
        inner.running = true;
        inner.scan_id = inner.scan_id.saturating_add(1);
        inner.phase = "discover".to_string();
        inner.current_repo = None;
        inner.repos_total = 0;
        inner.repos_done = 0;
        inner.files_scanned = 0;
        inner.files_skipped = 0;
        inner.clusters_found = 0;
        inner.families_found = 0;
        inner.started_at = Some(server_time());
        inner.finished_at = None;
        inner.error = None;
        inner.last_publish_ms = 0;
        Ok(inner.status())
    }

    /// Fold one engine progress event in. Returns a snapshot only when the
    /// event should be pushed (throttle window elapsed or the phase/repo
    /// changed) — the throttle lives inside the same lock as the state.
    fn apply_progress(&self, progress: &ToolBuildScanProgress) -> Option<ToolFinderScanStatus> {
        let mut inner = self.inner.lock().expect("tool-finder scan mutex poisoned");
        let phase = progress.phase.as_str().to_string();
        let repo = (!progress.current_repo.is_empty()).then(|| progress.current_repo.clone());
        let phase_changed = inner.phase != phase;
        let repo_changed = inner.current_repo != repo;
        inner.phase = phase;
        inner.current_repo = repo;
        inner.repos_total = progress.repo_total as u32;
        inner.repos_done = progress.repos_done as u32;
        inner.files_scanned = progress.files_scanned as u32;
        inner.files_skipped = progress.files_skipped as u32;
        inner.clusters_found = progress.clusters_so_far as u32;
        let now = epoch_ms();
        if phase_changed
            || repo_changed
            || now.saturating_sub(inner.last_publish_ms) >= PUBLISH_INTERVAL_MS
        {
            inner.last_publish_ms = now;
            return Some(inner.status());
        }
        None
    }

    fn complete(&self, clusters: u32, families: u32) -> ToolFinderScanStatus {
        let mut inner = self.inner.lock().expect("tool-finder scan mutex poisoned");
        inner.running = false;
        inner.phase = "completed".to_string();
        inner.current_repo = None;
        inner.clusters_found = clusters;
        inner.families_found = families;
        inner.finished_at = Some(server_time());
        inner.status()
    }

    fn fail(&self, error: String) -> ToolFinderScanStatus {
        let mut inner = self.inner.lock().expect("tool-finder scan mutex poisoned");
        inner.running = false;
        inner.phase = "failed".to_string();
        inner.current_repo = None;
        inner.error = Some(error);
        inner.finished_at = Some(server_time());
        inner.status()
    }

    pub(super) fn snapshot(&self) -> ToolFinderScanStatus {
        self.inner
            .lock()
            .expect("tool-finder scan mutex poisoned")
            .status()
    }
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

/// `GET /api/v1/tool-finder/scan` — current/last scan status (poll path).
pub(super) async fn scan_status(State(state): State<Arc<WebState>>) -> Json<ToolFinderScanStatus> {
    Json(state.tool_finder_scan.snapshot())
}

/// `POST /api/v1/tool-finder/scan` — start the system-wide scan. 202 with the
/// fresh status, or a 409 typed error when one is already in flight.
pub(super) async fn scan_start(State(state): State<Arc<WebState>>) -> AxumResponse {
    match start_system_scan(&state) {
        Ok(status) => (StatusCode::ACCEPTED, Json(status)).into_response(),
        Err(StartScanError::Busy(_status)) => tool_finder_typed_error(
            StatusCode::CONFLICT,
            "tool_finder_scan_running",
            "start the system-wide tool-finder scan",
            "a scan is already in flight",
            &[
                "subscribe to the tool_finder.scan websocket scope for live progress",
                "GET /api/v1/tool-finder/scan for the current status",
            ],
            "wait for the running scan to finish, then POST again",
        ),
        Err(StartScanError::NotConfigured) => tool_finder_typed_error(
            StatusCode::FAILED_DEPENDENCY,
            "tool_finder_not_configured",
            "start the system-wide tool-finder scan",
            NOT_CONFIGURED,
            NOT_CONFIGURED_FIXES,
            "configure a split family on this server, then POST again",
        ),
    }
}

/// Why a scan could not start.
pub(super) enum StartScanError {
    Busy(Box<ToolFinderScanStatus>),
    /// This server has neither split-manifest checkouts nor a hosted
    /// repository that names a split family.
    NotConfigured,
}

/// The one sentence every surface uses for "this server has nothing to scan".
pub(super) const NOT_CONFIGURED: &str = "no split family is configured on this server: no --split-manifest checkout, \
     and no hosted repository's default branch has a root repos.manifest.toml \
     or *-split.lock.toml";

const NOT_CONFIGURED_FIXES: &[&str] = &[
    "GET /api/v1/tool-finder/source to see what discovery found and what it skipped",
    "host the split family's repos.manifest.toml or *-split.lock.toml on its default branch",
    "for a local checkout host, start the server with --split-manifest",
];

/// Shared scan trigger (HTTP + MCP): claims the single flight, then resolves a
/// scan source, runs the engine scan + persistence on a blocking thread, and
/// streams throttled progress through the WS hub.
pub(super) fn start_system_scan(
    state: &Arc<WebState>,
) -> Result<ToolFinderScanStatus, StartScanError> {
    if !could_scan(state) {
        return Err(StartScanError::NotConfigured);
    }
    let status = state
        .tool_finder_scan
        .try_begin()
        .map_err(StartScanError::Busy)?;
    publish_scan_event(state, "tool_finder.scan.started", &status);

    let task_state = state.clone();
    tokio::spawn(async move {
        let blocking_state = task_state.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let source = resolve_scan_source(&blocking_state).map_err(|error| error.to_string())?;
            publish_source_event(&blocking_state, &source);
            let options = ToolBuildScanOptions::system_default();
            let progress_state = blocking_state.clone();
            let report = scan_tool_build_system(
                source.roots(),
                SYSTEM_REPO_ID,
                source.commit_label(),
                &options,
                &move |progress| {
                    if let Some(status) = progress_state.tool_finder_scan.apply_progress(&progress)
                    {
                        publish_scan_event(&progress_state, "tool_finder.scan.progress", &status);
                    }
                },
            )
            .map_err(|error| format!("system scan: {error}"))?;
            blocking_state
                .codegraph_store
                .persist_tool_build_report(&report)
                .map_err(|error| format!("persist scan report: {error}"))?;
            blocking_state
                .codegraph_store
                .propagate_ignores_to_merged(&report.clusters)
                .map_err(|error| format!("propagate ignores: {error}"))?;
            Ok::<(u32, u32), String>((report.clusters.len() as u32, report.families.len() as u32))
        })
        .await;

        match outcome {
            Ok(Ok((clusters, families))) => {
                let status = task_state.tool_finder_scan.complete(clusters, families);
                publish_scan_event(&task_state, "tool_finder.scan.completed", &status);
            }
            Ok(Err(error)) => {
                let status = task_state.tool_finder_scan.fail(error);
                publish_scan_event(&task_state, "tool_finder.scan.failed", &status);
            }
            Err(join_error) => {
                let status = task_state
                    .tool_finder_scan
                    .fail(format!("scan task panicked: {join_error}"));
                publish_scan_event(&task_state, "tool_finder.scan.failed", &status);
            }
        }
    });

    Ok(status)
}

/// Could this server have anything to scan at all? A cheap in-memory check, so
/// the 424 is decided on the handler thread; the real (git-reading) discovery
/// happens on the blocking thread and reports its own emptiness.
fn could_scan(state: &WebState) -> bool {
    !state.split_manifests.is_empty()
        || state
            .core
            .list_repositories(None)
            .iter()
            .any(|repo| !repo.archived && !repo.disabled)
}

/// The sources one scan will read. The hosted variant owns its scratch tree:
/// holding the value alive holds the materialized sources on disk.
pub(super) enum ScanSource {
    /// `--split-manifest` sibling checkouts, walked in place.
    WorkingTree(Vec<(String, PathBuf)>),
    /// Default branches materialized out of the hosted bare repositories.
    Hosted(hosted::ScanWorkspace),
}

impl ScanSource {
    pub(super) fn roots(&self) -> &[(String, PathBuf)] {
        match self {
            Self::WorkingTree(roots) => roots,
            Self::Hosted(workspace) => &workspace.roots,
        }
    }

    /// The `commit_sha` label the persisted report carries.
    fn commit_label(&self) -> &'static str {
        match self {
            Self::WorkingTree(_) => "working-tree",
            Self::Hosted(_) => "hosted-default-branch",
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::WorkingTree(_) => "working-tree",
            Self::Hosted(_) => "hosted",
        }
    }

    fn skipped(&self) -> &[hosted::SkippedRepo] {
        match self {
            Self::WorkingTree(_) => &[],
            Self::Hosted(workspace) => &workspace.skipped,
        }
    }
}

/// Why no scan source could be resolved.
pub(super) enum ScanSourceError {
    NotConfigured,
    Hosted(hosted::HostedSourceError),
}

impl std::fmt::Display for ScanSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => formatter.write_str(NOT_CONFIGURED),
            Self::Hosted(error) => write!(formatter, "{error}"),
        }
    }
}

/// Resolve the sources for one scan: the configured checkouts when they exist
/// (local development), else the hosted bare repositories (the forge). Runs
/// git; call it off the async workers.
pub(super) fn resolve_scan_source(state: &WebState) -> Result<ScanSource, ScanSourceError> {
    let parents = manifest_parents(&state.split_manifests);
    if !parents.is_empty() {
        let roots = discover_system_repo_roots(&parents).unwrap_or_default();
        if !roots.is_empty() {
            return Ok(ScanSource::WorkingTree(roots));
        }
    }
    match hosted::materialize(state, hosted::discover(state)) {
        Ok(workspace) => Ok(ScanSource::Hosted(workspace)),
        Err(hosted::HostedSourceError::NotConfigured) => Err(ScanSourceError::NotConfigured),
        Err(error) => Err(ScanSourceError::Hosted(error)),
    }
}

/// Announce what a scan is about to read and what discovery left out, so the
/// bounds are visible rather than silent.
fn publish_source_event(state: &Arc<WebState>, source: &ScanSource) {
    let skipped = source.skipped();
    let summary = format!(
        "tool-finder scan sources: {} repos from {} ({} skipped)",
        source.roots().len(),
        source.kind(),
        skipped.len()
    );
    let payload = json!({
        "kind": source.kind(),
        "repos": source.roots().iter().map(|(repo, _)| repo).collect::<Vec<_>>(),
        "skipped": skipped,
    });
    state.ws.publish(SCAN_SCOPE, move |seq| WebEvent {
        seq,
        timestamp: server_time(),
        scope: SCAN_SCOPE.to_string(),
        kind: "tool_finder.scan.source".to_string(),
        entity: SYSTEM_REPO_ID.to_string(),
        summary,
        payload,
    });
}

/// `GET /api/v1/tool-finder/source` — what a scan would read right now, and
/// what it would leave out. Admin-only by path, like every other tool-finder
/// route; hosted discovery reads private repositories too.
pub(super) async fn source(State(state): State<Arc<WebState>>) -> AxumResponse {
    let worker_state = state.clone();
    match tokio::task::spawn_blocking(move || source_payload(&worker_state)).await {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => {
            let reason = error.to_string();
            tool_finder_typed_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "tool_finder_source_failed",
                "report the tool-finder scan source",
                &reason,
                &["retry in a few seconds"],
                "retry; if it persists check the server log for a discovery panic",
            )
        }
    }
}

/// What `GET /api/v1/tool-finder/source` answers.
#[derive(Debug, serde::Serialize)]
pub(super) struct ToolFinderSource {
    pub schema_version: &'static str,
    pub generated_at: String,
    /// `working-tree`, `hosted` or `none`.
    pub kind: &'static str,
    /// False when this server has no split family to scan at all.
    pub configured: bool,
    /// One sentence naming the state, for the Findings page to paint.
    pub detail: String,
    /// The `--split-manifest` paths, if any.
    pub split_manifests: Vec<String>,
    pub repos: Vec<hosted::ScanSourceRepo>,
    pub skipped: Vec<hosted::SkippedRepo>,
}

/// Shared source report (HTTP + MCP). Discovery only: nothing is materialized,
/// so this stays cheap enough to poll from the page.
pub(super) fn source_payload(state: &WebState) -> ToolFinderSource {
    let split_manifests: Vec<String> = state
        .split_manifests
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    let mut report = ToolFinderSource {
        schema_version: SOURCE_SCHEMA,
        generated_at: server_time(),
        kind: "none",
        configured: false,
        detail: NOT_CONFIGURED.to_string(),
        split_manifests,
        repos: Vec::new(),
        skipped: Vec::new(),
    };
    let parents = manifest_parents(&state.split_manifests);
    if !parents.is_empty() {
        let roots = discover_system_repo_roots(&parents).unwrap_or_default();
        if !roots.is_empty() {
            report.kind = "working-tree";
            report.configured = true;
            report.detail = format!(
                "{} split-family checkouts beside the configured --split-manifest paths",
                roots.len()
            );
            report.repos = roots
                .into_iter()
                .map(|(repo, _)| hosted::ScanSourceRepo {
                    repo,
                    family: None,
                    branch: None,
                    commit: None,
                    private: false,
                })
                .collect();
            return report;
        }
    }
    let found = hosted::discover(state);
    report.skipped = found.skipped;
    if found.repos.is_empty() {
        return report;
    }
    report.kind = "hosted";
    report.configured = true;
    report.detail = format!(
        "{} hosted repositories, read from their default branches in the bare repos",
        found.repos.len()
    );
    report.repos = found.repos;
    report
}

/// The directories whose `*-split` children carry family manifests, derived
/// from the configured manifests (each manifest's grandparent — e.g.
/// `/home/ubuntu` for `/home/ubuntu/jeryu-split/repos.manifest.toml`). This is
/// what turns the two systemd-configured manifests into all sibling families.
fn manifest_parents(manifests: &[PathBuf]) -> Vec<PathBuf> {
    let mut parents: BTreeSet<PathBuf> = BTreeSet::new();
    for manifest in manifests {
        if let Some(parent) = manifest.parent().and_then(|split_root| split_root.parent()) {
            parents.insert(parent.to_path_buf());
        }
    }
    parents.into_iter().collect()
}

/// Push one scan event to every `tool_finder.scan` subscriber.
fn publish_scan_event(state: &Arc<WebState>, kind: &str, status: &ToolFinderScanStatus) {
    let kind = kind.to_string();
    let summary = scan_summary(status);
    let payload = match serde_json::to_value(status) {
        Ok(value) => value,
        Err(error) => json!({ "serialize_error": error.to_string() }),
    };
    state.ws.publish(SCAN_SCOPE, move |seq| WebEvent {
        seq,
        timestamp: server_time(),
        scope: SCAN_SCOPE.to_string(),
        kind,
        entity: SYSTEM_REPO_ID.to_string(),
        summary,
        payload,
    });
}

fn scan_summary(status: &ToolFinderScanStatus) -> String {
    match status.phase.as_str() {
        "completed" => format!(
            "tool-finder scan complete: {} clusters in {} families across {} repos ({} files)",
            status.clusters_found, status.families_found, status.repos_total, status.files_scanned
        ),
        "failed" => format!(
            "tool-finder scan failed: {}",
            status.error.as_deref().unwrap_or("unknown error")
        ),
        _ => format!(
            "tool-finder scan: {} ({}/{} repos, {} files, {} clusters)",
            status.current_repo.as_deref().unwrap_or(&status.phase),
            status.repos_done,
            status.repos_total,
            status.files_scanned,
            status.clusters_found,
        ),
    }
}

/// Snapshot frame for a fresh `tool_finder.scan` subscription.
pub(super) fn snapshot_event(state: &WebState, seq: u64, timestamp: String) -> WebEvent {
    let status = state.tool_finder_scan.snapshot();
    let summary = scan_summary(&status);
    WebEvent {
        seq,
        timestamp,
        scope: SCAN_SCOPE.to_string(),
        kind: "tool_finder.scan.snapshot".to_string(),
        entity: SYSTEM_REPO_ID.to_string(),
        summary,
        payload: match serde_json::to_value(&status) {
            Ok(value) => value,
            Err(error) => json!({ "serialize_error": error.to_string() }),
        },
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct DashboardQuery {
    pub(super) limit: Option<usize>,
    #[serde(default)]
    pub(super) include_ignored: bool,
}

/// `GET /api/v1/tool-finder/dashboard` — pattern families over the persisted
/// system scan.
pub(super) async fn dashboard(
    State(state): State<Arc<WebState>>,
    Query(query): Query<DashboardQuery>,
) -> AxumResponse {
    let limit = query.limit.unwrap_or(200).clamp(1, 500);
    match dashboard_payload(&state, limit, query.include_ignored) {
        Ok(dashboard) => Json(dashboard).into_response(),
        Err(error) => tool_finder_typed_error(
            StatusCode::FAILED_DEPENDENCY,
            "tool_finder_store_unavailable",
            "build the tool-finder dashboard",
            &error,
            &[
                "POST /api/v1/tool-finder/scan to run the first system scan",
                "verify the codegraph SQLite store path is readable",
            ],
            "run a scan, then reload the dashboard",
        ),
    }
}

/// Shared dashboard builder (HTTP + MCP).
pub(super) fn dashboard_payload(
    state: &WebState,
    limit: usize,
    include_ignored: bool,
) -> Result<ToolFinderDashboard, String> {
    let clusters = state
        .codegraph_store
        .tool_build_clusters(Some(SYSTEM_REPO_ID), limit, include_ignored)
        .map_err(|error| error.to_string())?;
    let families = group_pattern_families(&clusters);

    let cluster_dto = |cluster: &ToolBuildCluster| -> ToolFinderCluster {
        let enrichment = enrich_cluster(cluster);
        ToolFinderCluster {
            cluster_id: cluster.cluster_id.clone(),
            category: cluster.category.as_str().to_string(),
            score: cluster.score,
            occurrence_count: cluster.occurrence_count as u32,
            repo_count: cluster.repo_count as u32,
            file_count: cluster.file_count as u32,
            total_lines: cluster.total_lines as u32,
            language: cluster.language.clone(),
            insight: cluster.insight.clone(),
            normalized_preview: cluster.normalized_preview.clone(),
            anticipated_loc_saved: enrichment.anticipated_loc_saved as u32,
            suggested_name: enrichment.suggested_name,
            suggested_kind: enrichment.suggested_kind.to_string(),
            ignored: cluster.ignored.is_some(),
            occurrences: cluster
                .occurrences
                .iter()
                .map(|occ| ToolFinderOccurrence {
                    repo_id: occ.repo_id.clone(),
                    path: occ.path.clone(),
                    start_line: occ.start_line as u32,
                    end_line: occ.end_line as u32,
                    is_test: occ.is_test,
                })
                .collect(),
        }
    };

    let mut family_dtos: Vec<ToolFinderPatternFamily> = Vec::with_capacity(families.len());
    let mut candidate_loc_saved = 0u32;
    for family in &families {
        let mut members: Vec<ToolFinderCluster> = clusters
            .iter()
            .filter(|cluster| family.cluster_ids.contains(&cluster.cluster_id))
            .map(cluster_dto)
            .collect();
        members.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.cluster_id.cmp(&b.cluster_id))
        });
        if family.category == jeryu_codegraph::ToolBuildCategory::ToolCandidate {
            candidate_loc_saved =
                candidate_loc_saved.saturating_add(family.anticipated_loc_saved_total as u32);
        }
        family_dtos.push(ToolFinderPatternFamily {
            family_id: family.family_id.clone(),
            label: family.label.clone(),
            category: family.category.as_str().to_string(),
            language: family.language.clone(),
            anticipated_loc_saved: family.anticipated_loc_saved_total as u32,
            occurrence_count: family.occurrence_total as u32,
            file_count: family.file_total as u32,
            repos: family.repo_ids.clone(),
            clusters: members,
        });
    }

    // Persisted-scan provenance: every cluster row carries the report's
    // scanned_at; the in-memory state adds repo/file counters when this
    // process ran the scan.
    let scanned_at = scan_created_at(state);
    let status = state.tool_finder_scan.snapshot();
    let scan = ToolFinderScanMeta {
        scanned_at,
        repos_scanned: status.repos_total,
        files_scanned: status.files_scanned,
    };

    Ok(ToolFinderDashboard {
        generated_at: server_time(),
        scan,
        family_count: family_dtos.len() as u32,
        cluster_count: clusters.len() as u32,
        candidate_loc_saved,
        families: family_dtos,
    })
}

/// The persisted scan's `created_at` (unix millis), read from the cluster rows.
pub(super) fn scan_created_at(state: &WebState) -> Option<String> {
    state
        .codegraph_store
        .tool_build_scanned_at(SYSTEM_REPO_ID)
        .ok()
        .flatten()
}

#[derive(Debug, Default, Deserialize)]
struct ProposeRequest {
    tool_id: Option<String>,
    name: Option<String>,
    kind: Option<String>,
}

/// `POST /api/v1/tool-finder/propose/:cluster_id` — promote a cluster into a
/// `jeryu-tool` registry proposal + build task. Idempotent on
/// `origin_cluster`: re-proposing an already-proposed cluster is a no-op
/// receipt, mirroring the historical `propose.py`.
pub(super) async fn propose(
    State(state): State<Arc<WebState>>,
    AxumPath(cluster_id): AxumPath<String>,
    body: Bytes,
) -> AxumResponse {
    let request: ProposeRequest = if body.is_empty() {
        ProposeRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(error) => {
                return tool_finder_typed_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "tool_finder_invalid_request",
                    "propose a tool from a cluster",
                    &error.to_string(),
                    &["send JSON with optional tool_id/name/kind overrides, or an empty body"],
                    "fix the request body, then retry the proposal",
                );
            }
        }
    };
    match propose_cluster(&state, &cluster_id, request) {
        Ok(receipt) => Json(receipt).into_response(),
        Err(ProposeError::RegistryUnavailable(reason)) => tool_finder_typed_error(
            StatusCode::FAILED_DEPENDENCY,
            "tool_finder_registry_unavailable",
            "propose a tool from a cluster",
            &reason,
            &["start the server with --split-manifest so jeryu-tool's registry resolves"],
            "wire the registry path, then retry the proposal",
        ),
        Err(ProposeError::ClusterNotFound) => tool_finder_typed_error(
            StatusCode::NOT_FOUND,
            "tool_finder_cluster_not_found",
            "propose a tool from a cluster",
            "no persisted system-scan cluster has that id",
            &[
                "POST /api/v1/tool-finder/scan to refresh the system scan",
                "GET /api/v1/tool-finder/dashboard for current cluster ids",
            ],
            "re-run the scan, then propose a current cluster id",
        ),
        Err(ProposeError::ToolIdTaken(tool_id)) => tool_finder_typed_error(
            StatusCode::CONFLICT,
            "tool_finder_tool_id_taken",
            "propose a tool from a cluster",
            &format!("registry already has a tool id {tool_id:?} from a different cluster"),
            &["pass a distinct tool_id in the request body"],
            "retry with an explicit tool_id",
        ),
        Err(ProposeError::Io(reason)) => tool_finder_typed_error(
            StatusCode::FAILED_DEPENDENCY,
            "tool_finder_registry_write_failed",
            "propose a tool from a cluster",
            &reason,
            &["verify the jeryu-tool checkout is writable"],
            "fix the registry write path, then retry the proposal",
        ),
    }
}

enum ProposeError {
    RegistryUnavailable(String),
    ClusterNotFound,
    ToolIdTaken(String),
    Io(String),
}

/// Minimal registry probe: just enough to detect idempotency + id collisions.
#[derive(Debug, Default, Deserialize)]
struct RegistryProbe {
    #[serde(default)]
    tool: Vec<RegistryProbeTool>,
}

#[derive(Debug, Deserialize)]
struct RegistryProbeTool {
    id: String,
    #[serde(default)]
    origin_cluster: Option<String>,
}

fn propose_cluster(
    state: &WebState,
    cluster_id: &str,
    request: ProposeRequest,
) -> Result<ToolFinderProposeReceipt, ProposeError> {
    let Some(registry_path) = state.tool_registry_path.as_ref() else {
        return Err(ProposeError::RegistryUnavailable(
            "no tools-registry.toml wired (missing --split-manifest)".to_string(),
        ));
    };
    let registry_text = std::fs::read_to_string(registry_path)
        .map_err(|error| ProposeError::RegistryUnavailable(error.to_string()))?;
    let registry: RegistryProbe = toml::from_str(&registry_text)
        .map_err(|error| ProposeError::RegistryUnavailable(error.to_string()))?;

    // Idempotency: an existing proposal from this cluster is a no-op.
    if let Some(existing) = registry
        .tool
        .iter()
        .find(|tool| tool.origin_cluster.as_deref() == Some(cluster_id))
    {
        return Ok(ToolFinderProposeReceipt {
            created: false,
            tool_id: existing.id.clone(),
            task_id: None,
            message: format!("already proposed as tool {:?}", existing.id),
        });
    }

    let cluster = state
        .codegraph_store
        .tool_build_clusters(Some(SYSTEM_REPO_ID), 500, true)
        .map_err(|error| ProposeError::Io(error.to_string()))?
        .into_iter()
        .find(|cluster| cluster.cluster_id == cluster_id)
        .ok_or(ProposeError::ClusterNotFound)?;
    let enrichment = enrich_cluster(&cluster);

    let tool_id = slugify(
        request
            .tool_id
            .as_deref()
            .unwrap_or(&enrichment.suggested_name),
    );
    if registry.tool.iter().any(|tool| tool.id == tool_id) {
        return Err(ProposeError::ToolIdTaken(tool_id));
    }

    let name = request
        .name
        .unwrap_or_else(|| enrichment.suggested_name.clone());
    let kind = request
        .kind
        .unwrap_or_else(|| enrichment.suggested_kind.to_string());
    let candidate_repos: BTreeSet<String> = cluster
        .occurrences
        .iter()
        .map(|occ| occ.repo_id.clone())
        .collect();
    let estimate = enrichment.anticipated_loc_saved;
    let tasks_dir = registry_path
        .parent()
        .map(|dir| dir.join("tasks"))
        .ok_or_else(|| ProposeError::Io("registry path has no parent".to_string()))?;
    let task_index = next_task_index(&tasks_dir);
    let task_id = format!("{task_index:04}");
    let description = cluster.insight.replace('"', "'");
    let repos_toml = toml_list(&candidate_repos);

    let tool_block = format!(
        "\n# Proposed by jeryu-tool-finder from cluster {cluster_id}.\n\
         [[tool]]\n\
         id = \"{tool_id}\"\n\
         name = \"{name}\"\n\
         kind = \"{kind}\"\n\
         status = \"proposed\"\n\
         source = \"\"\n\
         description = \"{description}\"\n\
         origin_cluster = \"{cluster_id}\"\n\
         adopting_repos = []\n\
         candidate_repos = {repos_toml}\n\
         loc_saved = 0\n\
         loc_saved_estimate = {estimate}\n"
    );
    let task_text = format!(
        "# tasks/{task_id}-{tool_id}.toml — filed by jeryu-tool-finder.\n\n\
         id = \"{task_id}\"\n\
         tool_id = \"{tool_id}\"\n\
         title = \"Extract {name} into a shared {kind}\"\n\
         status = \"open\"\n\
         origin_cluster = \"{cluster_id}\"\n\
         anticipated_loc_saved = {estimate}\n\
         target_repos = {repos_toml}\n\
         rollout = [\n\
         \x20 \"Build the tool in its canonical home and tag it.\",\n\
         \x20 \"Replace each target repo's local copy with the shared tool.\",\n\
         \x20 \"Move migrated repos from candidate_repos to adopting_repos and grow loc_saved.\",\n\
         \x20 \"Confirm each repo's gate lanes stay green after the swap.\",\n\
         ]\n"
    );

    let mut appended = registry_text;
    appended.push_str(&tool_block);
    std::fs::write(registry_path, appended).map_err(|error| ProposeError::Io(error.to_string()))?;
    std::fs::create_dir_all(&tasks_dir).map_err(|error| ProposeError::Io(error.to_string()))?;
    std::fs::write(
        tasks_dir.join(format!("{task_id}-{tool_id}.toml")),
        task_text,
    )
    .map_err(|error| ProposeError::Io(error.to_string()))?;

    Ok(ToolFinderProposeReceipt {
        created: true,
        tool_id,
        task_id: Some(task_id),
        message: format!("proposed (+{estimate} LOC anticipated) from cluster {cluster_id}"),
    })
}

fn slugify(value: &str) -> String {
    let mut slug = String::with_capacity(value.len());
    let mut last_dash = true;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "tool".to_string()
    } else {
        slug
    }
}

fn toml_list(items: &BTreeSet<String>) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    let inner = items
        .iter()
        .map(|item| format!("  \"{item}\""))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("[\n{inner},\n]")
}

fn next_task_index(tasks_dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(tasks_dir) else {
        return 1;
    };
    let mut highest = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".toml") {
            continue;
        }
        let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(index) = digits.parse::<usize>() {
            highest = highest.max(index);
        }
    }
    highest + 1
}

fn tool_finder_typed_error(
    status: StatusCode,
    code: &'static str,
    purpose: &'static str,
    reason: &str,
    common_fixes: &'static [&'static str],
    repair_hint: &'static str,
) -> AxumResponse {
    typed_error(TypedError {
        status,
        code,
        purpose,
        reason,
        common_fixes,
        docs_url: TOOL_FINDER_DOCS,
        repair_hint,
        message: reason,
    })
}
