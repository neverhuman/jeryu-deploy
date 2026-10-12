//! Web account/session endpoints and auth middleware.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, Extension, Path as AxumPath, Query, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{DateTime, Duration, Utc};
use jeryu_core::{
    AccountStatus, AccountSummary, ForgeError, PersonalAccessTokenSummary, RepoAccessGrant,
    RepoAccessLevel, UserRole,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::bot_token::{self, BearerClass};
use super::repositories::find_repo;
use super::{WebState, api_error, bots};

const HOST_SESSION_COOKIE: &str = "__Host-jeryu-session";
const LOCAL_SESSION_COOKIE: &str = "jeryu-session";
const CSRF_HEADER: &str = "x-jeryu-csrf";
const AUTH_LIMIT_MAX: u32 = 10;
/// One client address may join the waitlist five times an hour, whatever
/// email it sends. A separate route bucket caps the whole path.
const WAITLIST_IP_MAX: u32 = 5;
const WAITLIST_IP_WINDOW_SECS: i64 = 60 * 60;
const WAITLIST_ROUTE_MAX: u32 = 30;
const WAITLIST_ROUTE_WINDOW_SECS: i64 = 60;
/// Reject a waitlist body larger than this before it is parsed.
pub(super) const WAITLIST_BODY_MAX: usize = 4096;
const WAITLIST_PAGE_DEFAULT: u32 = 100;
const WAITLIST_PAGE_MAX: u32 = 500;
#[cfg(test)]
const AUTH_LIMIT_WINDOW_SECS: i64 = request_limits::LIMIT_WINDOW_SECS;
const REMEMBER_ME_MAX_AGE_SECS: i64 = 60 * 60 * 24 * 30;

#[derive(Debug, Deserialize)]
pub(super) struct SignupRequest {
    login: String,
    password: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct WaitlistJoinRequest {
    email: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub(super) struct WaitlistListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LoginRequest {
    login: String,
    password: String,
    #[serde(default)]
    remember_me: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateTokenRequest {
    name: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PasswordChangeRequest {
    current_password: String,
    new_password: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct GrantRequest {
    access: RepoAccessLevel,
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateUserRequest {
    login: String,
    role: UserRole,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AuthUserResponse {
    login: String,
    role: UserRole,
    must_change_password: bool,
    csrf_token: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TokenResponse {
    id: String,
    token: String,
    name: String,
    expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TokenSummaryResponse {
    id: String,
    name: String,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub(super) struct PasswordResetResponse {
    login: String,
    password: String,
}

#[derive(Debug, Serialize)]
pub(super) struct CreatedUserResponse {
    login: String,
    role: UserRole,
    password: String,
}

impl From<AccountSummary> for AuthUserResponse {
    fn from(account: AccountSummary) -> Self {
        Self::new(account, None)
    }
}

impl AuthUserResponse {
    fn new(account: AccountSummary, csrf_token: Option<String>) -> Self {
        Self {
            login: account.login,
            role: account.role,
            must_change_password: account.must_change_password,
            csrf_token,
        }
    }
}

impl From<PersonalAccessTokenSummary> for TokenSummaryResponse {
    fn from(token: PersonalAccessTokenSummary) -> Self {
        Self {
            id: token.id.to_string(),
            name: token.name,
            created_at: token.created_at,
            expires_at: token.expires_at,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RateLimitBucket {
    attempts: u32,
    reset_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthSource {
    Bearer,
    Session,
    LocalDev,
}

#[derive(Clone, Debug)]
pub(crate) struct HeaderAuth {
    pub(crate) account: AccountSummary,
    pub(crate) source: AuthSource,
    pub(crate) bot: Option<bot_token::BotPrincipal>,
}

pub(super) async fn signup(
    State(state): State<Arc<WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(request): Json<SignupRequest>,
) -> AxumResponse {
    if auth_rate_limit_exceeded(&state, peer.as_ref(), &headers, "signup", &request.login) {
        return rate_limited();
    }
    let account = match state
        .core
        .create_account(&request.login, &request.password, UserRole::User)
    {
        Ok(account) => account,
        Err(ForgeError::Conflict(_)) => {
            return api_error(
                StatusCode::CONFLICT,
                "conflict",
                "an account with that login already exists",
            );
        }
        Err(ForgeError::Validation(reason)) => {
            return api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", &reason);
        }
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_failed",
                &format!("could not create account: {error}"),
            );
        }
    };
    let session = match state.core.create_session(&account.login) {
        Ok(session) => session,
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_failed",
                &format!("could not create session: {error}"),
            );
        }
    };
    let csrf_token = session.session.csrf_token.clone();
    let mut response = Json(AuthUserResponse::new(account, Some(csrf_token))).into_response();
    if let Ok(value) = cookie_header(&state, &session.token, None) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// `POST /api/v1/waitlist` — store an email for a later invitation.
///
/// This does not create an account, a session, or a cookie. A valid new
/// address and a repeat of a stored address both answer `202 {"result":"received"}`.
pub(super) async fn waitlist_join(
    State(state): State<Arc<WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(request): Json<WaitlistJoinRequest>,
) -> AxumResponse {
    if waitlist_rate_limit_exceeded(&state, peer.as_ref(), &headers) {
        return rate_limited();
    }
    match state.core.join_waitlist(
        &request.email,
        request.name.as_deref(),
        request.note.as_deref(),
    ) {
        Ok(_) => {}
        Err(error) => return waitlist_join_failure(error),
    }
    (StatusCode::ACCEPTED, Json(json!({ "result": "received" }))).into_response()
}

/// Map a failed `join_waitlist` onto the public waitlist response.
///
/// A full list is the typed `ForgeError::WaitlistFull` from the core cap; no
/// display text is matched, so a storage error that happens to mention
/// `waitlist_full` stays a 500. Any other failure keeps its detail off the wire.
pub(super) fn waitlist_join_failure(error: ForgeError) -> AxumResponse {
    match error {
        ForgeError::Validation(reason) => {
            api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", &reason)
        }
        ForgeError::WaitlistFull => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "waitlist_full",
            "the waitlist is not accepting new addresses",
        ),
        _ => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            "could not save the waitlist signup",
        ),
    }
}

/// `GET /api/v1/admin/waitlist` — newest request first, one page at a time.
pub(super) async fn admin_waitlist(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<WaitlistListQuery>,
) -> AxumResponse {
    if account.role != UserRole::Admin {
        return forbidden("admin role required");
    }
    let limit = query.limit.unwrap_or(WAITLIST_PAGE_DEFAULT);
    if limit == 0 || limit > WAITLIST_PAGE_MAX {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "limit must be from 1 to 500",
        );
    }
    let offset = query.offset.unwrap_or(0);
    let mut rows = state.core.list_waitlist();
    rows.sort_by(|left, right| {
        right
            .last_requested_at
            .cmp(&left.last_requested_at)
            .then_with(|| left.email.cmp(&right.email))
    });
    let total = rows.len();
    let signups: Vec<_> = rows
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();
    if state
        .core
        .append_audit_as(
            &account.login,
            "waitlist.list",
            "waitlist",
            "completed",
            json!({ "limit": limit, "offset": offset, "total": total }),
        )
        .is_err()
    {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            "could not record the waitlist read",
        );
    }
    Json(json!({
        "signups": signups,
        "limit": limit,
        "offset": offset,
        "total": total,
    }))
    .into_response()
}

pub(super) async fn login(
    State(state): State<Arc<WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> AxumResponse {
    if auth_rate_limit_exceeded(&state, peer.as_ref(), &headers, "login", &request.login) {
        return rate_limited();
    }
    let account = match state
        .core
        .authenticate_password(&request.login, &request.password)
    {
        Ok(account) => account,
        Err(_) => {
            return api_error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "invalid login or password",
            );
        }
    };
    let session = match if request.remember_me {
        state
            .core
            .create_session_with_ttl(&account.login, Duration::seconds(REMEMBER_ME_MAX_AGE_SECS))
    } else {
        state.core.create_session(&account.login)
    } {
        Ok(session) => session,
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_failed",
                &format!("could not create session: {error}"),
            );
        }
    };
    let csrf_token = session.session.csrf_token.clone();
    let mut response = Json(AuthUserResponse::new(account, Some(csrf_token))).into_response();
    let max_age = request.remember_me.then_some(REMEMBER_ME_MAX_AGE_SECS);
    if let Ok(value) = cookie_header(&state, &session.token, max_age) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

pub(super) async fn logout(State(state): State<Arc<WebState>>, headers: HeaderMap) -> AxumResponse {
    if let Some(token) = session_token_from_headers(&headers) {
        let _ = state.core.revoke_session(&token);
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    if let Ok(value) = expired_cookie_header(&state) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

pub(super) async fn me(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    headers: HeaderMap,
) -> AxumResponse {
    let csrf_token = session_token_from_headers(&headers).and_then(|token| {
        state
            .core
            .session_for_token(&token)
            .map(|(_, session)| session.csrf_token)
    });
    Json(AuthUserResponse::new(account, csrf_token)).into_response()
}

pub(super) async fn change_password(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Json(request): Json<PasswordChangeRequest>,
) -> AxumResponse {
    match state.core.change_account_password(
        &account.login,
        &request.current_password,
        &request.new_password,
    ) {
        Ok(updated) => match state.core.create_session(&updated.login) {
            Ok(session) => {
                let csrf_token = session.session.csrf_token.clone();
                let mut response =
                    Json(AuthUserResponse::new(updated, Some(csrf_token))).into_response();
                match cookie_header(&state, &session.token, None) {
                    Ok(value) => {
                        response.headers_mut().append(header::SET_COOKIE, value);
                        response
                    }
                    Err(error) => api_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "session_rotation_failed",
                        &format!("could not publish the rotated session: {error}"),
                    ),
                }
            }
            Err(error) => api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session_rotation_failed",
                &format!("could not create the rotated session: {error}"),
            ),
        },
        Err(ForgeError::Validation(_)) => api_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid current password",
        ),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("could not change password: {error}"),
        ),
    }
}

pub(super) async fn create_token(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Json(request): Json<CreateTokenRequest>,
) -> AxumResponse {
    let name = request.name.unwrap_or_else(|| "web token".to_string());
    match state
        .core
        .create_personal_access_token(&account.login, &name, request.expires_at)
    {
        Ok(receipt) => Json(TokenResponse {
            id: receipt.token.id.to_string(),
            token: receipt.secret,
            name: receipt.token.name,
            expires_at: receipt.token.expires_at,
        })
        .into_response(),
        Err(ForgeError::Validation(reason)) => {
            api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", &reason)
        }
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("could not create token: {error}"),
        ),
    }
}

pub(super) async fn list_tokens(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> AxumResponse {
    match state.core.list_personal_access_tokens(&account.login) {
        Ok(tokens) => Json(
            tokens
                .into_iter()
                .map(TokenSummaryResponse::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("could not list tokens: {error}"),
        ),
    }
}

pub(super) async fn delete_token(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
) -> AxumResponse {
    let id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                "invalid token id",
            );
        }
    };
    match state.core.revoke_personal_access_token(&account.login, id) {
        Ok(true) => (StatusCode::NO_CONTENT, "").into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "not_found", "token not found"),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("could not delete token: {error}"),
        ),
    }
}

pub(super) async fn admin_users(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> AxumResponse {
    if account.role != UserRole::Admin {
        return forbidden("admin role required");
    }
    Json(state.core.list_accounts()).into_response()
}

/// `POST /api/v1/admin/users` — add a forge identity with its role and hand
/// back a one-time password, so an operator never edits the account store by
/// hand with the service stopped.
pub(super) async fn admin_create_user(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(request): Json<CreateUserRequest>,
) -> AxumResponse {
    if account.role != UserRole::Admin {
        return forbidden("admin role required");
    }
    if auth_rate_limit_exceeded(&state, peer.as_ref(), &headers, "create", &request.login) {
        return rate_limited();
    }
    let CreateUserRequest { login, role } = request;
    let password = match state.core.generate_one_time_password() {
        Ok(password) => password,
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_failed",
                &format!("could not generate password: {error}"),
            );
        }
    };
    let _ = state.core.append_audit_as(
        &account.login,
        "account.create",
        &login,
        "requested",
        json!({ "role": &role }),
    );
    // The temporary flavour forces a password change, so the one-time password
    // in this reply cannot outlive the first login.
    match state
        .core
        .create_temporary_account(&login, &password, role.clone())
    {
        Ok(created) => {
            let _ = state.core.append_audit_as(
                &account.login,
                "account.create",
                &created.login,
                "completed",
                json!({ "role": created.role }),
            );
            (
                StatusCode::CREATED,
                Json(CreatedUserResponse {
                    login: created.login,
                    role: created.role,
                    password,
                }),
            )
                .into_response()
        }
        Err(error) => {
            let _ = state.core.append_audit_as(
                &account.login,
                "account.create",
                &login,
                "failed",
                json!({ "role": &role, "error": error.to_string() }),
            );
            match error {
                ForgeError::Conflict(_) => {
                    api_error(StatusCode::CONFLICT, "conflict", "user already exists")
                }
                ForgeError::Validation(reason) => {
                    api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", &reason)
                }
                error => api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage_failed",
                    &format!("could not create user: {error}"),
                ),
            }
        }
    }
}

