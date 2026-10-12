//! Account bot enrollment, roster, and token exchange.
//!
//! Token posts are open (no session) and rate-limited by client IP before the
//! core sees the secret. The enrollment key is returned once and never logged.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Extension, Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response as AxumResponse};
use jeryu_core::{
    AccountSummary, BotCall, BotEffect, BotEnrollment, BotKind, BotObservation, BotReach,
    BotSession, BotTaskKind, ForgeError,
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::auth::bot_token_rate_limit;
use super::bot_token::{self, BotPrincipal};
use super::repositories::find_repo;
use super::{WebState, api_error};

const TOKEN_TTL_SECS: u64 = 900;

pub(super) async fn enroll(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    headers: HeaderMap,
    body: Bytes,
) -> AxumResponse {
    let Some(fields) = object_fields(&body) else {
        return invalid_body();
    };
    let Some(slug) = text(&fields, "slug") else {
        return invalid_body();
    };
    let Some(display_name) = text(&fields, "display_name") else {
        return invalid_body();
    };
    // `kind` is a free-form label and grants nothing; omitted or null is
    // `agent`. Anything else must match `^[a-z][a-z0-9-]{0,31}$`.
    let kind = match fields.get("kind") {
        None | Some(Value::Null) => BotKind::default(),
        Some(Value::String(kind)) => match BotKind::parse(kind) {
            Ok(kind) => kind,
            Err(error) => return map_bot_error(error),
        },
        Some(_) => return invalid_kind(),
    };
    let Some(reach) = fields
        .get("reach")
        .and_then(|value| serde_json::from_value::<BotReach>(value.clone()).ok())
    else {
        return invalid_body();
    };
    match state
        .core
        .enroll_bot(&account.login, slug, display_name, kind, reach)
    {
        Ok(enrollment) => enrollment_response(&headers, &enrollment),
        Err(error) => map_bot_error(error),
    }
}

pub(super) async fn list(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> AxumResponse {
    match state.core.list_account_bots(&account.login) {
        Ok(bots) => Json(json!({ "bots": bots })).into_response(),
        Err(error) => map_bot_error(error),
    }
}

pub(super) async fn rotate(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    let Some(bot_id) = parse_bot_id(&id) else {
        return invalid_bot_id();
    };
    match state.core.rotate_bot_key(&account.login, bot_id) {
        Ok(enrollment) => enrollment_response(&headers, &enrollment),
        Err(error) => map_bot_error(error),
    }
}

pub(super) async fn suspend(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    let Some(bot_id) = parse_bot_id(&id) else {
        return invalid_bot_id();
    };
    match state.core.suspend_bot(&account.login, bot_id) {
        Ok(bot) => Json(bot).into_response(),
        Err(error) => map_bot_error(error),
    }
}

pub(super) async fn revoke(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    let Some(bot_id) = parse_bot_id(&id) else {
        return invalid_bot_id();
    };
    match state.core.revoke_bot(&account.login, bot_id) {
        Ok(bot) => Json(bot).into_response(),
        Err(error) => map_bot_error(error),
    }
}

/// `POST /api/v1/bots/:id/tokens` — issue a scoped agent token. The token is
/// in this response once and never again.
pub(super) async fn issue_token(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
    body: Bytes,
) -> AxumResponse {
    let Some(bot_id) = parse_bot_id(&id) else {
        return invalid_bot_id();
    };
    let fields = if body.is_empty() {
        Map::new()
    } else {
        let Some(fields) = object_fields(&body) else {
            return invalid_body();
        };
        fields
    };
    let label = match fields.get("label") {
        None | Some(Value::Null) => None,
        Some(Value::String(label)) => Some(label.as_str()),
        Some(_) => return invalid_body(),
    };
    let ttl_days = match fields.get("ttl_days") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64().and_then(|days| u32::try_from(days).ok()) {
            Some(days) => Some(days),
            None => return invalid_body(),
        },
    };
    match state
        .core
        .issue_bot_access_token(&account.login, bot_id, label, ttl_days)
    {
        Ok(issued) => (
            StatusCode::CREATED,
            Json(json!({
                "bot": issued.bot,
                "token_id": issued.token_id,
                "token": issued.token,
                "label": issued.label,
                "created_at": issued.created_at,
                "expires_at": issued.expires_at,
            })),
        )
            .into_response(),
        Err(error) => map_bot_error(error),
    }
}

/// `GET /api/v1/bots/:id/tokens` — the bot's agent tokens, never the tokens.
pub(super) async fn list_tokens(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    let Some(bot_id) = parse_bot_id(&id) else {
        return invalid_bot_id();
    };
    match state.core.list_bot_access_tokens(&account.login, bot_id) {
        Ok(tokens) => Json(json!({ "tokens": tokens })).into_response(),
        Err(error) => map_bot_error(error),
    }
}

/// `POST /api/v1/bots/:id/tokens/:token_id/revoke`
pub(super) async fn revoke_token(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((id, token_id)): AxumPath<(String, String)>,
) -> AxumResponse {
    let Some(bot_id) = parse_bot_id(&id) else {
        return invalid_bot_id();
    };
    match state
        .core
        .revoke_bot_access_token(&account.login, bot_id, &token_id)
    {
        Ok(summary) => Json(summary).into_response(),
        Err(error) => map_bot_error(error),
    }
}

pub(super) async fn roster(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return missing_repo();
    };
    match state
        .core
        .list_repository_bots(&account.login, &repo.owner, &repo.name)
    {
        Ok(bots) => Json(json!({ "bots": bots })).into_response(),
        Err(error) => map_bot_error(error),
    }
}

