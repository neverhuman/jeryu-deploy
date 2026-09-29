//! Axum HTTP/WebSocket edge for the local live Jeryu API.

mod agent_runs;
pub(crate) mod auth;
mod ci_evidence;
mod codegraph;
mod conditional;
mod control_plane;
mod ecosystem;
mod embedded_web;
mod error_codes;
mod error_envelope;
mod idempotency;
mod jankurai;
mod markdown;
mod merge_attempts;
mod merge_queue;
pub(crate) mod mirror_reconcile;
mod operator_resources;
mod paging;
pub(crate) use merge_queue::{is_queue_owned_ref, rebase_onto};
mod mcp_backend;
mod permissions;
mod pipeline;
mod pulls;
mod release_board;
mod repo_address;
mod repo_admin;
mod repo_automation;
mod repositories;
mod repository_create;
mod request_id;
mod request_rules;
mod route_index;
mod search;
mod sessions;
pub(crate) mod shift;
mod surface;
mod tool_build;
mod tool_finder;
mod tool_finder_schedule;
mod tool_proposals;
mod tool_registry;
mod tool_status_messages;
mod work;
mod workcells;
mod workcells_support;
mod ws;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, Extension, Path as AxumPath, Query, Request, State};
use axum::http::{HeaderName, HeaderValue, Method as HttpMethod, StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response as AxumResponse};
use axum::routing::{MethodRouter, any, get, post};
use axum::{Json, Router as AxumRouter};
use jeryu_codegraph::CodeGraphStore;
use jeryu_core::{AccountSummary, ForgeCore, UserRole};
use jeryu_jira::WorkStore;
use jeryu_readmodel::TuiReadModel;
use jeryu_readmodel::contracts::{RepositoryRole, ServerWsMessage, WebEvent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;

use crate::GithubRouter;
use crate::git_materializer::{CoreRedirects, GitMaterializer};
use crate::github::{
    GH_AUTH_BOUNDARY, GH_SETUP_COMMAND, GH_SETUP_REPAIR_COMMAND, GH_SETUP_TOKEN_FILE,
    MCP_GUIDANCE_TOOLS, MCP_RUN_TESTS_TOOL,
};
use jeryu_gitd::{GitdConfig, RepoManager};
use jeryu_runner_oci::{CliContainerRuntime, ContainerLifecycle};
use jeryu_runnerd::{WarmPool, WorkcellManager};
use repositories::{
    deployed_repositories, fleet_tool_adoption, repo_blob, repo_commits, repo_compare, repo_detail,
    repo_jankurai_scores_ingest, repo_jankurai_scores_list, repo_raw, repo_readme,
    repo_readme_update, repo_refs, repo_release_tag, repo_tree, repo_update, repos,
};
use surface::{bootstrap_payload_for_user, github_forward, graphql, markdown_render, repo_entry};

const WS_PROTOCOL: &str = "jeryu.ws.v1";
const MCP_READ_TOOL: &str = "jeryu.get_system_snapshot";
const MCP_CHECKS_TOOL: &str = "jeryu.get_ci_run_jobs";
const MCP_BLOCKERS_TOOL: &str = "jeryu.explain_blockers";
const MCP_PATCH_TOOL: &str = "jeryu.propose_patch";
const MCP_MERGE_TOOL: &str = "jeryu.request_merge";
const MCP_ISSUE_TOOL: &str = "jeryu.bug_submit";
/// The agent-run tool whose presence tells the manifest that `jeryu agent auth`
/// has a live surface to prepare credentials for.
const MCP_AGENT_WORK_TOOL: &str = "jeryu.agent_work.start";
/// Steady-state depth of pre-warmed agent containers the pool refills back to, so
/// a New Session claims a ready cell instead of paying a cold-start.
const WARM_POOL_TARGET: usize = 2;
const BOOTSTRAP_ADMIN_LOGIN: &str = "jeryu-admin";
const BOOTSTRAP_ADMIN_PASSWORD_ENV: &str = "JERYU_BOOTSTRAP_ADMIN_PASSWORD";

#[derive(Clone, Debug)]
pub struct WebServerConfig {
    pub bind: SocketAddr,
    pub spa_dir: PathBuf,
    pub data_dir: PathBuf,
    /// Storage root for bare git repositories served over smart-HTTP.
    pub git_storage_root: PathBuf,
    /// Optional split-family manifests used to classify portal/member repos.
    pub split_manifests: Vec<PathBuf>,
    /// Enforce account/session auth on `/api/v1/*`.
    pub auth_required: bool,
    /// Explicit single-host development bypass for local demos/tests.
    pub trust_local_dev: bool,
    /// Use Secure `__Host-` cookies. Disable only for plain-HTTP local dev.
    pub secure_cookies: bool,
}

mod catalog;

use catalog::{SplitCatalog, resolve_tool_registry_path};

#[derive(Clone)]
pub(crate) struct WebState {
    github: GithubRouter,
    tui: TuiReadModel,
    pub(crate) spa_dir: PathBuf,
    /// Live-stream fan-out hub: hands out monotonic sequence numbers and keeps
    /// a subscriber registry so the WS edge can push snapshots/deltas per scope.
    ws: WsHub,
    /// In-memory workcell controller for claim/repair/export/release flows.
    pub(crate) workcells: Arc<Mutex<WorkcellManager>>,
    /// Live high-level agent-run registry and control channels.
    pub(crate) agent_runs: agent_runs::AgentRunStore,
    /// Live PR gate runners, fed by `POST /api/v1/runners/heartbeat`.
    pub(crate) gate_runners: control_plane::GateRunnerStore,
    /// Family release boards, fed by `PUT /api/v1/release-board/:family`.
    pub(crate) release_boards: release_board::ReleaseBoardStore,
    /// Merge queue index; the queue itself lives in `refs/queue*` of each repo.
    pub(crate) merge_queue: Arc<merge_queue::MergeQueue>,
    /// The last merge attempt per PR and its forge answer (`merge_attempts`).
    pub(crate) merge_attempts: merge_attempts::MergeAttemptStore,
    /// Kept answers to `POST` writes that carried an `Idempotency-Key`.
    pub(crate) idempotency: idempotency::IdempotencyStore,
    /// todoq shift heartbeats (`<data_dir>/shift.sqlite`) and PR author.
    pub(crate) shift: shift::ShiftState,
    /// Pipeline event log (`<data_dir>/shift.sqlite`, table `pipeline_events`).
    pub(crate) events: pipeline::EventStore,
    /// Quality-gate disputes (`<data_dir>/shift.sqlite`, `jankurai_disputes`).
    pub(crate) disputes: jankurai::DisputeStore,
    /// What the newest GitHub-mirror reconcile found per repository.
    pub(crate) mirror_state: mirror_reconcile::MirrorStateStore,
    /// The attention inbox's last answer (`GET /api/v1/attention`).
    pub(crate) attention: pipeline::attention::AttentionCache,
    /// What every deploy repo pins (`GET /api/v1/pins`), cached for a minute.
    pub(crate) pins: pipeline::pins::PinsCache,
    /// The repo graph's `depends_on` edges, read from every repository's Cargo
    /// manifests and cached for a minute so the graph never walks them twice.
    pub(crate) repo_depends: control_plane::DependsCache,
    /// Auxiliary codegraph SQLite store for read-only oracle queries.
    pub(crate) codegraph_store: CodeGraphStore,
    /// Shared git-daemon repository manager backing the smart-HTTP transport.
    pub(crate) repo_manager: Arc<RepoManager>,
    /// Forge handle for the push->CI bridge (shares state with `github`).
    pub(crate) core: ForgeCore,
    /// Local-first Work Tracker store shared by Work routes and the issue bridge.
    pub(crate) work: WorkStore,
    /// Pool of pre-warmed agent containers a New Session claims from, so the
    /// launch reuses a ready cell with no cold-start. It needs `&mut self` to
    /// claim and refill, so it lives behind the same `Mutex` style the rest of
    /// `WebState` uses. Production wires the real CLI lifecycle (plan-only unless
    /// `JERYU_RUN_OCI=1`); tests inject a recording fake lifecycle so the claim
    /// path is exercised without Docker/Podman.
    pub(crate) warm_pool: Arc<Mutex<WarmPool>>,
    /// Which PTY backend a New Session agent runs under (native kernel sandbox vs.
    /// docker-backed live container) and the docker seam. Resolved once from
    /// `JERYU_AGENT_RUNTIME` / `JERYU_DOCKER_BIN`; a test injects it directly so it
    /// never mutates process-global env.
    pub(crate) session_runtime: sessions::SessionRuntimeConfig,
    split_catalog: SplitCatalog,
    /// Path to `jeryu-tool/tools-registry.toml`, resolved from the split
    /// manifest in `serve()`. `None` in tests and when no manifest is wired, in
    /// which case the golden-box endpoint reports an empty registry.
    tool_registry_path: Option<PathBuf>,
    /// Split manifests handed to `serve()`; the tool-finder system scan
    /// derives its family-discovery parents from these. Empty in tests.
    split_manifests: Vec<PathBuf>,
    /// Single-flight state for the system-wide tool-finder scan, retained
    /// across scans so the page can paint the last result.
    pub(crate) tool_finder_scan: tool_finder::ToolFinderScanState,
    pub(crate) auth_required: bool,
    pub(crate) trust_local_dev: bool,
    pub(crate) secure_cookies: bool,
    pub(crate) auth_rate_limits: Arc<Mutex<BTreeMap<String, auth::RateLimitBucket>>>,
    /// Unit-test sessions must never seed agent credentials from the operator home.
    #[cfg(test)]
    session_auth_home: Arc<tempfile::TempDir>,
}

impl WebState {
    fn with_repo_manager(
        core: ForgeCore,
        repo_manager: Arc<RepoManager>,
        spa_dir: PathBuf,
        data_dir: PathBuf,
        split_catalog: SplitCatalog,
    ) -> Self {
        // Seed the read model from ForgeCore state. No runner has reported a
        // heartbeat yet, so capacity starts at zero; `workcells::live_tui`
        // re-derives pools/health from the live fabric on every request.
        let tui = crate::read_model::assemble_read_model(
            &control_plane::active_repo_jobs(&core),
            &crate::read_model::FleetCapacity::default(),
        );
        // ForgeCore is Arc-backed, so this handle shares state with `github`.
        let core_handle = core.clone();
        let codegraph_path = {
            #[cfg(test)]
            {
                // The durable data_dir is only consulted outside tests. The
                // path carries a process-wide counter on top of the timestamp:
                // parallel tests constructing WebStates in the same millisecond
                // must NOT share one sqlite file (locked-database flakes).
                let _ = &data_dir;
                static TEST_DB_SEQ: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                std::env::temp_dir().join(format!(
                    "jeryu-web-codegraph-{}-{}.sqlite",
                    jeryu_runner_core::receipt::now_ms(),
                    TEST_DB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ))
            }
            #[cfg(not(test))]
            {
                data_dir.join("codegraph.sqlite")
            }
        };
        let codegraph_store = CodeGraphStore::open(codegraph_path).expect("open codegraph store");
        let work_path = {
            #[cfg(test)]
            {
                let _ = &data_dir;
                static TEST_WORK_DB_SEQ: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                std::env::temp_dir().join(format!(
                    "jeryu-web-work-{}-{}.sqlite",
                    jeryu_runner_core::receipt::now_ms(),
                    TEST_WORK_DB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ))
            }
            #[cfg(not(test))]
            {
                data_dir.join("work.sqlite")
            }
        };
        let work = WorkStore::open(work_path).expect("open work store");
        let shift_path = {
            #[cfg(test)]
            {
                static TEST_SHIFT_DB_SEQ: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                std::env::temp_dir().join(format!(
                    "jeryu-web-shift-{}-{}-{}.sqlite",
                    std::process::id(),
                    jeryu_runner_core::receipt::now_ms(),
                    TEST_SHIFT_DB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ))
            }
            #[cfg(not(test))]
            {
                data_dir.join("shift.sqlite")
            }
        };
        let shift = shift::ShiftState::open(&shift_path);
        let events = pipeline::EventStore::open(&shift_path).expect("open pipeline event store");
        let disputes =
            jankurai::DisputeStore::open(&shift_path).expect("open jankurai dispute store");
        // Pre-warm the agent pool over the real CLI lifecycle. With the OCI gate
        // closed this only records planned cells (no daemon), so construction is
        // infallible in every environment the web edge boots in.
        let warm_runtime: Arc<dyn ContainerLifecycle> = Arc::new(CliContainerRuntime);
        let warm_pool = Arc::new(Mutex::new(
            WarmPool::new(warm_runtime, WARM_POOL_TARGET).expect("pre-warm the agent pool"),
        ));
        Self {
            github: GithubRouter::with_core(core)
                .with_repo_manager(repo_manager.clone())
                .with_work_store(work.clone())
                .with_work_bridge_repair_store(&shift_path)
                .expect("open work bridge repair store"),
            tui,
            spa_dir,
            ws: WsHub::new(),
            workcells: Arc::new(Mutex::new(WorkcellManager::new())),
            agent_runs: agent_runs::AgentRunStore::new(),
            gate_runners: control_plane::GateRunnerStore::from_env(),
            release_boards: release_board::ReleaseBoardStore::from_env(),
            merge_queue: Arc::default(),
            merge_attempts: merge_attempts::MergeAttemptStore::default(),
            idempotency: idempotency::IdempotencyStore::default(),
            shift,
            events,
            disputes,
            mirror_state: mirror_reconcile::MirrorStateStore::default(),
            attention: pipeline::attention::AttentionCache::default(),
            pins: pipeline::pins::PinsCache::default(),
            repo_depends: control_plane::DependsCache::default(),
            codegraph_store,
            repo_manager,
            core: core_handle,
            work,
            warm_pool,
            session_runtime: sessions::SessionRuntimeConfig::from_env(),
            #[cfg(test)]
            session_auth_home: Arc::new(tempfile::tempdir().expect("session fixture auth home")),
            split_catalog,
            tool_registry_path: None,
            split_manifests: Vec::new(),
            tool_finder_scan: tool_finder::ToolFinderScanState::default(),
            auth_required: false,
            trust_local_dev: true,
            secure_cookies: false,
            auth_rate_limits: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn with_auth(mut self, required: bool, trust_local_dev: bool, secure_cookies: bool) -> Self {
        self.auth_required = required;
        self.trust_local_dev = trust_local_dev;
        self.secure_cookies = secure_cookies;
        self
    }

    /// Point the golden-box endpoint at `jeryu-tool/tools-registry.toml`.
    /// Production-only chaining in `serve()`; tests leave it unset.
    fn with_tool_registry_path(mut self, path: Option<PathBuf>) -> Self {
        self.tool_registry_path = path;
        self
    }

    /// Hand the tool-finder the split manifests so the system scan can derive
    /// its family-discovery parents. Production-only chaining in `serve()`.
    fn with_split_manifests(mut self, manifests: Vec<PathBuf>) -> Self {
        self.split_manifests = manifests;
        self
    }

    /// Attach the merge-to-GitHub mirror (loaded from the split manifest) to
    /// the embedded GitHub router. Production-only chaining in `serve()`;
    /// every other constructor leaves the mirror absent, so no test or
    /// embedded caller ever attempts a network push.
    fn with_github_mirror(mut self, mirror: Arc<crate::github_mirror::GithubMirror>) -> Self {
        self.github = self.github.with_github_mirror(mirror);
        self
    }

    /// Test-only constructor with a throwaway git storage root; the in-process
    /// router tests never exercise the smart-HTTP transport.
    #[cfg(test)]
    fn new(core: ForgeCore) -> Self {
        Self::with_repo_manager(
            core,
            Arc::new(RepoManager::new(GitdConfig::new(
                std::env::temp_dir().join("jeryu-web-test-git"),
            ))),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/web/dist"),
            std::env::temp_dir(),
            SplitCatalog::builtin(),
        )
    }

    /// Test-only constructor that roots the git `RepoManager` at `storage_root`
    /// so the workcell export slice gate can run a real `git diff` against a
    /// fixture bare repository.
    #[cfg(test)]
    fn new_with_git_storage(core: ForgeCore, storage_root: PathBuf) -> Self {
        Self::with_repo_manager(
            core,
            Arc::new(RepoManager::new(GitdConfig::new(storage_root))),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/web/dist"),
            std::env::temp_dir(),
            SplitCatalog::builtin(),
        )
    }

    /// Test-only constructor that roots the git `RepoManager` at `storage_root`
    /// AND injects a [`WarmPool`] built over the given container lifecycle, so the
    /// claim path can be driven with a recording `FakeContainerRuntime` (no
    /// Docker/Podman) while still resolving a real bare repository for branch
    /// registration. The pool pre-warms `warm_target` cells.
    #[cfg(test)]
    fn new_with_git_storage_and_warm_pool(
        core: ForgeCore,
        storage_root: PathBuf,
        warm_runtime: Arc<dyn ContainerLifecycle>,
        warm_target: usize,
    ) -> Self {
        let mut state = Self::new_with_git_storage(core, storage_root);
        state.warm_pool = Arc::new(Mutex::new(
            WarmPool::new(warm_runtime, warm_target).expect("pre-warm the test agent pool"),
        ));
        state
    }

    /// Test-only: override the session runtime backend + docker seam directly so a
    /// hermetic test drives the docker / native paths without mutating process-wide
    /// env (the crate forbids `unsafe`, so `std::env::set_var` is unavailable).
    #[cfg(test)]
    pub(crate) fn with_session_runtime(mut self, runtime: sessions::SessionRuntimeConfig) -> Self {
        self.session_runtime = runtime;
        self
    }
}

/// Live-stream fan-out hub for the WebSocket event spine.
///
/// Hands out the server-wide monotonic event sequence, tracks which scopes
/// each live connection is subscribed to, and fans producer events out to
/// exactly the interested connections through their registered outbound
/// queues ([`WsHub::publish`]). The snapshot-on-subscribe path also rides
/// this hub.
#[derive(Clone, Default)]
struct WsHub {
    inner: Arc<Mutex<WsHubInner>>,
}

#[derive(Default)]
struct WsHubInner {
    /// Server-wide monotonic event sequence; never reused, never decreases.
    next_seq: u64,
    /// Dedicated connection-id counter (never reused).
    next_conn_id: u64,
    /// Live connections, in registration order. Each tracks its own scopes.
    connections: Vec<WsConnection>,
}

/// A single live WebSocket connection's subscription state inside the hub.
struct WsConnection {
    id: u64,
    scopes: BTreeSet<String>,
    /// Outbound push lane drained by the connection's socket loop.
    sender: UnboundedSender<ServerWsMessage>,
}

impl WsHub {
    fn new() -> Self {
        Self::default()
    }

    /// Allocate the next monotonic event sequence number.
    fn next_seq(&self) -> u64 {
        let mut inner = self.inner.lock().expect("ws hub mutex poisoned");
        inner.next_seq = inner.next_seq.saturating_add(1);
        inner.next_seq
    }

    /// The highest sequence handed out so far (0 before any event).
    fn current_seq(&self) -> u64 {
        self.inner.lock().expect("ws hub mutex poisoned").next_seq
    }

    /// Register a fresh connection (with its outbound queue) and return its
    /// hub-unique id.
    fn register(&self, sender: UnboundedSender<ServerWsMessage>) -> u64 {
        let mut inner = self.inner.lock().expect("ws hub mutex poisoned");
        inner.next_conn_id = inner.next_conn_id.saturating_add(1);
        let id = inner.next_conn_id;
        inner.connections.push(WsConnection {
            id,
            scopes: BTreeSet::new(),
            sender,
        });
        id
    }

    /// Allocate a sequence, build the event once, and queue an `Event` frame
    /// to every connection subscribed to `scope`. Connections whose socket
    /// loop has gone away (receiver dropped) are pruned. Returns how many
    /// connections the event was queued to. Safe to call from blocking
    /// threads: `UnboundedSender::send` never blocks.
    fn publish(&self, scope: &str, make_event: impl FnOnce(u64) -> WebEvent) -> usize {
        let mut inner = self.inner.lock().expect("ws hub mutex poisoned");
        inner.next_seq = inner.next_seq.saturating_add(1);
        let frame = ServerWsMessage::Event {
            event: make_event(inner.next_seq),
        };
        let mut delivered = 0;
        inner.connections.retain(|conn| {
            if !conn.scopes.contains(scope) {
                return true;
            }
            match conn.sender.send(frame.clone()) {
                Ok(()) => {
                    delivered += 1;
                    true
                }
                Err(_) => false,
            }
        });
        delivered
    }

    /// Replace the scope set a connection is subscribed to.
    fn set_scopes(&self, id: u64, scopes: &BTreeSet<String>) {
        let mut inner = self.inner.lock().expect("ws hub mutex poisoned");
        if let Some(conn) = inner.connections.iter_mut().find(|c| c.id == id) {
            conn.scopes = scopes.clone();
        }
    }

    /// Drop scopes from a connection's subscription set.
    fn remove_scopes(&self, id: u64, scopes: &[String]) {
        let mut inner = self.inner.lock().expect("ws hub mutex poisoned");
        if let Some(conn) = inner.connections.iter_mut().find(|c| c.id == id) {
            for scope in scopes {
                conn.scopes.remove(scope);
            }
        }
    }

    /// Forget a connection entirely (on socket close).
    fn unregister(&self, id: u64) {
        let mut inner = self.inner.lock().expect("ws hub mutex poisoned");
        inner.connections.retain(|c| c.id != id);
    }
}

pub async fn serve(config: WebServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    if config.trust_local_dev && !config.bind.ip().is_loopback() {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "trust_local_dev requires a loopback bind address",
        )));
    }
    std::fs::create_dir_all(&config.data_dir)?;
    std::fs::create_dir_all(&config.git_storage_root)?;
    let db_path = config.data_dir.join("forge.sqlite");
    // Share one RepoManager between the create-repo materializer (so a created
    // repo gets a bare repo on disk) and the smart-HTTP transport (so it can be
    // cloned/pushed) — both rooted at the same git storage root.
    //
    // The materializer's manager does not follow redirects: creating or moving
    // a repository must act on the exact slug asked for. The transport's
    // manager does, so a clone by a renamed repository's old URL still works.
    let base_manager = RepoManager::new(GitdConfig::new(config.git_storage_root.clone()));
    let materializer = Arc::new(GitMaterializer::new(Arc::new(base_manager.clone())));
    let core = ForgeCore::open_sqlite(db_path)?
        .with_repo_materializer(materializer.clone())
        .with_repo_relocator(materializer);
    let repo_manager =
        Arc::new(base_manager.with_redirects(Arc::new(CoreRedirects::new(core.clone()))));
    let split_catalog = SplitCatalog::load(&config.split_manifests);
    let tool_registry_path = resolve_tool_registry_path(&config.split_manifests);
    // Merge-to-GitHub mirroring: targets come from the same manifest; with no
    // manifest (or JERYU_GITHUB_PUSH=0) the mirror loads disabled and merges
    // never attempt a push.
    let github_mirror = Arc::new(crate::github_mirror::GithubMirror::load(
        &config.split_manifests,
    ));
    let state = WebState::with_repo_manager(
        core,
        repo_manager,
        config.spa_dir.clone(),
        config.data_dir.clone(),
        split_catalog,
    )
    .with_github_mirror(github_mirror)
    .with_tool_registry_path(tool_registry_path)
    .with_split_manifests(config.split_manifests.clone())
    .with_auth(
        config.auth_required,
        config.trust_local_dev,
        config.secure_cookies,
    );
    bootstrap_public_accounts(&state, &config.data_dir)?;
    // The forge core protects every repository's default branch on startup;
    // todoq pushes claims straight to a queue repo's `queue` branch, so the
    // family queues are opted back out right after that backfill.
    shift::exempt_queues_from_default_branch_protection(&state, BOOTSTRAP_ADMIN_LOGIN);
    let state = shared_state(state, &config.spa_dir);
    tool_finder_schedule::spawn(state.clone());
    merge_queue::spawn_worker(state.clone(), std::time::Duration::from_secs(10));
    mirror_reconcile::spawn(state.clone());
    let app = router(state);
    let listener = TcpListener::bind(config.bind).await?;
    // ConnectInfo gives the git handlers the peer address so the gitd auth layer
    // can apply its loopback-permissive policy.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

mod bootstrap;

use bootstrap::bootstrap_public_accounts;
#[cfg(test)]
use bootstrap::bootstrap_public_accounts_with_admin_password;

#[cfg(test)]
fn app(state: WebState, spa_dir: &Path) -> AxumRouter {
    router(shared_state(state, spa_dir))
}

fn shared_state(mut state: WebState, spa_dir: &Path) -> Arc<WebState> {
    state.spa_dir = spa_dir.to_path_buf();
    Arc::new(state)
}

fn router(state: Arc<WebState>) -> AxumRouter {
    // `Router::layer` runs after routing, so the owner/name rewrite wraps the
    // whole routed app as the fallback of an otherwise empty router.
    let routed = routes(state.clone());
    AxumRouter::new().fallback_service(tower::ServiceExt::map_request(
        routed,
        move |request: Request| {
            repo_address::rewrite(&state, request_rules::trim_trailing_slash(request))
        },
    ))
}

fn routes(state: Arc<WebState>) -> AxumRouter {
    let mcp_state = Arc::new(jeryu_mcp::McpHttpState::new(Arc::new(
        mcp_backend::WebMcpBackend::new(state.clone()),
    )));
    let mcp_router = jeryu_mcp::mcp_router(mcp_state)
        .layer(from_fn(steer_headers))
        // Inside the gate, so the authenticated account it resolved reaches the
        // backend: the MCP tool context only carries a client-declared name.
        .layer(from_fn(mcp_backend::scope_caller))
        .layer(from_fn_with_state(state.clone(), auth::gate))
        .layer(from_fn(request_id::propagate));
    api_v1_routes()
        .into_iter()
        .fold(AxumRouter::new(), |router, (path, handlers)| {
            router.route(path, handlers)
        })
        .route("/health", get(health))
        // Steering surface: advertises the faster jeryu/MCP path so external
        // agents stuck on bespoke `gh` commands can discover it.
        .route("/.jeryu/capabilities", get(capabilities))
        .route("/api/v1", get(route_index::api_v1))
        .route("/api/v1/", get(route_index::api_v1))
        .route("/graphql", post(graphql))
        // GitHub-compatible REST edge — every request is forwarded to the
        // in-process `GithubRouter`, so the real `gh` CLI and any GitHub client
        // work against this live server (was built but never mounted).
        .route("/user", any(github_forward))
        .route("/users/:login", any(github_forward))
        .route("/api/v1/version", any(github_forward))
        .route("/api/v3", any(github_forward))
        .route("/api/v3/user", any(github_forward))
        .route("/api/v3/users/:login", any(github_forward))
        .route("/api/v3/repos", any(repo_entry))
        .route("/api/v3/repos/*rest", any(repo_entry))
        .route("/api/v3/graphql", any(github_forward))
        .route("/repos", any(repo_entry))
        .route("/repos/*rest", any(repo_entry))
        // Explicitly catch gh auth login/device-flow attempts so agents get a
        // typed Jeryu repair path instead of falling through to the SPA.
        .route("/login/*rest", any(github_forward))
        .route("/api/v3/login/*rest", any(github_forward))
        // Steering: first-contact doc for a confused agent on the REST edge.
        .route("/.jeryu/agents/first-contact", any(github_forward))
        // Git smart-HTTP transport on the unified listener so `git clone`/`push`
        // work against this server. Mounted under `/git/` to stay clear of the
        // GitHub-shaped REST routes above: a root-level `:owner` param would
        // conflict with the literal `/repos`, `/users`, ... routes in the matcher.
        .merge(
            AxumRouter::new()
                .route(
                    "/git/:owner/:repo/info/refs",
                    get(crate::git_transport::git_info_refs),
                )
                .route(
                    "/git/:owner/:repo/git-upload-pack",
                    post(crate::git_transport::git_upload_pack),
                )
                .route(
                    "/git/:owner/:repo/git-receive-pack",
                    post(crate::git_transport::git_receive_pack),
                )
                .route(
                    "/git/:owner/:repo/info/lfs/objects/batch",
                    post(crate::git_transport::git_lfs_batch),
                )
                .route(
                    "/git/:owner/:repo/info/lfs/objects/:oid",
                    get(crate::git_transport::git_lfs_download)
                        .put(crate::git_transport::git_lfs_upload),
                )
                .route(
                    "/git/:owner/:repo/info/lfs/objects/:oid/verify",
                    post(crate::git_transport::git_lfs_verify),
                )
                .route(
                    "/git/:owner/:repo/info/lfs/locks/verify",
                    post(crate::git_transport::git_lfs_locks_verify),
                )
                .route_layer(DefaultBodyLimit::disable()),
        )
        .fallback(surface::spa_fallback)
        // Innermost: a replayed write skips the handler, never the auth gate.
        .layer(from_fn_with_state(
            state.idempotency.clone(),
            idempotency::replay,
        ))
        // Response middleware that stamps every reply with advisory steering
        // headers (and a per-route MCP tool hint for gh/automation UAs).
        // Inside the auth gate: a 304 only answers a caller allowed the body.
        .layer(from_fn(conditional::etag))
        .layer(from_fn(steer_headers))
        .layer(from_fn_with_state(state.clone(), auth::gate))
        // Outside the auth gate so a merge refused there is recorded too.
        .layer(from_fn_with_state(state.clone(), merge_attempts::observe))
        // Preflight and Accept answer before the auth gate, inside the envelope.
        .layer(from_fn(request_rules::apply))
        // Outside the auth gate so its 401/403 answers take the envelope too.
        .layer(from_fn(error_envelope::normalize))
        .layer(from_fn(request_id::propagate))
        .with_state(state)
        .merge(mcp_router)
}

/// Every `/api/v1` route with its handlers. The router mounts this list and
/// `GET /api/v1` indexes it, so the published index cannot drift from what is
/// actually served.
fn api_v1_routes() -> Vec<(&'static str, MethodRouter<Arc<WebState>>)> {
    vec![
        ("/api/v1/errors", get(error_envelope::catalog)),
        ("/api/v1/search", get(search::search)),
        ("/api/v1/bootstrap", get(bootstrap)),
        ("/api/v1/read-model/tui", get(tui_read_model)),
        // The suffixed spelling reads like a content-type negotiation it never
        // was; kept as an alias so clients can move at their own pace.
        ("/api/v1/bootstrap.tui", get(tui_read_model)),
        ("/api/v1/work", get(work::list).post(work::create)),
        ("/api/v1/work/:key", get(work::detail).patch(work::patch)),
        ("/api/v1/work/:key/comments", post(work::comment)),
        ("/api/v1/work/:key/links", post(work::link)),
        ("/api/v1/auth/signup", post(auth::signup)),
        ("/api/v1/auth/login", post(auth::login)),
        ("/api/v1/auth/logout", post(auth::logout)),
        ("/api/v1/auth/me", get(auth::me)),
        ("/api/v1/auth/password", post(auth::change_password)),
        (
            "/api/v1/auth/tokens",
            get(auth::list_tokens).post(auth::create_token),
        ),
        (
            "/api/v1/auth/tokens/:id",
            axum::routing::delete(auth::delete_token),
        ),
        ("/api/v1/admin/users", get(auth::admin_users)),
        (
            "/api/v1/admin/users/:login/reset-password",
            post(auth::admin_reset_password),
        ),
        (
            "/api/v1/admin/repos/:owner/:repo/grants",
            get(auth::admin_repo_grants),
        ),
        (
            "/api/v1/admin/repos/:owner/:repo/grants/:login",
            post(auth::admin_grant_repo).delete(auth::admin_revoke_repo),
        ),
        (
            "/api/v1/agent-runs",
            get(agent_runs::list).post(agent_runs::start),
        ),
        ("/api/v1/agent-runs/:id", get(agent_runs::status)),
        ("/api/v1/agent-runs/:id/events", get(agent_runs::events)),
        // Live raw-TTY push transport (Server-Sent Events). An outside service such
        // as jpmc subscribes once and is streamed raw bytes as they publish, instead
        // of cursor-polling agent_work.tail; it replays the retained ring on connect.
        (
            "/api/v1/agent-runs/:id/tty/stream",
            get(agent_runs::tty_stream),
        ),
        ("/api/v1/agent-runs/:id/control", post(agent_runs::control)),
        ("/api/v1/agent-runs/:id/shell", post(agent_runs::shell)),
        (
            "/api/v1/agent-runs/:id/export_pr",
            post(agent_runs::export_pr),
        ),
        // Host-mediated publish: advance the session branch ref + open a PR. The
        // agent never pushes; the ref move goes through the protected ref service.
        ("/api/v1/agent-runs/:id/publish", post(sessions::publish)),
        (
            "/api/v1/workcells",
            get(workcells::list).post(workcells::claim),
        ),
        (
            "/api/v1/workcells/repair_live",
            post(workcells::repair_live),
        ),
        ("/api/v1/workcells/:id", get(workcells::status)),
        (
            "/api/v1/workcells/:id/heartbeat",
            post(workcells::heartbeat),
        ),
        ("/api/v1/workcells/:id/release", post(workcells::release)),
        (
            "/api/v1/workcells/:id/run_agent",
            post(workcells::run_agent),
        ),
        (
            "/api/v1/workcells/:id/export_pr",
            post(workcells::export_pr),
        ),
        ("/api/v1/repos", get(repos).post(repository_create::create)),
        ("/api/v1/repos/preview", post(repository_create::preview)),
        ("/api/v1/releases", get(operator_resources::releases)),
        ("/api/v1/mirrors", get(operator_resources::mirrors)),
        ("/api/v1/settings", get(operator_resources::settings)),
        ("/api/v1/audit", get(operator_resources::audit)),
        (
            "/api/v1/repos/:id",
            get(repo_detail)
                .patch(repo_update)
                .delete(repo_admin::repo_delete),
        ),
        // Repo-scoped agent sessions: launch a hardened session, and the live
        // per-repo agent-runs list the web Active-Agents page consumes.
        ("/api/v1/repos/:id/sessions", post(sessions::create)),
        ("/api/v1/repos/:id/agent-runs", get(sessions::list)),
        (
            "/api/v1/repos/:id/work",
            get(work::repo_list).post(work::repo_create),
        ),
        ("/api/v1/repos/:id/pulls", get(pulls::list)),
        ("/api/v1/repos/:id/pulls/:number", get(pulls::detail)),
        ("/api/v1/repos/:id/pulls/:number/diff", get(pulls::diff)),
        ("/api/v1/repos/:id/pulls/:number/checks", get(pulls::checks)),
        (
            "/api/v1/repos/:id/pulls/:number/threads",
            get(pulls::threads),
        ),
        (
            "/api/v1/repos/:id/pulls/:number/reviews",
            post(pulls::review),
        ),
        (
            "/api/v1/repos/:id/pulls/:number/comments",
            post(pulls::comment),
        ),
        (
            "/api/v1/repos/:id/pulls/:number/approve",
            post(pulls::approve),
        ),
        ("/api/v1/repos/:id/pulls/:number/merge", post(pulls::merge)),
        // The draft lifecycle. `PATCH /api/v3/repos/{owner}/{repo}/pulls/{n}`
        // with `{"draft": …}` stays the `gh`-compatible way in; these are the
        // named routes the index lists and the PR page's buttons call.
        (
            "/api/v1/repos/:id/pulls/:number/ready",
            post(pulls::ready_for_review),
        ),
        (
            "/api/v1/repos/:id/pulls/:number/draft",
            post(pulls::convert_to_draft),
        ),
        (
            "/api/v1/repos/:id/pulls/:number/queue",
            post(merge_queue::enqueue).delete(merge_queue::dequeue),
        ),
        (
            "/api/v1/repos/:id/pulls/:number/merge-attempt",
            get(merge_attempts::show),
        ),
        ("/api/v1/repos/:id/merge-queue", get(merge_queue::list_repo)),
        // What acts on the repository (checks, reviewer, merger and their
        // grants, runners, deployers) and where it is mirrored to.
        ("/api/v1/repos/:id/automation", get(repo_automation::show)),
        ("/api/v1/merge-queue", get(merge_queue::list_all)),
        (
            "/api/v1/repos/:id/jankurai-scores",
            get(repo_jankurai_scores_list).post(repo_jankurai_scores_ingest),
        ),
        // The audit queue a push writes and the gate runners drain. Runners
        // claim work here and submit the report to the score ingest above;
        // the forge itself never runs the auditor.
        ("/api/v1/jankurai-audits", get(jankurai::audits::list)),
        (
            "/api/v1/jankurai-audits/claim",
            post(jankurai::audits::claim),
        ),
        ("/api/v1/fleet/tool-adoption", get(fleet_tool_adoption)),
        // Quality-gate visibility: how the jankurai/proof gate has behaved,
        // before anyone makes it required. Reads need a login; filing a
        // dispute is admin-only.
        ("/api/v1/jankurai/overview", get(jankurai::overview)),
        (
            "/api/v1/jankurai/rules/:rule_id",
            get(jankurai::rule_detail),
        ),
        (
            "/api/v1/jankurai/scores/:score_id",
            get(jankurai::score_detail),
        ),
        (
            "/api/v1/jankurai/disputes",
            get(jankurai::dispute_list).post(jankurai::dispute_create),
        ),
        // The same data shaped for the web console's Quality gate pages.
        (
            "/api/v1/quality-gate/overview",
            get(jankurai::quality_gate::overview),
        ),
        (
            "/api/v1/quality-gate/rules/:rule",
            get(jankurai::quality_gate::rule),
        ),
        (
            "/api/v1/quality-gate/heads/:owner/:name/:sha",
            get(jankurai::quality_gate::head),
        ),
        (
            "/api/v1/quality-gate/findings/:id/dispute",
            post(jankurai::quality_gate::dispute),
        ),
        (
            "/api/v1/tools/registry/summary",
            get(tool_registry::summary),
        ),
        ("/api/v1/repos/:id/refs", get(repo_refs)),
        ("/api/v1/repos/:id/commits", get(repo_commits)),
        ("/api/v1/repos/:id/compare", get(repo_compare)),
        ("/api/v1/repos/:id/release-tag", get(repo_release_tag)),
        ("/api/v1/deployments", get(deployed_repositories)),
        ("/api/v1/repos/:id/tree", get(repo_tree)),
        ("/api/v1/repos/:id/blob", get(repo_blob)),
        ("/api/v1/repos/:id/raw", get(repo_raw)),
        ("/api/v1/repos/:id/codegraph/query", post(codegraph::query)),
        (
            "/api/v1/codegraph/tool-build/status",
            get(tool_build::status),
        ),
        (
            "/api/v1/codegraph/tool-build/clusters",
            get(tool_build::clusters),
        ),
        (
            "/api/v1/codegraph/tool-build/clusters/:id/feedback",
            post(tool_build::feedback),
        ),
        // System-wide tool-finder: live scan trigger/status, the /tools
        // pattern-family dashboard, and cluster -> registry proposal.
        (
            "/api/v1/tool-finder/scan",
            get(tool_finder::scan_status).post(tool_finder::scan_start),
        ),
        ("/api/v1/tool-finder/source", get(tool_finder::source)),
        ("/api/v1/tool-finder/dashboard", get(tool_finder::dashboard)),
        (
            "/api/v1/tool-finder/propose/:cluster_id",
            post(tool_finder::propose),
        ),
        (
            "/api/v1/tool-finder/proposals/:tool_id/decision",
            post(tool_proposals::decide),
        ),
        // Shift pages: todoq family queues, slot heartbeats, shift branches.
        // Reads need a login; every POST is admin-only (auth::admin_only_request).
        (
            "/api/v1/events",
            get(pipeline::list_events).post(pipeline::post_events),
        ),
        ("/api/v1/attention", get(pipeline::attention::attention)),
        ("/api/v1/pins", get(pipeline::pins::pins)),
        ("/api/v1/release-board", get(release_board::list_boards)),
        (
            "/api/v1/release-board/:family",
            get(release_board::get_board).put(release_board::put_board),
        ),
        ("/api/v1/shift/families", get(shift::families)),
        (
            "/api/v1/shift/todos",
            get(shift::list_todos).post(shift::file_todos),
        ),
        (
            "/api/v1/shift/todos/:family/:id/action",
            post(shift::todo_action),
        ),
        ("/api/v1/shift/heartbeat", post(shift::heartbeat)),
        ("/api/v1/shift/workers", get(shift::workers)),
        ("/api/v1/shift/workers/history", get(shift::workers_history)),
        ("/api/v1/shift/shifts", get(shift::list_shifts)),
        (
            "/api/v1/shift/shifts/:family/pr",
            post(shift::open_shift_pr),
        ),
        ("/api/v1/control-plane/status", get(control_plane::status)),
        (
            "/api/v1/control-plane/priorities",
            get(control_plane::priorities),
        ),
        (
            "/api/v1/control-plane/repo-graph",
            get(control_plane::repo_graph),
        ),
        (
            "/api/v1/control-plane/artifacts/latest",
            get(control_plane::artifacts_latest),
        ),
        ("/api/v1/control-plane/runners", get(control_plane::runners)),
        (
            "/api/v1/runners/heartbeat",
            post(control_plane::runner_heartbeat),
        ),
        (
            "/api/v1/repos/:id/readme",
            get(repo_readme).put(repo_readme_update),
        ),
        // Read-only ecosystem surface for generic external clients: the live
        // tool-graph and per-CI-run evidence. Additive, never mutating.
        ("/api/v1/ecosystem", get(ecosystem)),
        ("/api/v1/ci/runs/:id/evidence", get(ci_run_evidence)),
        ("/api/v1/markdown/render", post(markdown_render)),
        ("/api/v1/ws", get(ws::ws)),
    ]
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "service": "jeryu-api" }))
}