pub(super) async fn admin_reset_password(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    AxumPath(login): AxumPath<String>,
) -> AxumResponse {
    if account.role != UserRole::Admin {
        return forbidden("admin role required");
    }
    if auth_rate_limit_exceeded(&state, peer.as_ref(), &headers, "reset", &login) {
        return rate_limited();
    }
    let password = match state.core.generate_one_time_password() {
        Ok(password) => password,
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_failed",
                &format!("could not generate password: {error}"),
            );
        }
    };
    match state.core.reset_account_password(&login, &password) {
        Ok(_) => Json(PasswordResetResponse { login, password }).into_response(),
        Err(ForgeError::NotFound(_)) => {
            api_error(StatusCode::NOT_FOUND, "not_found", "user not found")
        }
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("could not reset password: {error}"),
        ),
    }
}

pub(super) async fn admin_repo_grants(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((owner, repo)): AxumPath<(String, String)>,
) -> AxumResponse {
    match state
        .core
        .list_repo_access_checked(&account.login, &owner, &repo)
    {
        Ok(grants) => Json(grants).into_response(),
        // Core's *_checked access calls reuse BranchProtection for "caller is
        // not a repo admin", so these routes answer 403, unlike the 405 the
        // GitHub-compat `error_response` uses for real protection blocks.
        Err(ForgeError::BranchProtection(_)) => forbidden("repo admin access required"),
        Err(ForgeError::NotFound(_)) => {
            api_error(StatusCode::NOT_FOUND, "not_found", "repository not found")
        }
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("could not list access: {error}"),
        ),
    }
}

