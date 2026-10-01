//! GitHub-compatible REST edge for the Jeryu forge.
//!
//! This module wraps [`jeryu_core::ForgeCore`] — the typed, HTTP-free forge
//! domain — and renders its values as GitHub-shaped JSON. The JSON field
//! shapes (PR `number`, `head`/`base` refs, check-run `conclusion`, combined
//! commit `state`, branch-protection booleans, etc.) are authored here against
//! Jeryu's own parity assertions, not vendored from any external spec.
//!
//! The dispatcher keeps the in-process [`Response`](crate::routes::Response)
//! contract used by the rest of the API facade so the future Axum/HTTP edge can
//! wrap [`GithubRouter::handle`] without changing product-truth behavior.
//!
//! The router itself lives here; the per-resource route handlers and their
//! GitHub-shaped JSON renderers are grouped by resource into sibling
//! submodules ([`repos`], [`pulls`], [`issues`], [`commit_status`],
//! [`check_runs`], [`branch_protection`], [`releases`], [`hooks`]). Shared
//! request parsing and response helpers live in [`support`].

mod actions;
mod branch_protection;
pub(crate) mod check_runs;
mod commit_status;
mod commits;
mod deployments;
mod graphql;
mod hooks;
mod issues;
pub(crate) mod pulls;
mod releases;
mod repos;
mod support;
mod users;
mod work_bridge_repairs;

use jeryu_core::ForgeCore;
use jeryu_jira::WorkStore;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::routes::Response;

pub(crate) use support::{
    GH_AUTH_BOUNDARY, GH_SETUP_COMMAND, GH_SETUP_REPAIR_COMMAND, GH_SETUP_TOKEN_FILE,
};
#[allow(unused_imports)]
pub(crate) use support::{MCP_GUIDANCE_TOOLS, MCP_RUN_TESTS_TOOL};
use support::{
    Pagination, PullStateSelector, first_contact_response, gh_auth_workaround_response,
    json_response, link_base, not_found,
};
use work_bridge_repairs::WorkBridgeRepairQueue;

/// Semantic version reported by `GET /api/v1/version`.
pub const JERYU_API_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The jeryu-deploy commit this binary was built from, if the build knew it
/// (`JERYU_BUILD_COMMIT` at build time, else the checkout's `git rev-parse HEAD`).
pub const JERYU_BUILD_COMMIT: Option<&str> = option_env!("JERYU_BUILD_COMMIT");

/// The jeryu-web commit pinned in `jeryu-split.lock.toml` at build time.
pub const JERYU_WEB_COMMIT: Option<&str> = option_env!("JERYU_WEB_COMMIT");

/// HTTP method understood by the GitHub-compatible edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    Get,
    Patch,
    Post,
    Put,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkBridgeRepair {
    pub code: String,
    pub operation: String,
    pub owner: String,
    pub repo: String,
    pub issue_number: u64,
    pub work_key: Option<String>,
    pub reason: String,
    pub common_fixes: Vec<String>,
    pub docs_url: String,
    pub repair_hint: String,
}

/// GitHub-compatible REST router backed by an in-memory [`ForgeCore`] store.
///
/// When built with the `web` feature, the router may also carry an optional
/// [`jeryu_gitd::RepoManager`] (via [`GithubRouter::with_repo_manager`]). When
/// present, the PR merge endpoint performs a REAL, gated git merge that
/// advances `refs/heads/<base>` in the bare repo; when absent the merge fails
/// closed with a 503 unless a test opted into the in-memory synthetic-sha merge
/// via [`GithubRouter::with_in_memory_merge`].
#[derive(Clone, Debug, Default)]
pub struct GithubRouter {
    core: ForgeCore,
    work_store: Option<WorkStore>,
    work_bridge_repairs: WorkBridgeRepairQueue,
    #[cfg(feature = "web")]
    repo_manager: Option<std::sync::Arc<jeryu_gitd::RepoManager>>,
    /// Opt-in, tests only: finalize a merge with a synthesized sha when no
    /// [`jeryu_gitd::RepoManager`] is wired. Off everywhere else, so a server
    /// missing its git backend fails the merge closed instead of recording a
    /// sha no repository holds.
    #[cfg(feature = "web")]
    in_memory_merge: bool,
    #[cfg(feature = "web")]
    github_mirror: Option<std::sync::Arc<crate::github_mirror::GithubMirror>>,
}

