//! General local-web helpers that are not repository-specific.

use axum::Json;
use axum::body::Bytes;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method as HttpMethod, StatusCode, header};
use axum::response::{Html, IntoResponse, Response as AxumResponse};
use jeryu_core::{AccountSummary, UserRole};
use jeryu_readmodel::contracts::{RenderedMarkdown, RepositorySummary, Viewer, WebBootstrap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use tokio::fs;

use super::markdown::render_markdown;
use super::permissions::{feature_flags, permissions};
#[cfg(test)]
use super::repositories::repo_summaries;
use super::repositories::repo_summaries_for_user;
use crate::{Method, Response as GithubResponse};

#[derive(Debug, Deserialize)]
pub(super) struct MarkdownRequest {
    #[serde(default)]
    markdown: String,
}

pub(super) async fn markdown_render(
    Json(request): Json<MarkdownRequest>,
) -> Json<RenderedMarkdown> {
    Json(render_markdown(&request.markdown))
}

pub(super) async fn graphql(
    State(state): State<std::sync::Arc<super::WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: Bytes,
) -> AxumResponse {
    let account = match github_account_from_headers(&state, peer.as_ref(), &headers) {
        Ok(account) => account,
        Err(response) => return *response,
    };
    let body = std::str::from_utf8(&body).unwrap_or_default();
    let body = bind_authenticated_actor(body, &account.login);
    github_response(state.github.handle(Method::Post, "/graphql", &body))
}

/// Accept-aware `/repos` entrypoint that serves the SPA shell to browser
/// navigations and the GitHub-compatible REST edge to API clients.
pub(super) async fn repo_entry(
    State(state): State<std::sync::Arc<super::WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    method: HttpMethod,
    headers: HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    body: Bytes,
) -> AxumResponse {
    if method == HttpMethod::GET {
        let path = uri.path();
        let browser_navigation = is_browser_navigation(&headers) || accepts_html(&headers);
        if (browser_navigation && is_browser_repo_route(path))
            || (is_repo_index(path) && !accepts_json(&headers))
        {
            return spa_shell_response(&state.spa_dir).await;
        }
    }
    github_forward_request(state, peer, method, headers, uri, body).await
}

pub(super) async fn spa_fallback(
    State(state): State<std::sync::Arc<super::WebState>>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> AxumResponse {
    if is_unrouted_api_path(uri.path()) {
        return unknown_api_route(uri.path());
    }
    spa_response(&state.spa_dir, uri.path()).await
}

/// No page of the web app lives under `/api/`, so a request that reaches
/// the fallback there named a route this server does not have.
fn is_unrouted_api_path(path: &str) -> bool {
    // Every API version: `/api/v3/nope` fell through to the web app as well.
    path == "/api" || path.starts_with("/api/")
}

/// A missing API route answers a JSON 404. Serving the web app's HTML shell
/// with a 200 here made a missing route look like success to every API client.
fn unknown_api_route(path: &str) -> AxumResponse {
    super::workcells_support::typed_error(super::workcells_support::TypedError {
        status: StatusCode::NOT_FOUND,
        code: "api_route_not_found",
        purpose: "route a jeryu API request",
        reason: &format!("no API route matches {path}"),
        common_fixes: &[
            "check the path and HTTP method against the API docs",
            "the server may be older than the client: compare /api/v1/bootstrap versions",
        ],
        docs_url: "docs/phase7-api.md",
        repair_hint: "list the available routes in docs/phase7-api.md and docs/pipeline-events.md",
        message: "API route not found",
    })
}

/// Forwards a GitHub-compatible REST request to the in-process [`GithubRouter`],
/// which routes by `(method, path)` and renders GitHub-shaped JSON. The original
/// request path is forwarded verbatim so the dispatcher's segment matching works
/// unchanged; an unsupported HTTP verb returns a GitHub-shaped `405`.
pub(super) async fn github_forward(
    State(state): State<std::sync::Arc<super::WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    method: HttpMethod,
    headers: HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    body: Bytes,
) -> AxumResponse {
    github_forward_request(state, peer, method, headers, uri, body).await
}

async fn github_forward_request(
    state: std::sync::Arc<super::WebState>,
    peer: Option<ConnectInfo<SocketAddr>>,
    method: HttpMethod,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> AxumResponse {
    let Some(method) = map_method(&method) else {
        return guided_github_edge_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "Method Not Allowed",
            "route unsupported GitHub-compatible REST method",
            "the Jeryu GitHub edge accepts GET, PATCH, POST, and PUT for the guided compatibility subset",
            uri.path(),
        );
    };
    let account = match github_account_from_headers(&state, peer.as_ref(), &headers) {
        Ok(account) => account,
        Err(response) => return *response,
    };
    let path_and_query = uri
        .path_and_query()
        .map_or_else(|| uri.path().to_string(), ToString::to_string);
    if let Some(response) = authorize_github_repo_request(&state, method, &path_and_query, &account)
    {
        return response;
    }
    if github_repo_list_path(&path_and_query) && method == Method::Get {
        return github_response(
            state
                .github
                .list_repos_for_account(&path_and_query, &account),
        );
    }
    if github_user_path(&path_and_query) && method == Method::Get {
        return github_response(GithubResponse {
            status: 200,
            body: json!({
                "login": account.login,
                "id": 1,
                "node_id": format!("U_{}", account.login),
                "type": "User",
                "name": account.login,
                "url": "/user",
            })
            .to_string(),
            headers: Vec::new(),
        });
    }
    let body = std::str::from_utf8(&body).unwrap_or_default();
    let body = bind_authenticated_actor(body, &account.login);
    let response = state.github.handle(method, &path_and_query, &body);
    let normalized = normalize_github_edge_path(&path_and_query);
    super::pipeline::emit::github_edge(
        &state,
        method != Method::Get,
        normalized
            .split_once('?')
            .map_or(normalized, |(path, _)| path),
        &account.login,
        &body,
        response.status,
        &response.body,
    );
    github_response(response)
}

/// Replaces any caller-supplied actor with the authenticated principal before
/// the compatibility router records authorship. Invalid or non-object JSON is
/// left untouched so the route's normal request validation reports the error.
fn bind_authenticated_actor(body: &str, login: &str) -> String {
    let Ok(Value::Object(mut object)) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    object.insert("actor".to_string(), Value::String(login.to_string()));
    Value::Object(object).to_string()
}

fn accepts_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| {
            accept
                .split(',')
                .map(|part| part.split(';').next().unwrap_or("").trim())
                .any(|media| {
                    media == "application/json"
                        || media == "application/vnd.github+json"
                        || media
                            .strip_prefix("application/")
                            .is_some_and(|suffix| suffix.ends_with("+json"))
                })
        })
}

fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"))
}

fn is_repo_index(path: &str) -> bool {
    matches!(path, "/repos" | "/repos/")
}

fn is_browser_navigation(headers: &HeaderMap) -> bool {
    let mode = headers
        .get("sec-fetch-mode")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if mode == "navigate" {
        return true;
    }
    headers
        .get("sec-fetch-dest")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|dest| dest.eq_ignore_ascii_case("document"))
}

fn is_browser_repo_route(path: &str) -> bool {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    match segments.as_slice() {
        ["repos", "family", _family] => true,
        ["repos", _host, _repo] => true,
        ["repos", _host, _repo, "code"] => true,
        ["repos", _host, _repo, "settings", ..] => true,
        ["repos", _host, _repo, "blob", ..] => true,
        ["repos", _host, _repo, "tree", ..] => true,
        ["repos", _host, _repo, "pulls"] => true,
        ["repos", _host, _repo, "pulls", number] => number.chars().all(|ch| ch.is_ascii_digit()),
        ["repos", _provider, _owner, _name] => true,
        ["repos", _provider, _owner, _name, "agents", ..] => true,
        ["repos", _provider, _owner, _name, "code"] => true,
        ["repos", _provider, _owner, _name, "settings", ..] => true,
        ["repos", _provider, _owner, _name, "blob", ..] => true,
        ["repos", _provider, _owner, _name, "tree", ..] => true,
        ["repos", _provider, _owner, _name, "pulls"] => true,
        ["repos", _provider, _owner, _name, "pulls", number] => {
            number.chars().all(|ch| ch.is_ascii_digit())
        }
        ["repos", _provider, _owner, _name, "issues", ..] => true,
        ["repos", _provider, _owner, _name, "work", ..] => true,
        _ => false,
    }
}

fn github_account_from_headers(
    state: &super::WebState,
    peer: Option<&ConnectInfo<SocketAddr>>,
    headers: &HeaderMap,
) -> Result<AccountSummary, Box<AxumResponse>> {
    if !state.auth_required || super::auth::local_dev_trusted(state, peer.map(|connect| connect.0))
    {
        return Ok(super::auth::trusted_local_account(state));
    }
    super::auth::authenticate_headers(state, headers)
        .map(|auth| auth.account)
        .ok_or_else(|| {
            Box::new(
                (
                    StatusCode::UNAUTHORIZED,
                    [(header::WWW_AUTHENTICATE, "Basic realm=\"Jeryu\"")],
                    Json(json!({
                        "message": "Requires authentication",
                        "documentation_url": "/docs/rest",
                    })),
                )
                    .into_response(),
            )
        })
}

