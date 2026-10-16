//! Shared request parsing, principal resolution, and response helpers for the
//! GitHub-compatible edge.
//!
//! These helpers are used across every resource submodule so they live in one
//! place; the GitHub-shaped status codes (422 for unparseable/invalid bodies,
//! 405 for branch-protection blocks, 404 for misses) are asserted by the
//! `github_api` conformance tests and must stay byte-for-byte.

use jeryu_core::ForgeError;
use serde_json::{Value, json};

use crate::discovery::GH_AUTH_DOCS;
use crate::routes::Response;

/// Path prefix the GitHub-compatible edge is served under. It is stripped
/// before segment matching, so anything rendering a URL back to a caller
/// (`Link` headers) has to put it back.
pub(crate) const V3_PREFIX: &str = "/api/v3";

/// `html_url` for a web UI path. GitHub clients expect an absolute URL, so a
/// rooted path is prefixed with the configured public origin; without one
/// (local dev, tests) the path is returned unchanged.
pub(crate) fn web_url(path: &str) -> String {
    crate::links::absolute(path)
}

/// [`web_url`] against an explicit origin, for the callers that already hold
/// one (and for tests).
fn web_url_with_origin(origin: Option<&str>, path: &str) -> String {
    crate::links::absolute_with_origin(origin, path)
}

/// The MCP tools this edge's guidance may name.
///
/// The bodies here go out to whoever asked, including an anonymous caller, so
/// they name only tools this forge implements and that no authenticated
/// principal short of admin is refused outright
/// ([`crate::tool_access::unprivileged_tools`]). The caller's own usable set,
/// admin tools included, is in the capability manifest at
/// [`FASTER_PATH`], which answers for the credential that asks.
pub(crate) fn mcp_guidance_tools() -> Vec<String> {
    crate::tool_access::unprivileged_tools()
}

/// What the gh-setup guidance says to pass as the token file. The guidance goes
/// out to anonymous callers, so it asks for a path the caller already has
/// instead of naming one on the host: where a host keeps its own files is not
/// something a stranger needs to be told.
pub(crate) const GH_SETUP_TOKEN_FILE: &str = "<your-token-file>";

/// Host the gh-setup guidance names when no public origin is configured — the
/// developer's own forge, which is what an unconfigured build is.
const GH_SETUP_LOCAL_HOST: &str = "http://127.0.0.1:8787";

/// Repair form of [`gh_setup_command`] for a host entry that already exists.
/// It names no host of its own, so it reads the same wherever it is served.
pub(crate) const GH_SETUP_REPAIR_COMMAND: &str =
    "jeryu gh-setup --host <the-same-host> --token-file <your-token-file>";

/// The public origin this deployment is reached at, if it has one.
pub(crate) fn public_origin() -> Option<String> {
    crate::links::public_origin()
}

/// `jeryu gh-setup` for the host the caller reached. Production sets
/// [`crate::links::PUBLIC_ORIGIN_ENV`], and a loopback address is an instruction an internet
/// client cannot follow, so the public origin wins wherever one is configured.
pub(crate) fn gh_setup_command() -> String {
    gh_setup_command_with_origin(public_origin().as_deref())
}

pub(crate) fn gh_setup_command_with_origin(origin: Option<&str>) -> String {
    let host = match origin.map(|origin| origin.trim().trim_end_matches('/')) {
        Some(origin) if !origin.is_empty() => origin,
        _ => GH_SETUP_LOCAL_HOST,
    };
    format!("jeryu gh-setup --host {host} --token-file {GH_SETUP_TOKEN_FILE}")
}

pub(crate) const GH_AUTH_BOUNDARY: &str = "GitHub.com auth and local Jeryu host auth are separate; do not run gh auth login for Jeryu hosts.";

/// The fast-path pointer surfaced on every error body so a confused agent is
/// always handed the capability manifest instead of being left to guess. It
/// sits under the `/api/v1` prefix the edge routes: the manifest's original
/// `/.jeryu/capabilities` path is served too, but only the prefixed one is
/// advertised, because only it resolves through every deployment's edge.
pub(super) const FASTER_PATH: &str = crate::discovery::CAPABILITIES_PATH;

/// [`FASTER_PATH`] as a route a steering block can name as the next request.
pub(super) const CAPABILITIES_ROUTE: &str = "GET /api/v1/capabilities";

/// The REST document, as a route: what a caller reads to learn the fields a
/// route accepts.
pub(super) const REST_DOC_ROUTE: &str = "GET /api/v1/docs/rest";

