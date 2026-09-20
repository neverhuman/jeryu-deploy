//! Repository routes (`/repos`, `/repos/{owner}/{repo}`) and their
//! GitHub-shaped repository renderer.

#[cfg(feature = "web")]
use jeryu_core::{AccountSummary, UserRole};
use jeryu_core::{CreateRepositoryRequest, Repository};
use serde_json::{Value, json};

use crate::routes::Response;

use super::GithubRouter;
use super::support::{
    Pagination, error_response, json_response, owner_for_create, owner_json, paginate, parse_body,
};

impl GithubRouter {
    pub(super) fn list_repos(&self, path: &str, page: Pagination) -> Response {
        let repos = self.core.list_repositories(None);
        let body: Vec<Value> = repos.iter().map(repository_json).collect();
        paginate(path, page, &body, |slice, _total| {
            Value::Array(slice.to_vec())
        })
    }

    #[cfg(feature = "web")]
    pub(crate) fn list_repos_for_account(&self, path: &str, account: &AccountSummary) -> Response {
        let path = super::normalize_github_path(path);
        let (_route_path, query) = path.split_once('?').unwrap_or((path, ""));
        let page = Pagination::from_query(query);
        let repos = self.core.list_repositories(None);
        let body: Vec<Value> = repos
            .iter()
            .filter(|repo| {
                account.role == UserRole::Admin
                    || self
                        .core
                        .user_can_read_repo(&account.login, &repo.owner, &repo.name)
            })
            .map(repository_json)
            .collect();
        paginate(path, page, &body, |slice, _total| {
            Value::Array(slice.to_vec())
        })
    }

    pub(super) fn create_repo(&self, body: &str) -> Response {
        let req: CreateRepositoryRequest = match parse_body(body) {
            Ok(value) => value,
            Err(response) => return response,
        };
        // GitHub authenticated-user repo creation; the in-memory edge uses the
        // request login when present, defaulting to the canonical owner.
        let owner = owner_for_create(body).unwrap_or_else(|| "jeryu".to_owned());
        match self.core.create_repository(&owner, req) {
            Ok(repo) => json_response(201, &repository_json(&repo)),
            Err(err) => error_response(err),
        }
    }

    pub(super) fn get_repo(&self, owner: &str, repo: &str) -> Response {
        match self.core.get_repository(owner, repo) {
            Ok(repo) => json_response(200, &repository_json(&repo)),
            Err(err) => error_response(err),
        }
    }

    /// `PATCH /repos/{owner}/{repo}` — archive or unarchive, spelled the way
    /// GitHub spells it, so `gh` and every GitHub client already know how to
    /// ask. Admin-only; the edge refuses a non-admin before this runs.
    ///
    /// Only `archived` is accepted. Every other repository setting has its own
    /// typed Jeryu route, and quietly ignoring an unknown key would let a
    /// caller believe a setting moved when it did not, so an unrecognised body
    /// is a validation error rather than a silent success.
    ///
    /// `actor` is bound by the edge from the authenticated principal before
    /// this runs, so the audit trail names the caller, never the body.
    pub(super) fn update_repo(&self, owner: &str, repo: &str, body: &str) -> Response {
        let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else {
            return error_response(jeryu_core::ForgeError::Validation(
                "body must be a JSON object".to_string(),
            ));
        };
        let Some(archived) = fields.get("archived") else {
            return error_response(jeryu_core::ForgeError::Validation(
                "no supported field: this route accepts `archived` (a boolean)".to_string(),
            ));
        };
        let Some(archived) = archived.as_bool() else {
            return error_response(jeryu_core::ForgeError::Validation(
                "archived must be a boolean".to_string(),
            ));
        };
        let actor = fields
            .get("actor")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match self
            .core
            .set_repository_archived(actor, owner, repo, archived)
        {
            Ok(repo) => json_response(200, &repository_json(&repo)),
            Err(err) => error_response(err),
        }
    }
}

pub(super) fn repository_json(repo: &Repository) -> Value {
    json!({
        "id": repo.id,
        "name": repo.name,
        "full_name": repo.full_name,
        "private": repo.private,
        "owner": owner_json(&repo.owner),
        "description": repo.description,
        "default_branch": repo.default_branch,
        "archived": repo.archived,
        "disabled": repo.disabled,
        "html_url": super::support::web_url(&format!("/repos/jeryu/{}", repo.full_name)),
        "url": format!("/repos/{}", repo.full_name),
        "created_at": repo.created_at,
        "updated_at": repo.updated_at,
    })
}