const HDR_API: &str = "x-jeryu-api";
const HDR_FAST_PATH: &str = "x-jeryu-fast-path";
const HDR_TOOL: &str = "x-jeryu-tool";

/// Response middleware: stamps every reply with advisory steering headers. For
/// `gh`/automation user-agents it also injects a suggested jeryu MCP tool for
/// the request's route+method, nudging external agents off bespoke `gh`
/// invocations and onto the faster MCP path. Cheap and infallible: it never
/// fails the request and only ever appends headers.
async fn steer_headers(request: Request, next: Next) -> AxumResponse {
    let user_agent = request
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in advisory_headers(&user_agent, &method, &path) {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
    response
}

/// Pure builder for the advisory steering headers. Always emits the API version
/// and fast-path pointer; for `gh`/automation/agent user-agents it additionally
/// emits a per-route MCP tool hint when one is known. Factored out of the
/// middleware so the header policy can be unit-tested without a live server.
fn advisory_headers(
    user_agent: &str,
    method: &HttpMethod,
    path: &str,
) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        (HDR_API, "v4".to_string()),
        (HDR_FAST_PATH, "/.jeryu/capabilities".to_string()),
    ];
    if is_automation_agent(user_agent)
        && let Some(tool) = suggested_tool(method, path)
    {
        headers.push((HDR_TOOL, tool.to_string()));
    }
    headers
}

