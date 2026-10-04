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
        let (route_path, query) = path.split_once('?').unwrap_or((path, ""));
        let page = match Pagination::from_query(query) {
            Ok(page) => page,
            Err(refused) => return refused,
        };
        // The same `Link` base the router builds: the edge URL plus the
        // caller's surviving filters, never the raw `?per_page=` they sent.
        let link_base = super::support::link_base(route_path, query);
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
        paginate(&link_base, page, &body, |slice, _total| {
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

    /// `PATCH /repos/{owner}/{repo}` — archive, unarchive or rename, spelled
    /// the way GitHub spells them (`archived`, `name`), so `gh` and every
    /// GitHub client already know how to ask. Admin-only; the edge refuses a
    /// non-admin before this runs.
    ///
    /// Only `archived` and `name` are accepted, one per request: archiving and
    /// renaming are separate decisions, and a rename of an archived repository
    /// is refused by the core anyway. Every other repository setting has its
    /// own typed Jeryu route, and quietly ignoring an unknown key would let a
    /// caller believe a setting moved when it did not, so an unrecognised body
    /// is a validation error rather than a silent success.
    ///
    /// `actor` is bound by the edge from the authenticated principal before
    /// this runs, so the audit trail names the caller, never the body.
    pub(super) fn update_repo(&self, owner: &str, repo: &str, body: &str) -> Response {
        let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else {
            return validation("body must be a JSON object");
        };
        let actor = fields
            .get("actor")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match (fields.get("archived"), fields.get("name")) {
            (Some(_), Some(_)) => validation(
                "send `archived` or `name`, not both: archive and rename are separate requests",
            ),
            (Some(archived), None) => {
                let Some(archived) = archived.as_bool() else {
                    return validation("archived must be a boolean");
                };
                match self
                    .core
                    .set_repository_archived(actor, owner, repo, archived)
                {
                    Ok(repo) => json_response(200, &repository_json(&repo)),
                    Err(err) => error_response(err),
                }
            }
            (None, Some(name)) => {
                let Some(name) = name.as_str() else {
                    return validation("name must be a string");
                };
                self.move_repo(actor, owner, repo, None, Some(name), 200)
            }
            (None, None) => validation(
                "no supported field: this route accepts `archived` (a boolean) or `name` (a string)",
            ),
        }
    }

    /// `POST /repos/{owner}/{repo}/transfer` — move the repository to
    /// `new_owner`, optionally renaming it to `new_name`, in GitHub's request
    /// shape. Admin-only; the edge refuses a non-admin before this runs.
    /// Answers `202` like GitHub, although the move is already complete.
    pub(super) fn transfer_repo(&self, owner: &str, repo: &str, body: &str) -> Response {
        let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else {
            return validation("body must be a JSON object");
        };
        let Some(new_owner) = fields.get("new_owner").and_then(Value::as_str) else {
            return validation("new_owner is required and must be a string");
        };
        let new_name = match fields.get("new_name") {
            None | Some(Value::Null) => None,
            Some(Value::String(name)) => Some(name.as_str()),
            Some(_) => return validation("new_name must be a string"),
        };
        let actor = fields
            .get("actor")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        self.move_repo(actor, owner, repo, Some(new_owner), new_name, 202)
    }

    /// Rename and/or transfer through the core. The path may name the
    /// repository by an old slug, the way GitHub keeps a moved repository's
    /// old URL working, so it is resolved to the current slug first.
    fn move_repo(
        &self,
        actor: &str,
        owner: &str,
        repo: &str,
        new_owner: Option<&str>,
        new_name: Option<&str>,
        status: u16,
    ) -> Response {
        let current = match self.core.get_repository(owner, repo) {
            Ok(current) => current,
            Err(err) => return error_response(err),
        };
        let new_owner = new_owner.unwrap_or(&current.owner);
        let new_name = new_name.unwrap_or(&current.name);
        match self
            .core
            .rename_repository(actor, &current.owner, &current.name, new_owner, new_name)
        {
            Ok(repo) => json_response(status, &repository_json(&repo)),
            Err(err) => error_response(err),
        }
    }
}

fn validation(message: &str) -> Response {
    error_response(jeryu_core::ForgeError::Validation(message.to_string()))
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