pub(super) async fn admin_grant_repo(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((owner, repo, login)): AxumPath<(String, String, String)>,
    Json(request): Json<GrantRequest>,
) -> AxumResponse {
    match state.core.grant_repo_access_checked(
        &account.login,
        &login,
        &owner,
        &repo,
        request.access,
    ) {
        Ok(grant) => Json(grant).into_response(),
        Err(ForgeError::BranchProtection(_)) => forbidden("repo admin access required"),
        Err(ForgeError::NotFound(_)) => api_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "user or repository not found",
        ),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("could not grant access: {error}"),
        ),
    }
}

pub(super) async fn admin_revoke_repo(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((owner, repo, login)): AxumPath<(String, String, String)>,
) -> AxumResponse {
    match state
        .core
        .revoke_repo_access_checked(&account.login, &login, &owner, &repo)
    {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(ForgeError::BranchProtection(_)) => forbidden("repo admin access required"),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("could not revoke access: {error}"),
        ),
    }
}

pub(super) async fn gate(
    State(state): State<Arc<WebState>>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> AxumResponse {
    if credential_in_query(request.uri()) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "credentials must not be passed as query parameters",
        );
    }
    if !auth_applies(request.uri().path()) {
        return next.run(request).await;
    }
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect| connect.0);
    let mut read_budget = None;
    let auth = if !state.auth_required || local_dev_trusted(&state, peer, request.headers()) {
        HeaderAuth {
            account: trusted_local_account(&state),
            source: AuthSource::LocalDev,
            bot: None,
        }
    } else {
        let ip = client_ip(peer, request.headers());
        let bad_key = format!("bad-credential:{ip}");
        let credential = request_limits::credential_key(request.headers());
        if credential.is_some() {
            let budget = request_limits::budget_for(
                &state
                    .auth_rate_limits
                    .lock()
                    .expect("auth rate-limit mutex poisoned"),
                &bad_key,
                Utc::now(),
                request_limits::bad_tokens_per_window(),
            );
            if budget.remaining == 0 {
                return request_limits::refused(
                    "too many failed credentials from this client",
                    &budget,
                );
            }
        }
        match authenticate_headers(&state, request.headers()) {
            Some(auth) => {
                let read = matches!(*request.method(), Method::GET | Method::HEAD);
                if read && let Some(credential) = credential {
                    let (over, budget) = request_limits::hit_with_budget(
                        &mut state
                            .auth_rate_limits
                            .lock()
                            .expect("auth rate-limit mutex poisoned"),
                        format!("read:{credential}"),
                        Utc::now(),
                        request_limits::reads_per_window(),
                    );
                    if over {
                        return request_limits::refused(
                            "too many reads with this credential",
                            &budget,
                        );
                    }
                    // Every metered read says what is left of the window, so a
                    // polling client can slow down before it is refused.
                    read_budget = Some(budget);
                }
                auth
            }
            None => {
                if credential.is_some() {
                    request_limits::hit(
                        &mut state
                            .auth_rate_limits
                            .lock()
                            .expect("auth rate-limit mutex poisoned"),
                        bad_key,
                        Utc::now(),
                        request_limits::bad_tokens_per_window(),
                    );
                } else if anonymous_read_allowed(&state, request.method(), request.uri().path()) {
                    request.extensions_mut().insert(anonymous_account());
                    return next.run(request).await;
                }
                if bot_credential_presented(request.headers()) {
                    return bot_token::invalid_credential();
                }
                return api_error(StatusCode::UNAUTHORIZED, "unauthorized", "login required");
            }
        }
    };
    let account = auth.account.clone();

    if account.must_change_password && !password_change_allowed_path(request.uri().path()) {
        return api_error(
            StatusCode::FORBIDDEN,
            "password_change_required",
            "password change required before continuing",
        );
    }

    if unsafe_method(request.method())
        && auth.source == AuthSource::Session
        && !csrf_valid(&state, request.headers())
    {
        return api_error(
            StatusCode::FORBIDDEN,
            "csrf_required",
            "missing or invalid CSRF token",
        );
    }

    if auth.bot.is_some()
        && (!bot_path_allowed(request.method(), request.uri().path())
            || bot_repo_admin_denied(request.method(), request.uri().path()))
    {
        return api_error(
            StatusCode::FORBIDDEN,
            "insufficient_scope",
            "the bot grant does not allow this request",
        );
    }
    // An unscoped personal access token carries every right of its account,
    // admin included, so MCP takes a scoped agent token instead. A personal
    // token is accepted only from a direct peer the site listed; the decision
    // reads the TCP peer, never a request header, and the list is empty by
    // default.
    if auth.bot.is_none()
        && auth.source == AuthSource::Bearer
        && mcp_path(request.uri().path())
        && !state.mcp_personal_token_peers.contains(&peer_ip(peer))
    {
        return api_error(
            StatusCode::FORBIDDEN,
            "insufficient_scope",
            "MCP needs a scoped agent token (POST /api/v1/bots/{id}/tokens); a personal \
             access token is accepted only from a peer listed in \
             JERYU_MCP_PERSONAL_TOKEN_PEERS",
        );
    }
    if request.uri().path() == "/api/v1/bot/whoami" && auth.bot.is_none() {
        return api_error(
            StatusCode::FORBIDDEN,
            "insufficient_scope",
            "the bot grant does not allow this request",
        );
    }

    // A WebSocket upgrade is a GET, so CSRF tokens never cover it, and a
    // browser sends ambient authority with a cross-site upgrade: the session
    // cookie, or local-dev trust for a loopback peer (where a page could
    // otherwise drive a host PTY). Refuse every upgrade that is not
    // bearer-authenticated and carries a foreign Origin. Browsers always send
    // Origin on an upgrade; a client without one (the CLI) is not a browser.
    if auth.source != AuthSource::Bearer
        && websocket_upgrade(request.headers())
        && request.headers().contains_key(header::ORIGIN)
        && !websocket_origin_allowed(
            request.headers(),
            std::env::var(PRODUCTION_ORIGIN_ENV).ok().as_deref(),
        )
    {
        return api_error(
            StatusCode::FORBIDDEN,
            "origin_denied",
            "cross-origin WebSocket upgrade refused",
        );
    }

    let viewer_run = super::agent_runs::agent_run_viewer_id(request.method(), request.uri().path());
    if viewer_run.is_none()
        && admin_only_request(request.method(), request.uri().path())
        && account.role != UserRole::Admin
    {
        return forbidden("admin role required");
    }
    // A session's owner and its repository's writers may watch the run; the
    // run's own routes still refuse non-admin control while it is a host PTY.
    if let Some(run_id) = viewer_run
        && !super::agent_runs::may_view_run(&state, &account, run_id)
    {
        return forbidden("agent run access denied");
    }

    let mut observed_repo = None;
    if let Some(repo_id) = repo_id_from_path(request.uri().path())
        && let Some(repo) = find_repo(&state, &repo_id)
    {
        observed_repo = Some(repo.full_name.clone());
        if let Some(bot) = &auth.bot {
            let write = !matches!(*request.method(), Method::GET | Method::HEAD);
            let (task_kind, task_id) = bots::task_from_api_path(request.uri().path());
            let call = bots::bot_call(bot, &repo.owner, &repo.name, write, task_kind, task_id);
            if let Err(error) = state.core.authorize_bot(&call) {
                return bots::map_bot_error(error);
            }
        } else {
            let allowed = match *request.method() {
                Method::GET | Method::HEAD => {
                    account.role == UserRole::Admin
                        || !repo.private
                        || state
                            .core
                            .user_can_read_repo(&account.login, &repo.owner, &repo.name)
                }
                Method::DELETE => {
                    account.role == UserRole::Admin
                        || state
                            .core
                            .user_can_admin_repo(&account.login, &repo.owner, &repo.name)
                }
                _ => {
                    account.role == UserRole::Admin
                        || state
                            .core
                            .user_can_write_repo(&account.login, &repo.owner, &repo.name)
                }
            };
            if !allowed {
                return forbidden("repository access denied");
            }
        }
    }

    let observed_bot = auth.bot.clone();
    let observed_method = request.method().clone();
    let observed_path = request.uri().path().to_string();
    if let Some(bot) = auth.bot.clone() {
        request.extensions_mut().insert(bot);
    }
    request.extensions_mut().insert(account);
    let mut response = next.run(request).await;
    if let Some(bot) = observed_bot
        && (observed_path.starts_with("/api/v1/repos/")
            || observed_path == "/mcp"
            || observed_path.starts_with("/mcp/"))
    {
        bots::record_bot_http(
            &state,
            &bot,
            observed_method.as_str(),
            &observed_path,
            observed_repo.as_deref(),
            response.status(),
        );
    }
    if let Some(budget) = read_budget {
        request_limits::stamp(&mut response, &budget);
    }
    response
}