/// Every route the GitHub-compatible edge serves, in the shape a caller types
/// it. The edge's index (`GET /api/v3`), its unmatched-route 404 and the REST
/// document all answer with this one list, so none of them can drift from
/// another — `v3_routes_all_dispatch` keeps it from drifting from the router.
pub const V3_ROUTES: &[&str] = &[
    "GET /user",
    "GET /repos",
    "POST /repos (name, optional owner)",
    "GET /repos/{owner}/{repo}",
    "PATCH /repos/{owner}/{repo} (archived: bool or name: string; admin only)",
    "POST /repos/{owner}/{repo}/transfer (new_owner, optional new_name; admin only)",
    "GET /repos/{owner}/{repo}/pulls?state=open|closed|all&sort=created|updated|popularity|long-running&direction=asc|desc&head=owner:branch&base=branch&per_page=1-100&page=N (default created/desc; an unaccepted value is a 422)",
    "POST /repos/{owner}/{repo}/pulls (title, head, base, optional body, draft)",
    "GET /repos/{owner}/{repo}/pulls/{number}",
    "PATCH /repos/{owner}/{repo}/pulls/{number} (draft: bool, title, body, state; author or admin. The named Jeryu routes are POST /api/v1/repos/{id}/pulls/{number}/ready and /draft)",
    "GET /repos/{owner}/{repo}/pulls/{number}/commits?per_page=1-100&page=N",
    "PUT /repos/{owner}/{repo}/pulls/{number}/merge (optional merge_method, sha)",
    "GET /repos/{owner}/{repo}/issues?state=open|closed|all&sort=created|updated|comments&direction=asc|desc&per_page=1-100&page=N (default created/desc; an unaccepted value is a 422)",
    "POST /repos/{owner}/{repo}/issues (title, optional body, labels)",
    "GET /repos/{owner}/{repo}/issues/{number}",
    "PATCH /repos/{owner}/{repo}/issues/{number} (title, body, state)",
    "GET /repos/{owner}/{repo}/issues/{number}/comments",
    "POST /repos/{owner}/{repo}/issues/{number}/comments (body)",
    "GET /repos/{owner}/{repo}/commits?sha=&direction=asc|desc&per_page=1-100&page=N (newest first by default; an unaccepted value is a 422)",
    "GET /repos/{owner}/{repo}/commits/{ref}/status",
    "POST /repos/{owner}/{repo}/statuses/{sha} (state, optional context, description, target_url)",
    "GET /repos/{owner}/{repo}/check-runs?per_page=1-100&page=N",
    "POST /repos/{owner}/{repo}/check-runs (name, head_sha, optional status, conclusion)",
    "GET /repos/{owner}/{repo}/commits/{ref}/check-runs?per_page=1-100&page=N",
    "GET /repos/{owner}/{repo}/branches/{branch}/protection",
    "PUT /repos/{owner}/{repo}/branches/{branch}/protection (required_status_checks, required_pull_request_reviews, ...; admin only)",
    "GET /repos/{owner}/{repo}/deployments?sha=&ref=&environment=&per_page=1-100&page=N",
    "POST /repos/{owner}/{repo}/deployments (ref, optional environment, description)",
    "GET /repos/{owner}/{repo}/deployments/{id}",
    "GET /repos/{owner}/{repo}/deployments/{id}/statuses?per_page=1-100&page=N",
    "POST /repos/{owner}/{repo}/deployments/{id}/statuses (state, optional description, environment_url)",
    "GET /repos/{owner}/{repo}/environments",
    "GET /repos/{owner}/{repo}/releases?per_page=1-100&page=N",
    "POST /repos/{owner}/{repo}/releases",
    "GET /repos/{owner}/{repo}/actions/runs?per_page=1-100&page=N",
    "GET /repos/{owner}/{repo}/actions/runs/{id}",
    "GET /repos/{owner}/{repo}/actions/runs/{id}/jobs",
    "GET /repos/{owner}/{repo}/actions/workflows?per_page=1-100&page=N",
    "GET /repos/{owner}/{repo}/actions/workflows/{workflow_id}",
    "GET /repos/{owner}/{repo}/actions/workflows/{workflow_id}/runs?per_page=1-100&page=N",
    "GET /repos/{owner}/{repo}/hooks",
    "POST /repos/{owner}/{repo}/hooks (config.url, optional events, active)",
    "POST /graphql",
    "GET /health",
    "GET /api/v1/version",
    "GET /.jeryu/agents/first-contact",
];