fn authorize_github_repo_request(
    state: &super::WebState,
    method: Method,
    path_and_query: &str,
    account: &AccountSummary,
) -> Option<AxumResponse> {
    let normalized = normalize_github_edge_path(path_and_query);
    let route_path = normalized
        .split_once('?')
        .map_or(normalized, |(path, _)| path);
    let segments: Vec<&str> = route_path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    match (method, segments.as_slice()) {
        (Method::Post, ["repos"]) if account.role != UserRole::Admin => Some(github_forbidden(
            "repository creation requires admin access",
        )),
        (Method::Post, ["repos", _, _, "check-runs"])
        | (Method::Post, ["repos", _, _, "statuses", _])
            if !super::auth::can_publish_gate_statuses(account) =>
        {
            Some(github_forbidden(
                "CI evidence publication requires global-admin access or a JERYU_CI_PUBLISHERS identity",
            ))
        }
        (Method::Post, ["repos", _, _, "deployments"])
        | (Method::Post, ["repos", _, _, "deployments", _, "statuses"])
            if !super::auth::can_record_deployments(account) =>
        {
            // A deployment record is a claim about what an environment runs; the
            // release views and rollback decisions read it as fact.
            Some(github_forbidden(
                "recording deployments requires global-admin access and a JERYU_DEPLOYERS identity",
            ))
        }
        (Method::Put, ["repos", owner, repo, "branches", _, "protection"])
            if account.role != UserRole::Admin
                && !state.core.user_can_admin_repo(&account.login, owner, repo) =>
        {
            Some(github_forbidden(
                "branch-protection changes require repository-admin access",
            ))
        }
        // `PATCH /repos/{owner}/{repo}` archives and unarchives. Admin-only,
        // and deliberately stricter than the repository-write rule below:
        // archiving makes a repository read-only for everyone, so it is a
        // decision about the repository rather than a use of write access to
        // it. This mirrors the same call's authz on `PATCH /api/v1/repos/:id`.
        (Method::Patch, ["repos", _, _]) if account.role != UserRole::Admin => Some(
            github_forbidden("repository settings changes require admin access"),
        ),
        (_, ["repos", owner, repo, ..]) => {
            let allowed = match method {
                Method::Get => {
                    account.role == UserRole::Admin
                        || state.core.user_can_read_repo(&account.login, owner, repo)
                }
                Method::Patch | Method::Post | Method::Put => {
                    account.role == UserRole::Admin
                        || state.core.user_can_write_repo(&account.login, owner, repo)
                }
            };
            (!allowed).then(|| github_forbidden("repository access denied"))
        }
        _ => None,
    }
}

fn github_repo_list_path(path_and_query: &str) -> bool {
    let normalized = normalize_github_edge_path(path_and_query);
    let route_path = normalized
        .split_once('?')
        .map_or(normalized, |(path, _)| path);
    route_path.trim_matches('/') == "repos"
}

fn github_user_path(path_and_query: &str) -> bool {
    let normalized = normalize_github_edge_path(path_and_query);
    let route_path = normalized
        .split_once('?')
        .map_or(normalized, |(path, _)| path);
    route_path.trim_matches('/') == "user"
}

fn normalize_github_edge_path(path: &str) -> &str {
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

fn github_forbidden(message: &str) -> AxumResponse {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "message": message,
            "documentation_url": "/docs/rest",
        })),
    )
        .into_response()
}

async fn spa_shell_response(spa_dir: &Path) -> AxumResponse {
    spa_response(spa_dir, "/index.html").await
}

async fn spa_response(spa_dir: &Path, request_path: &str) -> AxumResponse {
    if let Some(relative_path) = clean_spa_path(request_path)
        && !relative_path.as_os_str().is_empty()
    {
        let disk_path = spa_dir.join(&relative_path);
        if disk_path.is_file()
            && let Ok(bytes) = fs::read(&disk_path).await
        {
            return (
                [(header::CONTENT_TYPE, content_type_for_path(&relative_path))],
                bytes,
            )
                .into_response();
        }

        let embedded_path = relative_path.to_string_lossy().replace('\\', "/");
        if let Some(asset) = super::embedded_web::get(&embedded_path) {
            return embedded_asset_response(asset);
        }
    }

    match fs::read_to_string(spa_dir.join("index.html")).await {
        Ok(html) => Html(html).into_response(),
        Err(_) => match super::embedded_web::index() {
            Some(asset) => embedded_asset_response(asset),
            None => (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                format!(
                    "failed to load SPA shell from {} and no embedded web assets were compiled",
                    spa_dir.display()
                ),
            )
                .into_response(),
        },
    }
}