pub(crate) fn authenticate_headers(state: &WebState, headers: &HeaderMap) -> Option<HeaderAuth> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(jeryu_gitd::auth::extract_bearer_or_basic);
    if let Some(bearer) = bearer {
        match bot_token::classify_bearer(&bearer) {
            BearerClass::BotAccess => {
                if let Some((account, bot)) =
                    bot_token::authenticate_bearer(state, headers, &bearer)
                {
                    return Some(HeaderAuth {
                        account,
                        source: AuthSource::Bearer,
                        bot: Some(bot),
                    });
                }
                return None;
            }
            BearerClass::AgentToken => {
                return bot_token::authenticate_agent_token(state, &bearer).map(
                    |(account, bot)| HeaderAuth {
                        account,
                        source: AuthSource::Bearer,
                        bot: Some(bot),
                    },
                );
            }
            BearerClass::Unusable => return None,
            BearerClass::Other => {
                if let Some(account) = state.core.authenticate_personal_access_token(&bearer) {
                    return Some(HeaderAuth {
                        account,
                        source: AuthSource::Bearer,
                        bot: None,
                    });
                }
            }
        }
    }
    if let Some(token) = session_token_from_headers(headers)
        && let Some(account) = state.core.authenticate_session(&token)
    {
        return Some(HeaderAuth {
            account,
            source: AuthSource::Session,
            bot: None,
        });
    }
    None
}

