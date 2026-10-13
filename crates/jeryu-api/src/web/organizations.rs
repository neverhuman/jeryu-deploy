//! Organizations with owners, and the teams they run (ADR 0009).
//!
//! Core decides every right: a forge administrator creates an organization,
//! its owners (or an administrator) run its owners, teams and members, and an
//! organization a caller may not see answers not found.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use jeryu_core::{AccountSummary, ForgeError};
use serde::Deserialize;
use serde_json::json;

use super::bots::map_bot_error;
use super::{WebState, api_error};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateOrganization {
    login: String,
    #[serde(default)]
    display_name: Option<String>,
    owners: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTeam {
    name: String,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    members: Vec<String>,
}

/// `GET /api/v1/orgs` — organizations the caller owns or has a team in;
/// every organization for a forge administrator.
pub(super) async fn list(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> AxumResponse {
    match state.core.list_organizations_for(&account.login) {
        Ok(organizations) => Json(json!({ "organizations": organizations })).into_response(),
        Err(error) => map_error(error),
    }
}

/// `POST /api/v1/orgs` — a forge administrator creates an organization with
/// at least one owner.
pub(super) async fn create(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    body: Bytes,
) -> AxumResponse {
    let Ok(request) = serde_json::from_slice::<CreateOrganization>(&body) else {
        return invalid_body();
    };
    match state.core.create_organization_as(
        &account.login,
        &request.login,
        request.display_name.as_deref(),
        &request.owners,
    ) {
        Ok(organization) => (StatusCode::CREATED, Json(organization)).into_response(),
        Err(error) => map_error(error),
    }
}

/// `GET /api/v1/orgs/:org` — the organization, its teams, and whether the
/// caller may run it.
pub(super) async fn detail(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(org): AxumPath<String>,
) -> AxumResponse {
    match state.core.organization_for(&account.login, &org) {
        Ok(view) => Json(view).into_response(),
        Err(error) => map_error(error),
    }
}

/// `PUT /api/v1/orgs/:org/owners/:login`
pub(super) async fn add_owner(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((org, login)): AxumPath<(String, String)>,
) -> AxumResponse {
    match state
        .core
        .add_organization_owner(&account.login, &org, &login)
    {
        Ok(organization) => Json(organization).into_response(),
        Err(error) => map_error(error),
    }
}

/// `DELETE /api/v1/orgs/:org/owners/:login` — never the last owner.
pub(super) async fn remove_owner(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((org, login)): AxumPath<(String, String)>,
) -> AxumResponse {
    match state
        .core
        .remove_organization_owner(&account.login, &org, &login)
    {
        Ok(organization) => Json(organization).into_response(),
        Err(error) => map_error(error),
    }
}

/// `POST /api/v1/orgs/:org/teams`
pub(super) async fn create_team(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(org): AxumPath<String>,
    body: Bytes,
) -> AxumResponse {
    let Ok(request) = serde_json::from_slice::<CreateTeam>(&body) else {
        return invalid_body();
    };
    match state.core.create_team_as(
        &account.login,
        &org,
        &request.name,
        request.slug.as_deref(),
        &request.members,
    ) {
        Ok(team) => (StatusCode::CREATED, Json(team)).into_response(),
        Err(error) => map_error(error),
    }
}

/// `PUT /api/v1/orgs/:org/teams/:team/members/:login`
pub(super) async fn add_member(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((org, team, login)): AxumPath<(String, String, String)>,
) -> AxumResponse {
    match state
        .core
        .add_team_member(&account.login, &org, &team, &login)
    {
        Ok(team) => Json(team).into_response(),
        Err(error) => map_error(error),
    }
}

/// `DELETE /api/v1/orgs/:org/teams/:team/members/:login` — also revokes the
/// agent tokens that person issued for the bots this team stewards.
pub(super) async fn remove_member(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((org, team, login)): AxumPath<(String, String, String)>,
) -> AxumResponse {
    match state
        .core
        .remove_team_member(&account.login, &org, &team, &login)
    {
        Ok(team) => Json(team).into_response(),
        Err(error) => map_error(error),
    }
}

fn map_error(error: ForgeError) -> AxumResponse {
    if let ForgeError::Validation(message) = &error
        && message.starts_with("last_owner:")
    {
        return api_error(StatusCode::CONFLICT, "last_owner", message);
    }
    map_bot_error(error)
}

fn invalid_body() -> AxumResponse {
    api_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_input",
        "request body does not match the documented fields",
    )
}