/// `GET /api/v3`: the edge's index, with the status an index answers with. It
/// used to render this body under a 404, which reads to every client (and
/// every agent) as "there is no such edge".
pub(super) fn v3_index_response() -> Response {
    json_response(
        200,
        &json!({
            "message": "Jeryu GitHub-compatible REST edge.",
            "documentation_url": docs_url(),
            "jeryu_api_routes": V3_ROUTES,
            "jeryu_connection": {
                "capabilities": FASTER_PATH,
                "first_contact": "/.jeryu/agents/first-contact",
                "mcp": "/mcp",
                "typed_api": crate::discovery::TYPED_INDEX_PATH,
                "openapi": crate::discovery::OPENAPI_PATH,
            },
            "jeryu_mcp_tools": mcp_guidance_tools(),
        }),
    )
}

/// RFC 5988 list pagination parsed off the request query string. GitHub's
/// defaults (`per_page=30`, `page=1`) and ceiling (`per_page<=100`) are
/// mirrored so `gh ... --paginate` and naive `?page=N` walks behave.
///
/// A value outside that range is refused with a 422 rather than clamped, the
/// one paging rule both edges follow (`docs/pagination.md`): `/api/v1` already
/// answered `invalid_page_parameter`, and a caller that asked for 500 rows and
/// silently got 100 cannot tell a short page from the end of the collection.
#[derive(Clone, Copy, Debug)]
pub(super) struct Pagination {
    pub(super) per_page: usize,
    pub(super) page: usize,
}

impl Pagination {
    pub(super) const DEFAULT_PER_PAGE: usize = 30;
    pub(super) const MAX_PER_PAGE: usize = 100;

    /// Parses `?per_page=&page=` from a raw query string (the part after `?`).
    pub(super) fn from_query(query: &str) -> std::result::Result<Self, Response> {
        let mut per_page = Self::DEFAULT_PER_PAGE;
        let mut page = 1usize;
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "per_page" => per_page = bounded(key, value, Self::MAX_PER_PAGE)?,
                "page" => page = bounded(key, value, usize::MAX)?,
                _ => {}
            }
        }
        Ok(Self { per_page, page })
    }
}

/// A paging parameter read as a whole number from 1 to `max`, or the 422.
fn bounded(field: &str, value: &str, max: usize) -> std::result::Result<usize, Response> {
    match value.trim().parse::<usize>() {
        Ok(parsed) if (1..=max).contains(&parsed) => Ok(parsed),
        _ => Err(invalid_pagination(field, value, max)),
    }
}

/// GitHub's 422 for an out-of-range `?per_page=`/`?page=`. Shaped like the
/// list-query 422 in `listing.rs` so one reader handles every refused list
/// parameter, and it says the value is not clamped so a caller does not retry
/// the same number expecting a short page.
fn invalid_pagination(field: &str, value: &str, max: usize) -> Response {
    let ceiling = if max == usize::MAX {
        "1 and up".to_owned()
    } else {
        format!("1 to {max}")
    };
    json_response(
        422,
        &json!({
            "message": "Validation Failed",
            "errors": [{
                "resource": "Pagination",
                "field": field,
                "code": "invalid",
                "value": value,
                "message": format!(
                    "{field} must be a whole number from {ceiling}, got {value:?}; it is not clamped"
                ),
            }],
            "documentation_url": docs_url(),
            "jeryu_steering": steering(
                None,
                None,
                &format!("?{field}= takes a whole number from {ceiling}; retry the same route with one of those or drop the parameter"),
            ),
        }),
    )
}

/// Slices `items` to the requested page and renders a GitHub-shaped 200 list
/// response carrying an RFC 5988 `Link` header (`next`/`last`/`prev`/`first`)
/// when more than one page exists. `render` shapes the JSON body from the page
/// slice (a bare array for most lists; a wrapped object for check-runs/actions).
pub(super) fn paginate<T, F>(
    base_path: &str,
    page_args: Pagination,
    items: &[T],
    render: F,
) -> Response
where
    F: FnOnce(&[T], usize) -> Value,
{
    let total = items.len();
    let per_page = page_args.per_page;
    let last_page = total.div_ceil(per_page).max(1);
    let page = page_args.page;
    let start = page.saturating_sub(1).saturating_mul(per_page);
    let slice: &[T] = if start >= total {
        &[]
    } else {
        let end = (start + per_page).min(total);
        &items[start..end]
    };
    let body = render(slice, total);
    let mut headers = Vec::new();
    if let Some(link) = link_header(base_path, per_page, page, last_page) {
        headers.push(("Link".to_owned(), link));
    }
    Response {
        status: 200,
        body: body.to_string(),
        headers,
    }
}