pub(crate) fn trusted_local_account(state: &WebState) -> AccountSummary {
    match state.core.get_account("jeryu-admin") {
        Ok(account) => account,
        Err(_) => AccountSummary {
            login: "jeryu-admin".to_string(),
            display_name: "Jeryu Admin".to_string(),
            role: UserRole::Admin,
            status: AccountStatus::Active,
            auth_epoch: 0,
            must_change_password: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
    }
}

/// External CI-evidence maintenance is an administrative boundary. Native
/// runners report through runner-only APIs; the server publishes their checks
/// internally instead of handing a repository writer a check-minting route.
pub(crate) fn can_publish_external_ci_evidence(account: &AccountSummary) -> bool {
    account.role == UserRole::Admin
}

/// Accounts that may post commit statuses and check runs: global admins, plus the dedicated gate
/// identities named in `JERYU_CI_PUBLISHERS` (comma-separated). A gate identity is one
/// installation's login, so there is no default: unset, only global admins publish. Gate runners
/// post required contexts without holding admin, so a runner token cannot merge, administer
/// repositories or ingest audit scores (those stay admin-only).
pub(crate) fn can_publish_gate_statuses(account: &AccountSummary) -> bool {
    account.role == UserRole::Admin
        || CI_PUBLISHERS
            .configured()
            .iter()
            .any(|login| login == &account.login)
}

/// Accounts whose jankurai report the forge will treat as authoritative: the
/// runner identities named in `JERYU_JANKURAI_SCORERS` (comma-separated, no
/// default — the scoring login is site configuration), plus global admins for
/// maintenance backfills.
///
/// Scoring moved off the forge host, so an identity here is trusted to have run
/// the governed auditor. That is why it is a short, configured list and not
/// "anyone who can write the repository": the ticket, the head, the base and
/// the auditor digest are all checked on top of it.
pub(crate) fn can_submit_runner_audit(account: &AccountSummary) -> bool {
    account.role == UserRole::Admin
        || JANKURAI_SCORERS
            .configured()
            .iter()
            .any(|login| login == &account.login)
}

/// Recording a deployment (or appending one of its statuses) is a claim about
/// what an environment runs, which the release views and rollback decisions
/// read as fact. Global admin alone is not enough: the gate and reviewer
/// identities are admins, and the gate's token is readable by the pull request
/// code it gates. So the caller must be an admin named in `JERYU_DEPLOYERS`
/// (comma-separated). Those logins are site configuration, so there is no
/// default: unset, nothing may record a deployment and the surface refuses with
/// the setting's name.
pub(crate) fn can_record_deployments(account: &AccountSummary) -> bool {
    is_deployer(account, DEPLOYERS.configured())
}

/// Logins a site trusts for one capability, read from its setting the first
/// time the capability is asked about. There is no host default for any of
/// them: an automation login belongs to one installation.
pub(crate) struct CapabilityLogins {
    setting: &'static str,
    logins: std::sync::OnceLock<Vec<String>>,
}

impl CapabilityLogins {
    const fn new(setting: &'static str) -> Self {
        Self {
            setting,
            logins: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn configured(&self) -> &[String] {
        self.logins
            .get_or_init(|| publisher_list(&std::env::var(self.setting).unwrap_or_default()))
    }

    /// Seed the list without reading the environment, so a test states the
    /// site it is testing instead of depending on one.
    #[cfg(test)]
    pub(crate) fn configure_for_tests(&self, logins: &[&str]) {
        let _ = self
            .logins
            .set(logins.iter().map(|login| (*login).to_owned()).collect());
    }
}

/// One login a site names for a role, read from its setting the first time the
/// role is asked about. No default, for the same reason as [`CapabilityLogins`].
pub(crate) struct RoleLogin {
    setting: &'static str,
    login: std::sync::OnceLock<Option<String>>,
}

impl RoleLogin {
    const fn new(setting: &'static str) -> Self {
        Self {
            setting,
            login: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn configured(&self) -> Option<&str> {
        self.login
            .get_or_init(|| {
                std::env::var(self.setting)
                    .ok()
                    .map(|login| login.trim().to_owned())
                    .filter(|login| !login.is_empty())
            })
            .as_deref()
    }

    #[cfg(test)]
    pub(crate) fn configure_for_tests(&self, login: &str) {
        let _ = self.login.set(Some(login.to_owned()));
    }
}

pub(crate) static REVIEW_IDENTITY: RoleLogin = RoleLogin::new("JERYU_REVIEW_IDENTITY");
pub(crate) static MERGE_IDENTITY: RoleLogin = RoleLogin::new("JERYU_MERGE_IDENTITY");

pub(crate) static CI_PUBLISHERS: CapabilityLogins = CapabilityLogins::new("JERYU_CI_PUBLISHERS");
pub(crate) static JANKURAI_SCORERS: CapabilityLogins =
    CapabilityLogins::new("JERYU_JANKURAI_SCORERS");
pub(crate) static DEPLOYERS: CapabilityLogins = CapabilityLogins::new("JERYU_DEPLOYERS");
pub(crate) static AUTOMATION_IDENTITIES: CapabilityLogins =
    CapabilityLogins::new("JERYU_AUTOMATION_IDENTITIES");

fn is_deployer(account: &AccountSummary, deployers: &[String]) -> bool {
    account.role == UserRole::Admin && deployers.iter().any(|login| login == &account.login)
}

fn publisher_list(configured: &str) -> Vec<String> {
    configured
        .split(',')
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .map(str::to_string)
        .collect()
}

pub(super) fn forbidden(message: &str) -> AxumResponse {
    api_error(StatusCode::FORBIDDEN, "permission_denied", message)
}

fn auth_applies(path: &str) -> bool {
    path == "/mcp"
        || path.starts_with("/mcp/")
        || (path.starts_with("/api/v1/") && !open_path(path))
}

/// The paths that answer without a credential. Signup and login are how a
/// caller gets one; the rest is discovery — the error bodies every
/// unauthenticated refusal returns advertise the capability manifest, the
/// error catalog, the documentation pages and the OpenAPI document, so asking
/// for a credential to read them would dead-end the agent being steered.
pub(super) fn open_path(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/auth/signup"
            | "/api/v1/auth/login"
            | "/api/v1/bot/token"
            | "/api/v1/bot/token/refresh"
            | "/api/v1/waitlist"
            | "/api/v1/errors"
            | crate::discovery::CAPABILITIES_PATH
            | crate::discovery::DOCS_PATH
            | crate::discovery::OPENAPI_PATH
    ) || path.starts_with("/api/v1/docs/")
}

/// The GitHub edge (`/repos`, `/api/v3`, `/graphql`) authenticates outside
/// [`gate`], which only covers `/mcp` and `/api/v1/*`, so it asks here for the
/// same two account-state policies: an account that must change its password
/// cannot act anywhere, and a cookie-session mutation needs its CSRF header.
/// Token and basic-auth callers stay CSRF-exempt, as they are on `/api/v1`.
/// Returns the refusal message, which the caller renders in the edge's shape.
pub(super) fn account_state_refusal(
    state: &WebState,
    auth: &HeaderAuth,
    method: &Method,
    headers: &HeaderMap,
) -> Option<&'static str> {
    if auth.account.must_change_password {
        return Some("password change required before continuing");
    }
    if unsafe_method(method) && auth.source == AuthSource::Session && !csrf_valid(state, headers) {
        return Some("missing or invalid CSRF token");
    }
    None
}

fn password_change_allowed_path(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/auth/me" | "/api/v1/auth/logout" | "/api/v1/auth/password"
    )
}

/// Local-dev trust: an unauthenticated loopback caller acts as the local admin.
/// A request that carries a forwarding header came through a proxy, and a proxy
/// on the same host connects from loopback, so such a request is never trusted
/// this way. The header can only withhold trust, never grant it. This is
/// defense in depth, not a boundary: a proxy that sends no such header still
/// looks local, so local-dev trust must never be enabled behind an edge.
pub(crate) fn local_dev_trusted(
    state: &WebState,
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
) -> bool {
    state.trust_local_dev && peer.is_some_and(|peer| peer.ip().is_loopback()) && !forwarded(headers)
}

fn forwarded(headers: &HeaderMap) -> bool {
    const NAMES: [&str; 7] = [
        "forwarded",
        "via",
        "x-real-ip",
        "x-client-ip",
        "true-client-ip",
        "cf-connecting-ip",
        "x-envoy-external-address",
    ];
    headers.keys().any(|name| {
        let name = name.as_str();
        name.starts_with("x-forwarded-") || NAMES.contains(&name)
    })
}

fn websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// Configured public site origin, e.g. `https://forge.example`.
const PRODUCTION_ORIGIN_ENV: &str = "JERYU_PRODUCTION_ORIGIN";

/// A cookie WebSocket must come from the configured public origin when one is
/// set; otherwise (local and preview deployments) from this request's Host.
pub(super) fn websocket_origin_allowed(headers: &HeaderMap, production: Option<&str>) -> bool {
    let production = production
        .map(str::trim)
        .filter(|origin| !origin.is_empty());
    let Some(production) = production else {
        return same_origin(headers);
    };
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| {
            origin
                .trim_end_matches('/')
                .eq_ignore_ascii_case(production.trim_end_matches('/'))
        })
}

/// True when `Origin` names exactly the authority in `Host`. A missing or
/// opaque (`null`) Origin is not same-origin.
pub(super) fn same_origin(headers: &HeaderMap) -> bool {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let (Some(origin), Some(host)) = (origin, host) else {
        return false;
    };
    origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

fn unsafe_method(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn csrf_valid(state: &WebState, headers: &HeaderMap) -> bool {
    let Some(session_token) = session_token_from_headers(headers) else {
        return true;
    };
    let Some(csrf_token) = headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    state.core.session_csrf_matches(&session_token, csrf_token)
}

/// Admin-only by path, plus every mutation under `/api/v1/shift/` (its reads
/// stay open to any logged-in account).
pub(super) fn admin_only_request(method: &Method, path: &str) -> bool {
    admin_only_path(path)
        || (path.starts_with("/api/v1/shift/")
            && unsafe_method(method)
            && !shift_heartbeat_path(path))
        || (pipeline_events_path(path) && !unsafe_method(method))
        || (release_board_path(path) && !unsafe_method(method))
        || attention_path(path)
        || pins_path(path)
        || trace_path(path)
        || yield_path(path)
        || activity_reads_path(path)
        || audit_path(path)
}

/// The Activity histogram counts events by family and kind, and usage reports live
/// token spend per worker slot: both read the event log's contents, so like the log
/// itself they are for global admins (a non-admin could otherwise enumerate family
/// keys, private repositories included, through the family filter's 422).
fn activity_reads_path(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/events/histogram"
            | "/api/v1/events/histogram/"
            | "/api/v1/usage"
            | "/api/v1/usage/"
    )
}

/// Reporting a worker slot is the one shift write a worker-pool host makes, so
/// it is decided in the handler (admin or a `JERYU_SHIFT_REPORTERS` identity),
/// like runner heartbeats. Every other shift write stays admin-only.
fn shift_heartbeat_path(path: &str) -> bool {
    path == "/api/v1/shift/heartbeat" || path == "/api/v1/shift/heartbeat/"
}

/// Reading the pipeline event log is admin-only: events carry todo titles,
/// notes and log tails from repositories the reader may not have access to.
/// Posting is decided in the handler (admin or a `JERYU_EVENT_REPORTERS`
/// identity), like runner heartbeats.
fn pipeline_events_path(path: &str) -> bool {
    path == "/api/v1/events" || path == "/api/v1/events/"
}

/// Release boards name hosts, commands and the pinned commits of private
/// repositories, so reading them is for global admins. Writing is decided in
/// the handler (admin or a `JERYU_BOARD_REPORTERS` identity), like heartbeats.
fn release_board_path(path: &str) -> bool {
    path == "/api/v1/release-board"
        || path == "/api/v1/release-board/"
        || path.starts_with("/api/v1/release-board/")
}

/// The attention inbox names todos, notes and pull requests across every
/// repository, so like the event log it is for global admins only. Its
/// acknowledgements (`/api/v1/attention/acks`) decide what the inbox hides,
/// which is an administrator's call as much as reading it is.
fn attention_path(path: &str) -> bool {
    path == "/api/v1/attention" || path.starts_with("/api/v1/attention/")
}

/// Pins name the unreleased commits of every dependency, private ones
/// included, so like the inbox they are for global admins only.
fn pins_path(path: &str) -> bool {
    path == "/api/v1/pins" || path == "/api/v1/pins/"
}

/// One work trace names the todos, pull requests and deployments of every
/// repository the work touches, so like the inbox it is for global admins.
fn trace_path(path: &str) -> bool {
    path == "/api/v1/trace" || path == "/api/v1/trace/"
}

/// The yield report names todos, their spend and the routing experiments of
/// every family, so like the event log it reads from it is for global admins.
fn yield_path(path: &str) -> bool {
    path == "/api/v1/yield" || path == "/api/v1/yield/"
}

fn admin_only_path(path: &str) -> bool {
    path.starts_with("/api/v1/admin/")
        || path.starts_with("/api/v1/control-plane/")
        || path.starts_with("/api/v1/workcells")
        || path.starts_with("/api/v1/agent-runs")
        || path.starts_with("/api/v1/fleet/")
        || path.starts_with("/api/v1/codegraph/tool-build/")
        || path.starts_with("/api/v1/tool-finder/")
}

/// The forge audit trail names privileged mutations across every repository,
/// private ones included, so like the event log it is for global admins only.
fn audit_path(path: &str) -> bool {
    path == "/api/v1/audit" || path == "/api/v1/audit/"
}

pub(super) fn bot_token_rate_limit(
    state: &WebState,
    peer: Option<&ConnectInfo<SocketAddr>>,
    headers: &HeaderMap,
    action: &str,
) -> Option<AxumResponse> {
    if auth_rate_limit_exceeded(state, peer, headers, action, "bot") {
        Some(rate_limited())
    } else {
        None
    }
}

/// Five joins an hour from one address, and thirty a minute for the route.
/// The address is `client_ip`, which trusts `X-Forwarded-For` only from peers
/// in `JERYU_TRUSTED_PROXIES`: behind a reverse proxy that is not listed there,
/// every join shares the proxy's bucket, which `super::proxy_trust` warns
/// about once and reports on the admin runners page. The route bucket is a deliberate
/// global ceiling on unauthenticated writes; a burst from many addresses can
/// close it for the rest of the minute, which is the tradeoff for bounding
/// growth of the table.
/// The key does not include the email, so rotating addresses cannot multiply
/// the budget. A closed route window is refused before a per-address key is
/// created, so a flood of new addresses cannot grow the map for that hour.
fn waitlist_rate_limit_exceeded(
    state: &WebState,
    peer: Option<&ConnectInfo<SocketAddr>>,
    headers: &HeaderMap,
) -> bool {
    let ip = client_ip(peer.map(|connect| connect.0), headers);
    let now = Utc::now();
    let mut limits = state
        .auth_rate_limits
        .lock()
        .expect("auth rate-limit mutex poisoned");
    if waitlist_route_window_is_closed(&limits, now) {
        return true;
    }
    let address_over = request_limits::hit_for(
        &mut limits,
        format!("waitlist:{ip}"),
        now,
        WAITLIST_IP_MAX,
        WAITLIST_IP_WINDOW_SECS,
    );
    if address_over {
        return true;
    }
    request_limits::hit_for(
        &mut limits,
        "waitlist:route".to_string(),
        now,
        WAITLIST_ROUTE_MAX,
        WAITLIST_ROUTE_WINDOW_SECS,
    )
}

/// The route bucket already holds its thirty joins and the minute has not
/// elapsed. Reading it inserts nothing.
fn waitlist_route_window_is_closed(
    limits: &BTreeMap<String, RateLimitBucket>,
    now: DateTime<Utc>,
) -> bool {
    match limits.get("waitlist:route") {
        Some(bucket) => bucket.reset_at > now && bucket.attempts >= WAITLIST_ROUTE_MAX,
        None => false,
    }
}

fn auth_rate_limit_exceeded(
    state: &WebState,
    peer: Option<&ConnectInfo<SocketAddr>>,
    headers: &HeaderMap,
    action: &str,
    login: &str,
) -> bool {
    let ip = client_ip(peer.map(|connect| connect.0), headers);
    let login = login.trim().to_ascii_lowercase();
    let key = format!("{action}:{ip}:{login}");
    let mut limits = state
        .auth_rate_limits
        .lock()
        .expect("auth rate-limit mutex poisoned");
    rate_limit_hit(&mut limits, key, Utc::now())
}

/// Counts one attempt against `key` at `now`, opening a fresh window once the
/// previous one has elapsed.
fn rate_limit_hit(
    limits: &mut std::collections::BTreeMap<String, RateLimitBucket>,
    key: String,
    now: DateTime<Utc>,
) -> bool {
    request_limits::hit(limits, key, now, AUTH_LIMIT_MAX)
}

/// Every 429 carries `Retry-After` and the rate-limit budget, so a client
/// waits the window out instead of retrying straight into the same refusal.
fn rate_limited() -> AxumResponse {
    request_limits::too_many("too many authentication attempts")
}

fn client_ip(peer: Option<SocketAddr>, headers: &HeaderMap) -> IpAddr {
    let trusted = trusted_proxy_ips();
    let ip = client_ip_with(peer, headers, &trusted);
    // A forwarding header from a peer the forge does not trust means the edge
    // is unlisted: say so once instead of silently sharing one bucket.
    super::proxy_trust::observe(peer_ip(peer), headers, &trusted);
    ip
}

/// The direct peer, or the unspecified address when the transport named none,
/// exactly as `client_ip_with` reads it.
fn peer_ip(peer: Option<SocketAddr>) -> IpAddr {
    peer.map(|addr| addr.ip())
        .unwrap_or(IpAddr::from([0, 0, 0, 0]))
}

/// Believes `X-Forwarded-For` only when the direct peer is a trusted proxy.
fn client_ip_with(peer: Option<SocketAddr>, headers: &HeaderMap, trusted: &[IpAddr]) -> IpAddr {
    let peer_ip = peer_ip(peer);
    if !trusted.contains(&peer_ip) {
        return peer_ip;
    }
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.trim().parse::<IpAddr>().ok())
        .unwrap_or(peer_ip)
}

pub(super) fn trusted_proxy_ips() -> Vec<IpAddr> {
    std::env::var("JERYU_TRUSTED_PROXIES")
        .map(|raw| parse_trusted_proxies(&raw))
        .unwrap_or_default()
}

fn parse_trusted_proxies(raw: &str) -> Vec<IpAddr> {
    raw.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<IpAddr>().ok())
        .collect()
}