pub(super) async fn activity(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    let Some(repo) = find_repo(&state, &id) else {
        return missing_repo();
    };
    match state
        .core
        .list_repository_bot_activity(&account.login, &repo.owner, &repo.name)
    {
        Ok(events) => Json(json!({ "events": events })).into_response(),
        Err(error) => map_bot_error(error),
    }
}

pub(super) async fn exchange_token(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    peer: Option<ConnectInfo<SocketAddr>>,
    body: Bytes,
) -> AxumResponse {
    if let Some(response) = bot_token_rate_limit(&state, peer.as_ref(), &headers, "bot-token") {
        return response;
    }
    let Some(fields) = object_fields(&body) else {
        return invalid_body();
    };
    let Some(enrollment_key) = text(&fields, "enrollment_key") else {
        return invalid_body();
    };
    let Some(audience) = checked_audience(text(&fields, "aud")) else {
        return bad_audience();
    };
    match state.core.exchange_bot_enrollment_key(enrollment_key) {
        Ok(session) => token_response(&session, &audience),
        Err(error) => map_bot_error(error),
    }
}

pub(super) async fn refresh_token(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    peer: Option<ConnectInfo<SocketAddr>>,
    body: Bytes,
) -> AxumResponse {
    if let Some(response) = bot_token_rate_limit(&state, peer.as_ref(), &headers, "bot-refresh") {
        return response;
    }
    let Some(fields) = object_fields(&body) else {
        return invalid_body();
    };
    let Some(refresh) = text(&fields, "refresh_token") else {
        return invalid_body();
    };
    let Some(audience) = checked_audience(text(&fields, "aud")) else {
        return bad_audience();
    };
    match state.core.rotate_bot_refresh_token(refresh) {
        Ok(session) => token_response(&session, &audience),
        Err(error) => map_bot_error(error),
    }
}

pub(super) async fn whoami(
    State(state): State<Arc<WebState>>,
    Extension(bot): Extension<BotPrincipal>,
) -> AxumResponse {
    match state
        .core
        .authenticate_bot_access(bot.bot_id, bot.generation, &bot.key_id)
    {
        Ok(summary) => Json(json!({
            "kind": summary.kind.label(),
            "slug": summary.slug,
            "reach": summary.reach,
            "owner": summary.owner,
            "generation": summary.credential_generation,
        }))
        .into_response(),
        Err(_) => bot_token::invalid_credential(),
    }
}