/// Heuristic: does this user-agent look like the `gh` CLI, a generic HTTP
/// client used by automation, or a Jeryu/agent UA? Matched case-insensitively.
fn is_automation_agent(user_agent: &str) -> bool {
    let ua = user_agent.to_ascii_lowercase();
    const NEEDLES: [&str; 7] = [
        "github cli",
        "go-gh",
        "okhttp",
        "curl",
        "python-requests",
        "jeryu",
        "agent",
    ];
    NEEDLES.iter().any(|needle| ua.contains(needle))
}

/// Suggests the jeryu MCP tool for a route+method so steered agents can switch
/// to the faster path. Mutations map to dedicated MCP tools; all other GETs map
/// to the generic read tool. Returns `None` when no hint applies.
fn suggested_tool(method: &HttpMethod, path: &str) -> Option<&'static str> {
    let trimmed = path.trim_end_matches('/');
    match *method {
        HttpMethod::POST if trimmed.ends_with("/pulls") => Some(MCP_PATCH_TOOL),
        HttpMethod::POST if trimmed.contains("/actions/") => Some(MCP_RUN_TESTS_TOOL),
        HttpMethod::PUT if trimmed.ends_with("/merge") => Some(MCP_MERGE_TOOL),
        HttpMethod::POST if trimmed.ends_with("/issues") => Some(MCP_ISSUE_TOOL),
        HttpMethod::GET if trimmed.contains("/actions/") => Some(MCP_CHECKS_TOOL),
        HttpMethod::GET if trimmed.contains("/check-runs") => Some(MCP_CHECKS_TOOL),
        HttpMethod::GET if trimmed.contains("/pulls") => Some(MCP_BLOCKERS_TOOL),
        HttpMethod::GET => Some(MCP_READ_TOOL),
        _ => None,
    }
}