/// Login carried by a visitor with no credential. Core rejects it as an
/// account name (`(` is not a login byte), so it can never hold a grant.
pub(crate) const ANONYMOUS_LOGIN: &str = "(anonymous)";

/// Repository sub-resources a visitor without credentials may read on a
/// public repository: what the repository page shows. Settings, agent runs
/// and sessions stay behind a login.
const ANONYMOUS_REPO_READS: &[&str] = &[
    "refs",
    "tree",
    "blob",
    "raw",
    "readme",
    "pages",
    "blame",
    "compare",
    "release-tag",
    "pulls",
];

/// A public repository is readable without an account, as its PUBLIC chip and
/// Clone button promise: the repository list (narrowed to public ones by the
/// list handler) and the read routes of one public repository.
fn anonymous_read_allowed(state: &WebState, method: &Method, path: &str) -> bool {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return false;
    }
    // The site settings answer filters the wiki by the caller's access, so
    // an anonymous visitor only ever learns about a public wiki.
    if path == "/api/v1/repos" || path == "/api/v1/site-settings" {
        return true;
    }
    let Some(rest) = path.strip_prefix("/api/v1/repos/") else {
        return false;
    };
    // The repository is addressed by one (possibly percent-encoded) segment,
    // UUID or `owner%2Fname`, or by a bare `owner/name` pair.
    let segments: Vec<&str> = rest.split('/').collect();
    let (repo, sub) = match find_repo(state, &percent_decode(segments[0])) {
        Some(repo) => (repo, segments.get(1)),
        None => match segments.get(1).and_then(|name| {
            find_repo(
                state,
                &format!("{}/{}", percent_decode(segments[0]), percent_decode(name)),
            )
        }) {
            Some(repo) => (repo, segments.get(2)),
            None => return false,
        },
    };
    !repo.private && sub.is_none_or(|sub| ANONYMOUS_REPO_READS.contains(sub))
}