fn embedded_asset_response(asset: &'static super::embedded_web::EmbeddedAsset) -> AxumResponse {
    (
        [(header::CONTENT_TYPE, asset.content_type)],
        asset.bytes.to_vec(),
    )
        .into_response()
}

fn clean_spa_path(request_path: &str) -> Option<PathBuf> {
    let trimmed = request_path.trim_start_matches('/');
    if trimmed.is_empty() {
        return Some(PathBuf::new());
    }

    let mut clean = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(part) => clean.push(part),
            _ => return None,
        }
    }
    Some(clean)
}

fn content_type_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|part| part.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("map") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
pub(super) fn bootstrap_payload(
    state: &super::WebState,
) -> Result<WebBootstrap, serde_json::Error> {
    let repos = repo_summaries(state);
    bootstrap_payload_with_repos(
        state,
        "local",
        Some("Local Operator".to_string()),
        None,
        repos,
    )
}

pub(super) fn bootstrap_payload_for_user(
    state: &super::WebState,
    account: &AccountSummary,
) -> Result<WebBootstrap, serde_json::Error> {
    let repos = repo_summaries_for_user(state, Some(account));
    bootstrap_payload_with_repos(state, &account.login, None, Some(account), repos)
}

fn bootstrap_payload_with_repos(
    state: &super::WebState,
    login: &str,
    display_name: Option<String>,
    account: Option<&AccountSummary>,
    repos: Vec<RepositorySummary>,
) -> Result<WebBootstrap, serde_json::Error> {
    let tui = serialize_payload(&super::workcells::live_tui(state))?;
    Ok(WebBootstrap {
        generated_at: state.tui.generated_at.to_rfc3339(),
        schema_version: "0.1.0-alpha".to_string(),
        viewer: Viewer {
            id: login.to_string(),
            login: login.to_string(),
            display_name,
            avatar_url: None,
            global_permissions: permissions(),
        },
        tui,
        recent_repositories: repos.into_iter().take(10).collect(),
        websocket_url: "/api/v1/ws".to_string(),
        feature_flags: feature_flags(state, account),
    })
}

pub(super) fn serialize_payload<T: Serialize>(value: &T) -> Result<Value, serde_json::Error> {
    serde_json::to_value(value)
}

/// Maps the HTTP verbs the GitHub edge supports to the dispatcher's [`Method`].
pub(super) fn map_method(method: &HttpMethod) -> Option<Method> {
    match *method {
        HttpMethod::GET => Some(Method::Get),
        HttpMethod::PATCH => Some(Method::Patch),
        HttpMethod::POST => Some(Method::Post),
        HttpMethod::PUT => Some(Method::Put),
        _ => None,
    }
}

fn guided_github_edge_response(
    status: StatusCode,
    message: &str,
    purpose: &str,
    reason: &str,
    path: &str,
) -> AxumResponse {
    (
        status,
        Json(json!({
            "message": message,
            "documentation_url": "/docs/rest",
            "jeryu_repair_hint": {
                "purpose": purpose,
                "reason": reason,
                "common_fixes": [
                    "retry with one of the listed GitHub-compatible REST routes",
                    "use /.jeryu/capabilities to choose a typed jeryu.* MCP tool",
                    "add a conformance test before widening the compatibility subset"
                ],
                "docs_url": "/docs/rest",
                "repair_hint": "prefer the listed Jeryu MCP/API alternatives, then rerun cargo test -p jeryu-api --features web"
            },
            "jeryu_mcp_tools": super::MCP_GUIDANCE_TOOLS,
            "jeryu_api_routes": [
                "GET /user",
                "GET /repos",
                "GET /repos/{owner}/{repo}",
                "GET /repos/{owner}/{repo}/pulls",
                "GET /repos/{owner}/{repo}/issues",
                "GET /repos/{owner}/{repo}/commits/{ref}/status",
                "GET /repos/{owner}/{repo}/commits/{ref}/check-runs",
                "POST /graphql"
            ],
            "path": path,
        })),
    )
        .into_response()
}

fn github_response(response: GithubResponse) -> AxumResponse {
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut axum_response = (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        response.body,
    )
        .into_response();
    let headers = axum_response.headers_mut();
    for (name, value) in response.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
    axum_response
}