/// Capability manifest: advertises the live endpoints plus a `gh` command -> jeryu
/// mapping so external agents can discover and prefer the faster MCP path.
async fn capabilities(State(state): State<Arc<WebState>>) -> Json<Value> {
    Json(capabilities_payload(&live_mcp_tools(&state)))
}

/// The MCP tool names this server actually dispatches, read from the live
/// backend's catalog. The manifest is built from these so it can never steer an
/// agent at a tool the `/mcp` endpoint would answer "unknown tool" for.
fn live_mcp_tools(state: &Arc<WebState>) -> BTreeSet<String> {
    use jeryu_mcp::ToolBackend;
    mcp_backend::WebMcpBackend::new(state.clone())
        .list()
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

/// Pure builder for the `/.jeryu/capabilities` payload (unit-testable).
///
/// `tools` is the live backend catalog: every MCP tool named here is looked up
/// in it first, and a `gh` command whose jeryu answer is a tool that is not
/// installed is left out of the map rather than advertised.
fn capabilities_payload(tools: &BTreeSet<String>) -> Value {
    let installed = |tool: &str| tools.contains(tool);
    let mut gh_command_map = serde_json::Map::new();
    let mut map_rest = |command: &str, target: &str| {
        gh_command_map.insert(command.to_string(), Value::String(target.to_string()));
    };
    map_rest(
        "gh auth login",
        &format!("Do not run direct gh auth against a Jeryu host; run {GH_SETUP_COMMAND} instead."),
    );
    map_rest(
        "gh auth refresh",
        &format!(
            "Do not refresh host auth manually; rerun {GH_SETUP_REPAIR_COMMAND} for the Jeryu host entry."
        ),
    );
    map_rest(
        "gh auth status",
        &format!(
            "If status fails for the Jeryu host, do not start a login flow; rerun {GH_SETUP_REPAIR_COMMAND} and inspect /.jeryu/capabilities."
        ),
    );
    map_rest("gh pr list", "GET /repos/{owner}/{repo}/pulls");
    map_rest(
        "gh workflow list",
        "GET /repos/{owner}/{repo}/actions/workflows",
    );
    map_rest(
        "gh workflow view",
        "GET /repos/{owner}/{repo}/actions/workflows/{workflow_id}",
    );
    map_rest("gh run list", "GET /repos/{owner}/{repo}/actions/runs");
    map_rest("gh run view", "GET /repos/{owner}/{repo}/actions/runs/{id}");
    map_rest(
        "gh api",
        "Use /.jeryu/capabilities and the listed jeryu.* MCP tools; unsupported REST returns guided JSON.",
    );
    map_rest("gh repo create", "POST /repos");
    for (command, tool) in [
        ("gh pr create", MCP_PATCH_TOOL),
        ("gh pr merge", MCP_MERGE_TOOL),
        ("gh workflow run", MCP_RUN_TESTS_TOOL),
        ("gh run rerun", MCP_RUN_TESTS_TOOL),
        ("gh run cancel", MCP_RUN_TESTS_TOOL),
        ("gh issue create", MCP_ISSUE_TOOL),
    ] {
        if installed(tool) {
            gh_command_map.insert(command.to_string(), Value::String(tool.to_string()));
        }
    }

    let mut gh_auth_policy = json!({
        "do_not_run": ["gh auth login", "gh auth refresh", "credential-store token hunting"],
        "run_instead": GH_SETUP_COMMAND,
        "token_file": GH_SETUP_TOKEN_FILE,
        "stale_host_repair": GH_SETUP_REPAIR_COMMAND,
        "host_auth_boundary": GH_AUTH_BOUNDARY,
    });
    // `jeryu agent auth` only has something to prepare credentials for when the
    // agent-run tools are dispatched here, so the hint follows the backend.
    if installed(MCP_AGENT_WORK_TOOL) {
        gh_auth_policy["agent_auth"] = Value::String(
            "jeryu agent auth doctor <tool>; jeryu agent auth import --from-host <tool>"
                .to_string(),
        );
    }

    json!({
        "server": "jeryu",
        "api_version": "v4",
        "graphql": "/graphql",
        "websocket": "/api/v1/ws",
        "mcp_endpoint": "/mcp",
        "mcp_tools": tools.iter().cloned().collect::<Vec<_>>(),
        "gh_command_map": Value::Object(gh_command_map),
        "gh_auth_policy": gh_auth_policy,
        "web_feature_flags": {
            "purpose": "What each /api/v1/bootstrap feature flag gates, and what turns it on for a viewer. A flag reported off means this viewer lacks the grant, or the note says the flag is admin-only — never that the code is missing.",
            "flags": permissions::FEATURE_FLAG_NOTES
                .iter()
                .map(|(flag, note)| ((*flag).to_string(), Value::String((*note).to_string())))
                .collect::<serde_json::Map<String, Value>>(),
        },
        "fast_path_advice":
            "Prefer the jeryu MCP tools for mutations; gh REST/GraphQL is supported but slower.",
    })
}

async fn bootstrap(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> AxumResponse {
    match bootstrap_payload_for_user(&state, &account) {
        Ok(payload) => Json(payload).into_response(),
        Err(err) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "serialization_failed",
            &format!("bootstrap payload serialization failed: {err}"),
        ),
    }
}

/// `GET /api/v1/read-model/tui` — the TUI read model as a resource of its own,
/// fetched by the clients that project it rather than shipped inside every
/// bootstrap. Also served at `/api/v1/bootstrap.tui` for clients still asking
/// for that path.
async fn tui_read_model(State(state): State<Arc<WebState>>) -> Json<TuiReadModel> {
    Json(workcells::live_tui(&state))
}

/// `GET /api/v1/ecosystem` — the live ecosystem tool-graph for generic external
/// clients. Sources real data from the MCP catalog, the forge, and the live
/// read-model; read-only, never mutates state.
async fn ecosystem(State(state): State<Arc<WebState>>) -> AxumResponse {
    Json(ecosystem::ecosystem_response(state.github.core())).into_response()
}

/// `GET /api/v1/ci/runs/{id}/evidence` — the derived evidence list for a CI run
/// (a check-run keyed by UUID). Returns a structured 404 when the run id does
/// not resolve to a live run, never a silent empty list.
async fn ci_run_evidence(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    match ci_evidence::run_evidence(state.github.core(), &account, &id) {
        Some(evidence) => Json(evidence).into_response(),
        None => ci_evidence_not_found_error(),
    }
}

pub(super) fn server_time() -> String {
    chrono_like_now()
}

/// Wall-clock time at serialization. Never read this from a read-model
/// default: that value is fixed when the process starts.
fn chrono_like_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub(super) fn api_error(status: StatusCode, code: &str, message: &str) -> AxumResponse {
    (status, Json(json!({ "code": code, "message": message }))).into_response()
}

fn ci_evidence_not_found_error() -> AxumResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "code": "not_found",
            "message": "ci run not found",
            "purpose": "retrieve evidence for one live CI run",
            "reason": "the supplied run id is malformed or does not match any check-run in the live forge",
            "common_fixes": [
                "request a run id returned by GET /repos/{owner}/{repo}/actions/runs",
                "request a check-run id from GET /repos/{owner}/{repo}/commits/{sha}/check-runs",
                "retry after the push-to-CI bridge has registered check-runs for the commit"
            ],
            "docs_url": "/docs/api/ci-run-evidence",
            "repair_hint": "use a live check-run UUID, then retry GET /api/v1/ci/runs/{id}/evidence",
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod agent_runs_tests;

#[cfg(test)]
mod workcell_surface_tests;

#[cfg(test)]
mod deployment_surface_tests;

#[cfg(test)]
mod repository_move_tests;

#[cfg(test)]
mod merge_queue_tests;

#[cfg(test)]
mod paging_tests;

#[cfg(test)]
mod operator_resources_tests;

#[cfg(test)]
mod anonymous_read_tests;
#[cfg(test)]
mod repo_address_tests;