fn anonymous_account() -> AccountSummary {
    AccountSummary {
        login: ANONYMOUS_LOGIN.to_string(),
        display_name: "Anonymous".to_string(),
        role: UserRole::User,
        status: AccountStatus::Active,
        auth_epoch: 0,
        must_change_password: false,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// Direct peers whose personal access tokens may use MCP, from
/// `JERYU_MCP_PERSONAL_TOKEN_PEERS`. Unset or empty means none.
pub(super) fn mcp_personal_token_peers_from_env() -> Vec<IpAddr> {
    let raw = std::env::var("JERYU_MCP_PERSONAL_TOKEN_PEERS").unwrap_or_default();
    let (peers, rejected) = parse_mcp_personal_token_peers(&raw);
    for entry in rejected {
        eprintln!(
            "jeryu-api: JERYU_MCP_PERSONAL_TOKEN_PEERS ignores {entry:?}: list single \
             IP addresses, and never the unspecified address"
        );
    }
    peers
}

/// Single IP addresses only. The unspecified address is refused: a request with
/// no known peer reads as `0.0.0.0`, so listing it would admit every such request.
pub(super) fn parse_mcp_personal_token_peers(raw: &str) -> (Vec<IpAddr>, Vec<String>) {
    let mut peers = Vec::new();
    let mut rejected = Vec::new();
    for entry in raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        match entry.parse::<IpAddr>() {
            Ok(ip) if !ip.is_unspecified() => peers.push(ip),
            _ => rejected.push(entry.to_string()),
        }
    }
    (peers, rejected)
}

fn mcp_path(path: &str) -> bool {
    path == "/mcp" || path.starts_with("/mcp/")
}

fn bot_credential_presented(headers: &HeaderMap) -> bool {
    let Some(raw) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(token) = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
    else {
        return false;
    };
    matches!(
        bot_token::classify_bearer(token),
        bot_token::BearerClass::BotAccess
            | bot_token::BearerClass::Unusable
            | bot_token::BearerClass::AgentToken
    )
}

fn credential_in_query(uri: &axum::http::Uri) -> bool {
    let Some(query) = uri.query() else {
        return false;
    };
    query.split('&').any(|pair| {
        let key = percent_decode(pair.split('=').next().unwrap_or(""));
        key == "access_token" || key == "bot_key"
    })
}

fn bot_path_allowed(method: &Method, path: &str) -> bool {
    if path == "/api/v1/bot/whoami" {
        return matches!(*method, Method::GET | Method::HEAD);
    }
    path.starts_with("/api/v1/repos/") || path == "/mcp" || path.starts_with("/mcp/")
}

/// Repository deletion trusts the gate, and `repo_delete` does not look at the
/// caller again. Branch protection, transfer, hooks, keys, and the other
/// GitHub admin segments are the same refusal the GitHub edge already makes.
fn bot_repo_admin_denied(method: &Method, path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/v1/repos/") else {
        return false;
    };
    let segments: Vec<&str> = rest
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        return false;
    }
    // The repository itself (delete, archive, rename, settings) is the
    // owner's decision, never a bot's, whatever the owner's grant.
    if unsafe_method(method) && segments.len() == 1 {
        return true;
    }
    segments.len() >= 2 && bots::github_segment_blocked(&segments[1..])
}