/// Builds the base URL that pagination links hang off: the `/api/v3` edge URL
/// of the route plus the caller's own query with `per_page`/`page` dropped. The
/// surviving pairs keep their original order and encoding, so a `next` hop
/// still carries filters such as `?state=closed` instead of silently falling
/// back to the defaults.
///
/// `route_path` is the path with the `/api/v3` prefix already stripped for
/// segment matching, so the prefix goes back on here and the public origin in
/// front of it: a `Link` URL a client can follow verbatim, like `html_url`.
pub(super) fn link_base(route_path: &str, query: &str) -> String {
    link_base_with_origin(crate::links::public_origin().as_deref(), route_path, query)
}

fn link_base_with_origin(origin: Option<&str>, route_path: &str, query: &str) -> String {
    let edge = web_url_with_origin(
        origin,
        &format!("{V3_PREFIX}{}", route_path.trim_end_matches('/')),
    );
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let key = pair.split_once('=').map_or(*pair, |(key, _)| key);
            key != "per_page" && key != "page"
        })
        .collect();
    if kept.is_empty() {
        edge
    } else {
        format!("{edge}?{}", kept.join("&"))
    }
}

/// Builds the RFC 5988 `Link` header value. Emits `next`/`last` while more
/// pages remain and `prev`/`first` once past page 1; returns `None` for a
/// single-page result (GitHub omits the header entirely in that case).
pub(super) fn link_header(
    base_path: &str,
    per_page: usize,
    page: usize,
    last_page: usize,
) -> Option<String> {
    if last_page <= 1 {
        return None;
    }
    // A base that already carries a query (`?sha=`) keeps it on every link.
    let sep = if base_path.contains('?') { '&' } else { '?' };
    let url = |target: usize| format!("{base_path}{sep}per_page={per_page}&page={target}");
    let mut parts: Vec<String> = Vec::new();
    if page < last_page {
        parts.push(format!("<{}>; rel=\"next\"", url(page + 1)));
        parts.push(format!("<{}>; rel=\"last\"", url(last_page)));
    }
    if page > 1 {
        // A `?page=` past the end still answers (an empty page), and its `prev`
        // must land on a page that holds rows rather than another empty one.
        parts.push(format!(
            "<{}>; rel=\"prev\"",
            url((page - 1).min(last_page))
        ));
        parts.push(format!("<{}>; rel=\"first\"", url(1)));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// The jeryu steering block stamped onto every error body: a faster path, the
/// supported route that serves the intent, and a one-line hint. Errors teach
/// instead of dead-ending the agent.
///
/// `mcp_tool` is an MCP tool only when one this forge implements serves the
/// intent and no grant beyond the caller's is needed for it; otherwise it is
/// `null` and `supported_route` names the REST route to use instead. A tool
/// the `/mcp` endpoint would answer `tool_unavailable` for is worse than no
/// recommendation at all, so the field is never filled with one.
pub(super) fn steering(mcp_tool: Option<&str>, supported_route: Option<&str>, hint: &str) -> Value {
    debug_assert!(
        mcp_tool.is_none_or(|tool| mcp_guidance_tools().iter().any(|known| known == tool)),
        "steering names {mcp_tool:?}, which this forge does not serve to every caller"
    );
    json!({
        "faster_path": FASTER_PATH,
        "mcp_tool": mcp_tool,
        "supported_route": supported_route,
        "hint": hint,
    })
}

/// Hosted GitHub Actions writes have no Jeryu equivalent to steer at: the
/// forge gates a pull request head rather than dispatching workflows, so the
/// steering names the re-gate route and the Actions reads that are served.
pub(super) fn actions_write_steering() -> Value {
    steering(
        None,
        Some(ACTIONS_REGATE_ROUTE),
        "hosted Actions dispatch, rerun and cancel are not served; ask for the pull request's \
         head to be gated again with the re-gate route, and read existing runs with GET \
         /repos/{owner}/{repo}/actions/runs",
    )
}

/// The typed Jeryu route that asks for a pull request's current head to be
/// gated again — the forge's answer to `gh run rerun`.
pub(crate) const ACTIONS_REGATE_ROUTE: &str =
    "POST /api/v1/repos/{owner}/{repo}/pulls/{number}/regate";

pub(super) fn parse_body<T: serde::de::DeserializeOwned>(
    body: &str,
) -> std::result::Result<T, Response> {
    serde_json::from_str(body).map_err(|err| {
        // GitHub returns 422 Unprocessable Entity for a body it cannot parse
        // or that fails validation.
        json_response(
            422,
            &json!({
                "message": "Validation Failed",
                "errors": [{ "code": "invalid", "detail": err.to_string() }],
                "documentation_url": docs_url(),
                "jeryu_steering": steering(
                    None,
                    Some(REST_DOC_ROUTE),
                    "the JSON body did not validate; the REST document lists the fields each route accepts",
                ),
            }),
        )
    })
}

pub(super) fn parse_number(raw: &str) -> std::result::Result<u64, Response> {
    raw.parse::<u64>().map_err(|_| {
        json_response(
            422,
            &json!({
                "message": "Validation Failed",
                "errors": [{ "field": "number", "code": "invalid" }],
                "documentation_url": docs_url(),
                "jeryu_steering": steering(
                    None,
                    None,
                    "the path expected a numeric id; list the resource first to get a valid number",
                ),
            }),
        )
    })
}

/// Resolves the acting principal from the request body's optional `actor`
/// field, defaulting to the canonical service principal. The future HTTP edge
/// will replace this with the authenticated token's owner.
pub(super) fn actor(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("actor")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "jeryu".to_owned())
}

/// Resolves the owner for repo creation from the request body's optional
/// `owner` field.
pub(super) fn owner_for_create(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body).ok().and_then(|value| {
        value
            .get("owner")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

/// Renders an `owner`/`user` actor block shared by every GitHub-shaped entity.
pub(super) fn owner_json(login: &str) -> Value {
    json!({
        "login": login,
        "type": "User",
        "url": format!("/users/{login}"),
    })
}

pub(super) fn json_response(status: u16, value: &Value) -> Response {
    Response {
        status,
        body: value.to_string(),
        headers: Vec::new(),
    }
}

pub(super) fn error_response(err: ForgeError) -> Response {
    let (status, route, hint) = match err {
        ForgeError::NotFound(_) => (
            404,
            Some("GET /repos/{owner}/{repo}"),
            "the resource was not found; list the owner's repositories or the collection to find a live name or number",
        ),
        ForgeError::Conflict(_) => (
            422,
            None,
            "the request conflicts with current state; re-read the resource and build the request from what it says now",
        ),
        ForgeError::Validation(_) => (
            422,
            Some(REST_DOC_ROUTE),
            "the request failed validation; the REST document lists the fields each route accepts",
        ),
        // 405 matches GitHub's "not mergeable / protected branch" answer and the
        // explicit 405s in `pulls.rs`. The web admin routes (`web/auth.rs`) map
        // the same variant to 403 on purpose: core reuses it there to mean
        // "caller lacks repo admin", which is an authorization failure.
        ForgeError::BranchProtection(_) => (
            405,
            Some("GET /repos/{owner}/{repo}/branches/{branch}/protection"),
            "branch protection blocks this; read the protection rule for the required checks and reviews",
        ),
        // Core refuses who may do this (an author approving their own pull
        // request, a dismissal by someone without the right): GitHub's 403.
        ForgeError::Forbidden(_) => (
            403,
            None,
            "this identity may not do that here; a different reviewer or an admin has to act",
        ),
        ForgeError::RepositoryArchived(_) => (
            409,
            Some("PATCH /repos/{owner}/{repo} (archived: false; admin only)"),
            "the repository is archived and read-only; unarchive it before writing",
        ),
        ForgeError::WaitlistFull => (
            503,
            None,
            "the waitlist is full and not accepting new addresses",
        ),
        ForgeError::Storage(_) | ForgeError::StoreCustody(_) => (
            500,
            None,
            "an internal store error occurred; retry, and tell an operator if it persists",
        ),
    };
    json_response(
        status,
        &json!({
            "message": err.to_string(),
            "documentation_url": docs_url(),
            "jeryu_steering": steering(None, route, hint),
        }),
    )
}

pub(super) fn actions_write_response(owner: &str, repo: &str) -> Response {
    json_response(
        501,
        &json!({
            "message": "Hosted GitHub Actions writes are not supported on Jeryu; the forge gates a pull request head instead of dispatching workflows.",
            "documentation_url": docs_url(),
            "jeryu_repair_hint": {
                "purpose": "route unsupported GitHub Actions write request",
                "reason": "Jeryu gates pull request heads from its own CI and serves Actions reads for compatibility; hosted Actions dispatch, rerun and cancel writes have no equivalent here.",
                "common_fixes": [
                    format!("to run the gate again on a pull request's current head, {ACTIONS_REGATE_ROUTE}"),
                    "use GET /repos/{owner}/{repo}/actions/runs, GET /repos/{owner}/{repo}/actions/runs/{id}/jobs, or GET /repos/{owner}/{repo}/actions/workflows/{workflow_id} to inspect existing runs",
                    format!("GET {FASTER_PATH} for the routes and MCP tools your own credential may use")
                ],
                "docs_url": docs_url(),
                "repair_hint": format!("ask for the head to be gated again with {ACTIONS_REGATE_ROUTE}, then read the result with GET /repos/{owner}/{repo}/actions/runs"),
            },
            "jeryu_connection": {
                "capabilities": FASTER_PATH,
                "first_contact": "/.jeryu/agents/first-contact",
                "mcp": "/mcp",
                "actions_runs": format!("GET /repos/{owner}/{repo}/actions/runs"),
                "actions_run": format!("GET /repos/{owner}/{repo}/actions/runs/{{id}}"),
                "actions_run_jobs": format!("GET /repos/{owner}/{repo}/actions/runs/{{id}}/jobs"),
                "actions_workflows": format!("GET /repos/{owner}/{repo}/actions/workflows"),
                "actions_workflow": format!("GET /repos/{owner}/{repo}/actions/workflows/{{workflow_id}}"),
                "actions_workflow_runs": format!("GET /repos/{owner}/{repo}/actions/workflows/{{workflow_id}}/runs"),
            },
            "jeryu_mcp_tools": mcp_guidance_tools(),
            "jeryu_steering": actions_write_steering(),
        }),
    )
}

pub(super) fn gh_auth_workaround_response(path: &str) -> Response {
    json_response(
        501,
        &json!({
            "message": "Direct GitHub CLI auth setup is not supported for a Jeryu host.",
            "documentation_url": GH_AUTH_DOCS,
            "jeryu_repair_hint": {
                "purpose": "route GitHub CLI auth setup through Jeryu",
                "reason": "Jeryu uses an explicit gh hosts.yml entry plus portable agent-auth receipts; running gh auth login, refresh, or token-hunting workarounds against the Jeryu host is the wrong repair path. GitHub.com auth and local Jeryu host auth are separate.",
                "common_fixes": [
                    "stop running gh auth login, gh auth refresh, or credential-store searches for the Jeryu host",
                    gh_setup_command(),
                    format!("for a stale gh host token entry, rerun {GH_SETUP_REPAIR_COMMAND}"),
                    GH_AUTH_BOUNDARY,
                    format!("GET {FASTER_PATH} for the routes and MCP tools your own credential may use"),
                    "for agent native CLI credentials, run jeryu agent auth doctor <tool> and jeryu agent auth import --from-host <tool>"
                ],
                "docs_url": GH_AUTH_DOCS,
                "repair_hint": format!("rerun jeryu gh-setup for the Jeryu host with --token-file {GH_SETUP_TOKEN_FILE}, then retry the original Jeryu CLI/MCP/API operation instead of gh auth")
            },
            "jeryu_connection": {
                "capabilities": FASTER_PATH,
                "first_contact": "/.jeryu/agents/first-contact",
                "mcp": "/mcp",
                "gh_setup": gh_setup_command(),
                "gh_token_file": GH_SETUP_TOKEN_FILE,
                "agent_auth_doctor": "jeryu agent auth doctor <codex|claude|jekko>",
                "agent_auth_import": "jeryu agent auth import --from-host <codex|claude|jekko>"
            },
            "jeryu_mcp_tools": mcp_guidance_tools(),
            "jeryu_steering": steering(
                None,
                Some(CAPABILITIES_ROUTE),
                "stop the gh auth flow; configure gh with jeryu gh-setup and use the Jeryu REST or MCP route for the original task",
            ),
            "path": path,
        }),
    )
}

pub(super) fn not_found(status: u16) -> Response {
    json_response(
        status,
        &json!({
            "message": "Not Found",
            "documentation_url": docs_url(),
            "jeryu_repair_hint": {
                "purpose": "route unsupported GitHub-compatible REST request",
                "reason": "the request path is outside the current guided Jeryu GitHub subset",
                "common_fixes": [
                    "retry with one of the listed GitHub-compatible REST routes",
                    "use the typed Jeryu /api/v1 route for repository, PR, issue, check, release, or hook workflows",
                    "add a conformance test before widening the compatibility subset"
                ],
                "docs_url": docs_url(),
                "repair_hint": "map the command to the closest listed Jeryu route, then rerun cargo test -p jeryu-api --features web"
            },
            "jeryu_mcp_tools": mcp_guidance_tools(),
            "jeryu_steering": steering(
                None,
                Some("GET /api/v3"),
                "this path is outside the guided subset; GET /api/v3 lists every route it serves",
            ),
            "jeryu_api_routes": V3_ROUTES
        }),
    )
}

pub(super) fn docs_url() -> String {
    crate::discovery::REST_DOC_PATH.to_owned()
}

/// First-contact doc for a confused agent that landed on the REST edge. Points
/// it at the capability manifest and the typed MCP tools instead of letting it
/// brute-force bespoke `gh` invocations.
pub(super) fn first_contact_response() -> Response {
    json_response(
        200,
        &json!({
            "message": "Welcome — you are talking to the Jeryu GitHub-compatible edge.",
            "start_here": FASTER_PATH,
            "advice": [
                "GET /api/v1/capabilities for the live endpoint + gh-command map.",
                "Prefer the typed jeryu.* MCP tools over bespoke gh REST calls where one serves your intent; the manifest says which ones your credential may use.",
                format!("Do not run gh auth login or gh auth refresh for a Jeryu host; run {} for the host entry.", gh_setup_command()),
                format!("If gh reports a stale host token entry, rerun {GH_SETUP_REPAIR_COMMAND}."),
                GH_AUTH_BOUNDARY,
                "Every error reply carries a jeryu_steering block naming the supported route, and the MCP tool when one serves your intent.",
            ],
            "gh_auth_policy": {
                "do_not_run": ["gh auth login", "gh auth refresh", "credential-store token hunting"],
                "run_instead": gh_setup_command(),
                "token_file": GH_SETUP_TOKEN_FILE,
                "stale_host_repair": GH_SETUP_REPAIR_COMMAND,
                "host_auth_boundary": GH_AUTH_BOUNDARY,
                "agent_auth": "jeryu agent auth doctor <tool>; jeryu agent auth import --from-host <tool>",
            },
            "mcp_tools": mcp_guidance_tools(),
            "repo_scoped_tools": crate::tool_access::REPO_SCOPE_NOTE,
            "documentation_url": docs_url(),
        }),
    )
}

#[cfg(test)]
mod gh_setup_tests {
    use super::{
        GH_SETUP_REPAIR_COMMAND, GH_SETUP_TOKEN_FILE, first_contact_response,
        gh_auth_workaround_response, gh_setup_command_with_origin,
    };

    #[test]
    fn the_setup_command_names_the_public_origin_where_one_is_configured() {
        assert_eq!(
            gh_setup_command_with_origin(Some("https://forge.example-invented.test/")),
            "jeryu gh-setup --host https://forge.example-invented.test --token-file <your-token-file>"
        );
    }

    /// Without a configured origin the build is somebody's own forge, so the
    /// guidance keeps naming the local address they reach it at.
    #[test]
    fn the_setup_command_falls_back_to_the_local_host() {
        for origin in [None, Some(""), Some("   ")] {
            assert_eq!(
                gh_setup_command_with_origin(origin),
                "jeryu gh-setup --host http://127.0.0.1:8787 --token-file <your-token-file>"
            );
        }
    }

    /// The guidance asks for the caller's own token file everywhere it is
    /// served; none of it names a path on the host.
    #[test]
    fn the_guidance_asks_for_the_callers_own_token_file() {
        assert!(GH_SETUP_REPAIR_COMMAND.contains(GH_SETUP_TOKEN_FILE));
        for body in [
            gh_auth_workaround_response("/login/device/code").body,
            first_contact_response().body,
        ] {
            assert!(
                !body.contains(".jeryu/secrets"),
                "a guided reply names a path on the host: {body}"
            );
        }
    }
}

#[cfg(test)]
mod web_url_tests {
    use super::{link_base_with_origin, web_url_with_origin};

    #[test]
    fn prefixes_rooted_paths_with_the_public_origin() {
        assert_eq!(
            web_url_with_origin(
                Some("https://git.neverhuman.org/"),
                "/repos/jeryu/jeryu/jeryu-web/pulls/15"
            ),
            "https://git.neverhuman.org/repos/jeryu/jeryu/jeryu-web/pulls/15"
        );
    }

    #[test]
    fn leaves_paths_alone_without_an_origin_or_when_already_absolute() {
        assert_eq!(
            web_url_with_origin(None, "/repos/jeryu/a/b"),
            "/repos/jeryu/a/b"
        );
        assert_eq!(
            web_url_with_origin(Some("  "), "/repos/jeryu/a/b"),
            "/repos/jeryu/a/b"
        );
        assert_eq!(
            web_url_with_origin(Some("https://x.example"), "https://y.example/p"),
            "https://y.example/p"
        );
    }

    /// A `Link` base is the edge URL a client can follow as it stands: the
    /// `/api/v3` prefix on, the public origin in front of it where one is
    /// configured, and the caller's own filters kept with `per_page`/`page`
    /// dropped so the relation can put its own back.
    #[test]
    fn a_link_base_is_the_prefixed_edge_url_with_the_callers_filters() {
        assert_eq!(
            link_base_with_origin(
                None,
                "/repos/acme/widgets/pulls",
                "state=all&per_page=1&page=2"
            ),
            "/api/v3/repos/acme/widgets/pulls?state=all"
        );
        assert_eq!(
            link_base_with_origin(
                Some("https://forge.example/"),
                "/repos/acme/widgets/pulls",
                ""
            ),
            "https://forge.example/api/v3/repos/acme/widgets/pulls"
        );
        assert_eq!(
            link_base_with_origin(Some("https://forge.example"), "/repos", "page=3"),
            "https://forge.example/api/v3/repos"
        );
    }
}

#[cfg(test)]
mod error_response_tests {
    use super::{error_response, mcp_guidance_tools, steering};
    use jeryu_core::ForgeError;
    use serde_json::Value;

    #[test]
    fn maps_every_forge_error_to_its_pinned_status() {
        let table: [(ForgeError, u16, Option<&str>); 6] = [
            (
                ForgeError::NotFound("x".into()),
                404,
                Some("GET /repos/{owner}/{repo}"),
            ),
            (ForgeError::Conflict("x".into()), 422, None),
            (
                ForgeError::Validation("x".into()),
                422,
                Some("GET /api/v1/docs/rest"),
            ),
            (
                ForgeError::BranchProtection("x".into()),
                405,
                Some("GET /repos/{owner}/{repo}/branches/{branch}/protection"),
            ),
            (
                ForgeError::RepositoryArchived("x".into()),
                409,
                Some("PATCH /repos/{owner}/{repo} (archived: false; admin only)"),
            ),
            (ForgeError::Storage("x".into()), 500, None),
        ];
        for (err, status, route) in table {
            let message = err.to_string();
            let response = error_response(err);
            assert_eq!(response.status, status, "status for {message}");
            let body: Value = serde_json::from_str(&response.body).expect("json body");
            assert_eq!(body["message"], message);
            assert!(body["documentation_url"].is_string());
            // Every one of these is served over REST: there is no MCP tool for
            // them, and naming one the forge does not dispatch would dead-end
            // the agent that followed it.
            assert_eq!(
                body["jeryu_steering"]["mcp_tool"],
                Value::Null,
                "steering for {message} names an MCP tool"
            );
            assert_eq!(
                body["jeryu_steering"]["supported_route"],
                route.map_or(Value::Null, |route| Value::String(route.to_string())),
                "supported route for {message}"
            );
            assert!(
                body["jeryu_steering"]["hint"]
                    .as_str()
                    .is_some_and(|hint| !hint.is_empty())
            );
        }
    }

    /// The routes the steering names are the paths discovery publishes: one
    /// spelling of each, so a caller is never sent somewhere nothing answers.
    #[test]
    fn the_named_routes_are_the_paths_discovery_publishes() {
        assert_eq!(
            super::CAPABILITIES_ROUTE,
            format!("GET {}", crate::discovery::CAPABILITIES_PATH)
        );
        assert_eq!(
            super::REST_DOC_ROUTE,
            format!("GET {}", crate::discovery::REST_DOC_PATH)
        );
    }

    /// Guidance never names a tool this forge does not dispatch, and the
    /// steering block only ever names one of the tools it may offer whoever
    /// asks.
    #[test]
    fn guidance_names_only_tools_this_forge_serves_to_anyone() {
        let offered = mcp_guidance_tools();
        assert!(!offered.is_empty());
        for tool in &offered {
            assert!(crate::tool_access::is_implemented(tool), "{tool}");
            assert_ne!(
                crate::tool_access::required_access(tool),
                Some(crate::tool_access::ToolAccess::Admin),
                "{tool} is admin-only"
            );
        }
        let block = steering(Some(&offered[0]), Some("GET /api/v3"), "hint");
        assert_eq!(block["mcp_tool"], offered[0]);
        assert_eq!(block["supported_route"], "GET /api/v3");
    }
}