impl GithubRouter {
    /// Builds a router over a fresh in-memory forge store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a router over an existing forge store.
    pub fn with_core(core: ForgeCore) -> Self {
        Self {
            core,
            work_store: None,
            work_bridge_repairs: WorkBridgeRepairQueue::default(),
            #[cfg(feature = "web")]
            repo_manager: None,
            #[cfg(feature = "web")]
            in_memory_merge: false,
            #[cfg(feature = "web")]
            github_mirror: None,
        }
    }

    /// Attach the Work Tracker store used by the web server to mirror
    /// user-created GitHub-compatible issues into Work items.
    #[must_use]
    pub fn with_work_store(mut self, work_store: WorkStore) -> Self {
        self.work_store = Some(work_store);
        self
    }

    /// Persist the pending Work-mirror repair queue in the sqlite file at
    /// `path` (the web server's `<data_dir>/shift.sqlite`), seeding it with the
    /// repairs already recorded there. Without this call the queue lives only
    /// in this router, so pending repairs are lost when the process ends.
    #[cfg(feature = "web")]
    pub fn with_work_bridge_repair_store(mut self, path: &std::path::Path) -> Result<Self, String> {
        self.work_bridge_repairs = WorkBridgeRepairQueue::with_store(path)?;
        Ok(self)
    }

    /// Attach a git [`RepoManager`](jeryu_gitd::RepoManager) so the PR merge
    /// endpoint advances the real base ref in the bare repo. A single additive,
    /// forward-compatible builder call: production wiring chains this onto
    /// [`GithubRouter::with_core`] in `web.rs`.
    #[cfg(feature = "web")]
    #[must_use]
    pub fn with_repo_manager(
        mut self,
        repo_manager: std::sync::Arc<jeryu_gitd::RepoManager>,
    ) -> Self {
        self.repo_manager = Some(repo_manager);
        self
    }

    /// Tests only: let the PR merge endpoint finalize with a synthesized merge
    /// sha when no [`RepoManager`](jeryu_gitd::RepoManager) is attached. A
    /// server built without this (production wiring in `web.rs`, which attaches
    /// a real manager instead) answers such a merge with a 503 rather than
    /// recording a sha that exists in no repository.
    #[cfg(feature = "web")]
    #[must_use]
    pub fn with_in_memory_merge(mut self) -> Self {
        self.in_memory_merge = true;
        self
    }

    /// Attach the merge-to-GitHub mirror so a PR merged into a configured
    /// repo's default branch pushes the live main tip to
    /// `github.com/<github_slug>` (outcome recorded as the
    /// `jeryu/github-mirror` check-run; never affects the merge response).
    /// Absent (the default, incl. every test that doesn't opt in) no push is
    /// ever attempted.
    #[cfg(feature = "web")]
    #[must_use]
    pub fn with_github_mirror(
        mut self,
        mirror: std::sync::Arc<crate::github_mirror::GithubMirror>,
    ) -> Self {
        self.github_mirror = Some(mirror);
        self
    }

    /// Borrows the GitHub mirror, if one is attached. The reconcile loop needs
    /// the same targets the merge push uses.
    #[cfg(feature = "web")]
    pub fn github_mirror(&self) -> Option<&std::sync::Arc<crate::github_mirror::GithubMirror>> {
        self.github_mirror.as_ref()
    }

    /// Borrows the backing forge store (used by tests and embedding callers).
    pub fn core(&self) -> &ForgeCore {
        &self.core
    }

    /// Every Work-mirror repair still waiting for a human, oldest first.
    pub fn work_bridge_repairs(&self) -> Vec<WorkBridgeRepair> {
        self.work_bridge_repairs.pending()
    }

    fn record_work_bridge_repair(&self, repair: WorkBridgeRepair) {
        self.work_bridge_repairs.record(repair);
    }

    /// Drops the repairs filed against an issue whose bridge write has just
    /// reached the Work store.
    fn resolve_work_bridge_repairs(&self, owner: &str, repo: &str, issue_number: u64) {
        self.work_bridge_repairs.resolve(owner, repo, issue_number);
    }

    /// Dispatches a request. `body` is the raw JSON request body (empty for
    /// bodiless GETs). The actor is the authenticated principal; the in-memory
    /// edge defaults it where GitHub would take it from the token.
    pub fn handle(&self, method: Method, path: &str, body: &str) -> Response {
        let path = normalize_github_path(path);
        // Split a `path?query` so callers (tests, the future HTTP edge) can pass
        // RFC5988 list pagination as `?per_page=&page=` without the query
        // leaking into segment matching.
        let (route_path, query) = path.split_once('?').unwrap_or((path, ""));
        let page = Pagination::from_query(query);
        let segments: Vec<&str> = route_path.trim_matches('/').split('/').collect();
        // Pagination links hang off the path plus the caller's own filters, so
        // following `next` keeps `?state=` and friends instead of resetting them.
        let link_base = link_base(route_path, query);
        self.route(
            method,
            &segments,
            body,
            RouteContext {
                route_path,
                path: &link_base,
                page,
                query,
            },
        )
        .unwrap_or_else(not_found)
    }