pub(super) fn map_bot_error(error: ForgeError) -> AxumResponse {
    match error {
        ForgeError::Validation(message)
            if ForgeError::machine_text(&message) == "invalid bot credential" =>
        {
            bot_token::invalid_credential()
        }
        ForgeError::Validation(message) => {
            let code = [BotKind::INVALID_CODE, BotReach::INVALID_OWNERS_CODE]
                .into_iter()
                .find(|code| {
                    message
                        .strip_prefix(code)
                        .is_some_and(|rest| rest.starts_with(':'))
                })
                .unwrap_or("invalid_input");
            api_error(StatusCode::UNPROCESSABLE_ENTITY, code, &message)
        }
        ForgeError::Conflict(message)
            if ForgeError::machine_text(&message) == "idempotency_conflict" =>
        {
            api_error(
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "the Idempotency-Key was used with a different request",
            )
        }
        ForgeError::Conflict(message) => api_error(StatusCode::CONFLICT, "conflict", &message),
        ForgeError::Forbidden(message)
            if ForgeError::machine_text(&message) == "repo_not_granted" =>
        {
            api_error(
                StatusCode::FORBIDDEN,
                "repo_not_granted",
                "the bot has no grant for this repository",
            )
        }
        ForgeError::Forbidden(message)
            if ForgeError::machine_text(&message) == "insufficient_scope" =>
        {
            api_error(
                StatusCode::FORBIDDEN,
                "insufficient_scope",
                "the bot grant does not allow this write",
            )
        }
        ForgeError::Forbidden(message)
            if ForgeError::machine_text(&message) == "task_not_granted" =>
        {
            api_error(
                StatusCode::FORBIDDEN,
                "task_not_granted",
                "the bot grant does not allow this task",
            )
        }
        ForgeError::Forbidden(message) => {
            api_error(StatusCode::FORBIDDEN, "permission_denied", &message)
        }
        ForgeError::NotFound(message) => api_error(StatusCode::NOT_FOUND, "not_found", &message),
        ForgeError::RepositoryArchived(name) => api_error(
            StatusCode::CONFLICT,
            "repository_archived",
            &format!("repository {name} is archived"),
        ),
        other => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &other.to_string(),
        ),
    }
}

pub(super) fn needs_reconciliation() -> AxumResponse {
    api_error(
        StatusCode::CONFLICT,
        "needs_reconciliation",
        "a bot operation with this key is still in progress",
    )
}

pub(super) fn body_digest(body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body);
    hex::encode(hasher.finalize())
}

fn enrollment_response(headers: &HeaderMap, enrollment: &BotEnrollment) -> AxumResponse {
    let forge =
        bot_token::request_base_url(headers).unwrap_or_else(|| "http://127.0.0.1:8787".to_string());
    let setup_script = render_bot_setup_script(
        enrollment.bot.kind.as_str(),
        &enrollment.bot.slug,
        &forge,
        Some(&enrollment.enrollment_key),
    );
    Json(json!({
        "bot": enrollment.bot,
        "key_id": enrollment.key_id,
        "enrollment_key": enrollment.enrollment_key,
        "setup_script": setup_script,
    }))
    .into_response()
}

fn token_response(session: &BotSession, audience: &str) -> AxumResponse {
    let access_token = match bot_token::issue_access_token(session, audience) {
        Ok(token) => token,
        Err(_) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_failed",
                "bot signing key is unavailable",
            );
        }
    };
    Json(json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": TOKEN_TTL_SECS,
        "refresh_token": session.refresh_token,
        "refresh_expires_at": session.refresh_expires_at,
        "bot": session.bot,
    }))
    .into_response()
}

fn object_fields(body: &[u8]) -> Option<Map<String, Value>> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(fields)) => Some(fields),
        _ => None,
    }
}

fn text<'a>(fields: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    fields.get(key).and_then(Value::as_str)
}

fn checked_audience(audience: Option<&str>) -> Option<String> {
    bot_token::canonical_audience(audience?)
}

fn invalid_body() -> AxumResponse {
    api_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_input",
        "body must be a JSON object",
    )
}

fn bad_audience() -> AxumResponse {
    api_error(
        StatusCode::BAD_REQUEST,
        "invalid_input",
        "audience is not an acceptable forge origin",
    )
}

fn invalid_kind() -> AxumResponse {
    api_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        BotKind::INVALID_CODE,
        "bot kind must be a string matching ^[a-z][a-z0-9-]{0,31}$",
    )
}

fn invalid_bot_id() -> AxumResponse {
    api_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_input",
        "bot id must be a uuid",
    )
}

fn missing_repo() -> AxumResponse {
    api_error(StatusCode::NOT_FOUND, "not_found", "repository not found")
}

fn parse_bot_id(id: &str) -> Option<Uuid> {
    Uuid::parse_str(id).ok()
}