fn repo_id_from_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/api/v1/repos/")?;
    let id = rest.split('/').next()?;
    if id.is_empty() {
        None
    } else {
        Some(percent_decode(id))
    }
}

pub(super) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn session_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        if let Some((name, value)) = part.trim().split_once('=')
            && (name == HOST_SESSION_COOKIE || name == LOCAL_SESSION_COOKIE)
        {
            return Some(value.to_string());
        }
    }
    None
}

fn cookie_header(
    state: &WebState,
    token: &str,
    max_age_secs: Option<i64>,
) -> Result<axum::http::HeaderValue, axum::http::header::InvalidHeaderValue> {
    let name = if state.secure_cookies {
        HOST_SESSION_COOKIE
    } else {
        LOCAL_SESSION_COOKIE
    };
    let secure = if state.secure_cookies { "; Secure" } else { "" };
    let max_age = max_age_secs
        .map(|seconds| format!("; Max-Age={seconds}"))
        .unwrap_or_default();
    axum::http::HeaderValue::from_str(&format!(
        "{name}={token}; Path=/; HttpOnly; SameSite=Lax{max_age}{secure}"
    ))
}

fn expired_cookie_header(
    state: &WebState,
) -> Result<axum::http::HeaderValue, axum::http::header::InvalidHeaderValue> {
    let name = if state.secure_cookies {
        HOST_SESSION_COOKIE
    } else {
        LOCAL_SESSION_COOKIE
    };
    let secure = if state.secure_cookies { "; Secure" } else { "" };
    axum::http::HeaderValue::from_str(&format!(
        "{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}"
    ))
}

#[allow(dead_code)]
fn _grant_wire(_grant: &RepoAccessGrant) {}

mod request_limits;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod gate_status_publisher_tests {
    use super::publisher_list;

    #[test]
    fn publisher_list_trims_and_drops_empty_entries() {
        assert_eq!(
            publisher_list(" ci-bot , other-bot,,"),
            vec!["ci-bot", "other-bot"]
        );
        assert!(publisher_list("").is_empty());
    }
}

#[cfg(test)]
mod deployer_tests {
    use super::{AccountStatus, AccountSummary, UserRole, is_deployer, publisher_list};

    fn account(login: &str, role: UserRole) -> AccountSummary {
        AccountSummary {
            login: login.to_string(),
            display_name: login.to_string(),
            role,
            status: AccountStatus::Active,
            auth_epoch: 0,
            must_change_password: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn only_listed_admins_record_deployments() {
        let deployers = publisher_list("operator,rel-bot");
        assert!(is_deployer(
            &account("rel-bot", UserRole::Admin),
            &deployers
        ));
        assert!(
            !is_deployer(&account("ci-bot", UserRole::Admin), &deployers),
            "an admin automation identity is not a deployer"
        );
        assert!(
            !is_deployer(&account("rel-bot", UserRole::User), &deployers),
            "a listed login without admin is not a deployer"
        );
        assert!(!is_deployer(&account("rel-bot", UserRole::Admin), &[]));
    }
}