    /// Convenience GET wrapper.
    pub fn get(&self, path: &str) -> Response {
        self.handle(Method::Get, path, "")
    }

    /// Convenience POST wrapper.
    pub fn post(&self, path: &str, body: &str) -> Response {
        self.handle(Method::Post, path, body)
    }

    /// Convenience PUT wrapper.
    pub fn put(&self, path: &str, body: &str) -> Response {
        self.handle(Method::Put, path, body)
    }
}

/// The per-request context a route arm needs beyond its path segments: the
/// matched path, the base pagination links hang off (the path plus the caller's
/// surviving filters), the requested page and the raw query.
struct RouteContext<'a> {
    route_path: &'a str,
    path: &'a str,
    page: Pagination,
    query: &'a str,
}

impl GithubRouter {
    /// Routes a parsed request. Returns `Err(status)` for an unmatched route so
    /// the caller can render the GitHub-shaped fallback body.
    fn route(
        &self,
        method: Method,
        segments: &[&str],
        body: &str,
        context: RouteContext<'_>,
    ) -> std::result::Result<Response, u16> {
        let RouteContext {
            route_path,
            path,
            page,
            query,
        } = context;
        use Method::{Get, Patch, Post, Put};
        match (method, segments) {
            (Get, ["health"]) => Ok(json_response(
                200,
                &json!({ "status": "ok", "service": "jeryu-api" }),
            )),
            // Steering: first-contact doc for a confused agent on the REST edge.
            (Get, [".jeryu", "agents", "first-contact"]) => Ok(first_contact_response()),
            (
                _,
                ["login", "device", "code"]
                | ["login", "oauth", "access_token"]
                | ["login", "oauth", "authorize"],
            ) => Ok(gh_auth_workaround_response(route_path)),
            (Get, ["api", "v1", "version"]) => Ok(json_response(
                200,
                &json!({
                    "version": JERYU_API_VERSION,
                    "name": "jeryu-api",
                    "commit": JERYU_BUILD_COMMIT,
                    "webCommit": JERYU_WEB_COMMIT,
                }),
            )),
            (Get, ["user"]) => Ok(self.service_user()),
            (Post, ["graphql"]) => Ok(self.graphql(body)),

            // Repositories ---------------------------------------------------
            (Get, ["repos"]) => Ok(self.list_repos(path, page)),
            (Post, ["repos"]) => Ok(self.create_repo(body)),
            (Get, ["repos", owner, repo]) => Ok(self.get_repo(owner, repo)),
            (Patch, ["repos", owner, repo]) => Ok(self.update_repo(owner, repo, body)),
            (Post, ["repos", owner, repo, "transfer"]) => Ok(self.transfer_repo(owner, repo, body)),

            // Pull requests --------------------------------------------------
            (Get, ["repos", owner, repo, "pulls"]) => Ok(self.list_pulls(
                owner,
                repo,
                path,
                page,
                PullStateSelector::from_query(query),
            )),
            (Post, ["repos", owner, repo, "pulls"]) => Ok(self.create_pull(owner, repo, body)),
            (Get, ["repos", owner, repo, "pulls", number]) => {
                Ok(self.get_pull(owner, repo, number))
            }
            (Patch, ["repos", owner, repo, "pulls", number]) => {
                Ok(self.update_pull(owner, repo, number, body))
            }
            (Put, ["repos", owner, repo, "pulls", number, "merge"]) => {
                Ok(self.merge_pull(owner, repo, number, body))
            }

            // Issues ---------------------------------------------------------
            (Get, ["repos", owner, repo, "issues"]) => {
                Ok(self.list_issues(owner, repo, path, page))
            }
            (Post, ["repos", owner, repo, "issues"]) => Ok(self.create_issue(owner, repo, body)),
            (Get, ["repos", owner, repo, "issues", number]) => {
                Ok(self.get_issue(owner, repo, number))
            }
            (Patch, ["repos", owner, repo, "issues", number]) => {
                Ok(self.update_issue(owner, repo, number, body))
            }
            (Get, ["repos", owner, repo, "issues", number, "comments"]) => {
                Ok(self.list_comments(owner, repo, number))
            }
            (Post, ["repos", owner, repo, "issues", number, "comments"]) => {
                Ok(self.create_comment(owner, repo, number, body))
            }

            // Commit history -------------------------------------------------
            (Get, ["repos", owner, repo, "commits"]) => {
                Ok(self.list_commits(owner, repo, path, page, query))
            }

            // Commit status --------------------------------------------------
            (Get, ["repos", owner, repo, "commits", reference, "status"]) => {
                Ok(self.commit_status(owner, repo, reference))
            }
            (Post, ["repos", owner, repo, "statuses", sha]) => {
                Ok(self.create_status(owner, repo, sha, body))
            }

            // Check runs -----------------------------------------------------
            (Get, ["repos", owner, repo, "check-runs"]) => {
                Ok(self.list_check_runs(owner, repo, path, page))
            }
            (Get, ["repos", owner, repo, "commits", reference, "check-runs"]) => {
                Ok(self.list_check_runs_for_reference(owner, repo, reference, path, page))
            }
            (Post, ["repos", owner, repo, "check-runs"]) => {
                Ok(self.create_check_run(owner, repo, body))
            }

            // Branch protection ----------------------------------------------
            (Get, ["repos", owner, repo, "branches", branch, "protection"]) => {
                Ok(self.get_protection(owner, repo, branch))
            }
            (Put, ["repos", owner, repo, "branches", branch, "protection"]) => {
                Ok(self.set_protection(owner, repo, branch, body))
            }

            // Deployments ----------------------------------------------------
            (Get, ["repos", owner, repo, "deployments"]) => {
                Ok(self.list_deployments(owner, repo, path, page, query))
            }
            (Post, ["repos", owner, repo, "deployments"]) => {
                Ok(self.create_deployment(owner, repo, body))
            }
            (Get, ["repos", owner, repo, "deployments", id]) => {
                Ok(self.get_deployment(owner, repo, id))
            }
            (Get, ["repos", owner, repo, "deployments", id, "statuses"]) => {
                Ok(self.list_deployment_statuses(owner, repo, id, path, page))
            }
            (Post, ["repos", owner, repo, "deployments", id, "statuses"]) => {
                Ok(self.create_deployment_status(owner, repo, id, body))
            }
            (Get, ["repos", owner, repo, "environments"]) => {
                Ok(self.list_environments(owner, repo))
            }

            // Releases -------------------------------------------------------
            (Get, ["repos", owner, repo, "releases"]) => {
                Ok(self.list_releases(owner, repo, path, page))
            }
            (Post, ["repos", owner, repo, "releases"]) => Ok(self.create_release(owner, repo)),

            // Actions (sourced from check-runs as a CI proxy) ----------------
            (Get, ["repos", owner, repo, "actions", "runs"]) => {
                Ok(self.list_action_runs(owner, repo, path, page))
            }
            (Get, ["repos", owner, repo, "actions", "runs", id]) => {
                Ok(self.get_action_run(owner, repo, id))
            }
            (Get, ["repos", owner, repo, "actions", "runs", id, "jobs"]) => {
                Ok(self.list_action_run_jobs(owner, repo, id))
            }
            (Get, ["repos", owner, repo, "actions", "workflows"]) => {
                Ok(self.list_action_workflows(owner, repo, path, page))
            }
            (Get, ["repos", owner, repo, "actions", "workflows", workflow_id]) => {
                Ok(self.get_action_workflow(owner, repo, workflow_id))
            }
            (
                Get,
                [
                    "repos",
                    owner,
                    repo,
                    "actions",
                    "workflows",
                    workflow_id,
                    "runs",
                ],
            ) => Ok(self.list_action_workflow_runs(owner, repo, workflow_id, path, page)),
            (Post, ["repos", owner, repo, "actions", ..]) => {
                Ok(self.unsupported_action_write(owner, repo))
            }

            // Webhooks -------------------------------------------------------
            (Get, ["repos", owner, repo, "hooks"]) => Ok(self.list_hooks(owner, repo)),
            (Post, ["repos", owner, repo, "hooks"]) => Ok(self.create_hook(owner, repo, body)),

            _ => Err(404),
        }
    }
}

fn normalize_github_path(path: &str) -> &str {
    let Some(rest) = path.strip_prefix("/api/v3") else {
        return path;
    };
    if rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') {
        if rest.is_empty() || rest.starts_with('?') {
            "/"
        } else {
            rest
        }
    } else {
        path
    }
}