pub(crate) fn bot_call(
    bot: &BotPrincipal,
    owner: &str,
    repo: &str,
    write: bool,
    task_kind: Option<BotTaskKind>,
    task_id: Option<String>,
) -> BotCall {
    BotCall {
        bot_id: bot.bot_id,
        generation: bot.generation,
        key_id: bot.key_id.clone(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        effect: if write {
            BotEffect::Write
        } else {
            BotEffect::Read
        },
        task_kind,
        task_id,
    }
}

/// Task identity copied from the path being changed. A client field that
/// repeats the bound task is not consulted, and neither is the query string:
/// `?branch=` on an unrelated path would otherwise widen a branch task.
pub(crate) fn task_from_segments(segments: &[&str]) -> (Option<BotTaskKind>, Option<String>) {
    if segments.len() >= 2 {
        let id = segments[1];
        let numeric = !id.is_empty() && id.chars().all(|ch| ch.is_ascii_digit());
        if numeric && segments[0] == "issues" {
            return (Some(BotTaskKind::Issue), Some(id.to_string()));
        }
        if numeric && (segments[0] == "pulls" || segments[0] == "pull") {
            return (Some(BotTaskKind::Pull), Some(id.to_string()));
        }
        if (segments[0] == "branches" || segments[0] == "branch") && !id.is_empty() {
            return (Some(BotTaskKind::Branch), Some(id.to_string()));
        }
    }
    (None, None)
}

pub(super) fn task_from_api_path(path: &str) -> (Option<BotTaskKind>, Option<String>) {
    let Some(rest) = path.strip_prefix("/api/v1/repos/") else {
        return (None, None);
    };
    let segments: Vec<String> = rest
        .split('/')
        .map(super::auth::percent_decode)
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() < 2 {
        return (None, None);
    }
    let tail: Vec<&str> = segments[1..].iter().map(String::as_str).collect();
    task_from_segments(&tail)
}

/// Paths a bot must not use even when the owner could.
pub(crate) fn github_segment_blocked(segments: &[&str]) -> bool {
    const BLOCKED: &[&str] = &[
        "protection",
        "transfer",
        "deployments",
        "hooks",
        "keys",
        "rulesets",
        "secrets",
        "invitations",
        "autolinks",
        "check-runs",
        "statuses",
    ];
    segments.iter().any(|segment| BLOCKED.contains(segment))
}

pub(super) fn record_bot_http(
    state: &WebState,
    bot: &BotPrincipal,
    method: &str,
    path: &str,
    repo: Option<&str>,
    status: StatusCode,
) {
    let mut action = format!("{method} {path}");
    if action.contains("jbk_")
        || action.contains("jbr_")
        || action.contains("jat_")
        || action.contains("eyJ")
    {
        return;
    }
    action.truncate(120);
    let outcome = status.as_u16().to_string();
    let _ = state.core.note_bot_observation(
        bot.bot_id,
        BotObservation {
            key_id: bot.key_id.clone(),
            action,
            outcome,
            scope: "http".to_string(),
            repo: repo.map(str::to_string),
            session_id: None,
            success: status.is_success(),
            mutation: !matches!(method, "GET" | "HEAD"),
            heartbeat: false,
        },
    );
}

/// Self-contained setup script. `enrollment_key` is embedded only for the
/// enroll response; a re-run reads the 0600 file and does not print the key.
/// `kind` is the bot's kind. `muse` writes an env file and prints a skill
/// stub. Every other kind prints an `mcpServers` entry for the loopback proxy,
/// which any MCP client reads; `grok` also warns that its bots share the
/// computer. Callers validate the kind (`bot_kind_is_valid`).
///
/// The script writes gh config and the merge token under
/// `~/.jeryu/bots/<slug>/` and binds its own loopback port. It does not write
/// `~/.config/gh` or `~/.jeryu/secrets/merge-token`.
pub fn render_bot_setup_script(
    kind: &str,
    slug: &str,
    forge_url: &str,
    enrollment_key: Option<&str>,
) -> String {
    // Callers pass a validated kind; every value is shell-quoted below.
    let label = match kind {
        "grok" => "Grokbot",
        "muse" => "Musebot",
        other => other,
    };
    let kind_name = kind;
    let enrollment = match enrollment_key {
        Some(key) => shell_quote(key),
        None => "\"${BOT_ENROLLMENT_KEY:-}\"".to_string(),
    };
    TEMPLATE
        .replace("@@FORGE@@", &shell_quote(forge_url))
        .replace("@@SLUG@@", &shell_quote(slug))
        .replace("@@KIND@@", &shell_quote(kind_name))
        .replace("@@LABEL@@", &shell_quote(label))
        .replace("@@ENROLLMENT@@", &enrollment)
}

/// Whether `kind` is a valid bot kind: `^[a-z][a-z0-9-]{0,31}$`.
pub fn bot_kind_is_valid(kind: &str) -> bool {
    BotKind::is_valid(kind)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

const TEMPLATE: &str = include_str!("bot_setup_template.sh");
