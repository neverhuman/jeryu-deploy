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

    /// Repositories a bot may read. Forge admin is not a grant, and reach can
    /// hide repositories the owner could otherwise open.
    #[cfg(feature = "web")]
    pub(crate) fn list_repos_for_bot(
        &self,
        path: &str,
        bot: &crate::web::bot_token::BotPrincipal,
    ) -> Response {
        self.list_repos_filtered(path, |repo| {
            let call = crate::web::bots::bot_call(bot, &repo.owner, &repo.name, false, None, None);
            self.core.authorize_bot(&call).is_ok()
        })
    }

    #[cfg(feature = "web")]
    fn list_repos_filtered(&self, path: &str, allow: impl Fn(&Repository) -> bool) -> Response {
        let path = super::normalize_github_path(path);
        let (route_path, query) = path.split_once('?').unwrap_or((path, ""));
        let page = match Pagination::from_query(query) {
            Ok(page) => page,
            Err(refused) => return refused,
        };
        let link_base = super::support::link_base(route_path, query);
        let repos = self.core.list_repositories(None);
        let body: Vec<Value> = repos
            .iter()
            .filter(|repo| allow(repo))
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
            Ok(repo) => json_response(200, &self.repository_json_with_settings(&repo)),
            Err(err) => error_response(err),
        }
    }

    /// The repository JSON plus the merge settings GitHub reports on a single
    /// repository (`delete_branch_on_merge`). List routes keep the plain shape,
    /// as GitHub's do.
    fn repository_json_with_settings(&self, repo: &Repository) -> Value {
        let mut json = repository_json(repo);
        json["delete_branch_on_merge"] = Value::Bool(
            self.merge_settings
                .delete_branch_on_merge(&repo.id.to_string()),
        );
        json
    }

    /// `PATCH /repos/{owner}/{repo}` — archive, unarchive, rename, or turn
    /// `delete_branch_on_merge` on or off, spelled the way GitHub spells them
    /// (`archived`, `name`, `delete_branch_on_merge`), so `gh` and every GitHub
    /// client already know how to ask. Admin-only; the edge refuses a non-admin
    /// before this runs.
    ///
    /// Exactly one of those fields per request: archiving, renaming and the
    /// merge setting are separate decisions, and a rename of an archived
    /// repository is refused by the core anyway. Every other repository setting
    /// has its own typed Jeryu route, and quietly ignoring an unknown key would
    /// let a caller believe a setting moved when it did not, so an unrecognised
    /// body is a validation error rather than a silent success.
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
        let given = ["archived", "name", "delete_branch_on_merge"]
            .into_iter()
            .filter(|field| fields.contains_key(*field))
            .count();
        if given > 1 {
            return validation(
                "send one of `archived`, `name` or `delete_branch_on_merge`: each is a separate \
                 request",
            );
        }
        if let Some(value) = fields.get("delete_branch_on_merge") {
            let Some(value) = value.as_bool() else {
                return validation("delete_branch_on_merge must be a boolean");
            };
            let current = match self.core.get_repository(owner, repo) {
                Ok(current) => current,
                Err(err) => return error_response(err),
            };
            return match self.merge_settings.set_delete_branch_on_merge(
                &current.id.to_string(),
                value,
                actor,
            ) {
                Ok(()) => json_response(200, &self.repository_json_with_settings(&current)),
                Err(err) => json_response(
                    500,
                    &json!({ "message": format!("could not store delete_branch_on_merge: {err}") }),
                ),
            };
        }
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
                    Ok(repo) => json_response(200, &self.repository_json_with_settings(&repo)),
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
                "no supported field: this route accepts `archived` (a boolean), `name` (a string) \
                 or `delete_branch_on_merge` (a boolean)",
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

/// HLT-022 negative proof: a non-owner must NEVER see another account's private
/// repository, and a bot must NEVER inherit `UserRole::Admin`.
#[cfg(all(test, feature = "web"))]
mod isolation {
    use jeryu_core::{BotKind, BotReach, CreateRepositoryRequest, RepoAccessLevel, UserRole};

    use super::super::GithubRouter;
    use crate::routes::Response;

    #[test]
    fn a_non_owner_and_an_admin_bot_never_see_an_ungranted_private_repo() {
        let core = jeryu_core::ForgeCore::new();
        core.create_account("ada", "ada-password-1", UserRole::Admin)
            .unwrap();
        core.create_account("bea", "bea-password-1", UserRole::User)
            .unwrap();
        core.create_repository("ada", repo("open", false)).unwrap();
        core.create_repository("ada", repo("vault", true)).unwrap();
        core.grant_repo_access("ada", "bea", "ada", "open", RepoAccessLevel::Read)
            .unwrap();
        let bea = core.get_account("bea").unwrap();
        let ada = core.get_account("ada").unwrap();
        let enrolled = core
            .enroll_bot("ada", "atlas", "Atlas", BotKind::Grok, BotReach::General)
            .unwrap();
        let bot = crate::web::bot_token::BotPrincipal {
            bot_id: enrolled.bot.id,
            generation: enrolled.bot.credential_generation,
            key_id: enrolled.key_id,
        };
        let router = GithubRouter::with_core(core);

        let bea_names = names(&router.list_repos_for_account("/repos", &bea));
        assert!(bea_names.contains(&"ada/open".to_string()), "{bea_names:?}");
        assert!(
            !bea_names.iter().any(|name| name == "ada/vault"),
            "non-owner must not see ada/vault: {bea_names:?}"
        );
        let ada_names = names(&router.list_repos_for_account("/repos", &ada));
        assert!(
            ada_names.iter().any(|name| name == "ada/vault"),
            "admin account list still includes the private repository: {ada_names:?}"
        );
        let bot_names = names(&router.list_repos_for_bot("/repos", &bot));
        assert!(bot_names.contains(&"ada/open".to_string()), "{bot_names:?}");
        assert!(
            !bot_names.iter().any(|name| name == "ada/vault"),
            "admin bot must not see ada/vault: {bot_names:?}"
        );
    }

    fn repo(name: &str, private: bool) -> CreateRepositoryRequest {
        CreateRepositoryRequest {
            name: name.to_string(),
            private,
            description: None,
            default_branch: Some("main".to_string()),
        }
    }

    fn names(response: &Response) -> Vec<String> {
        assert_eq!(response.status, 200, "{}", response.body);
        let value: serde_json::Value = serde_json::from_str(&response.body).unwrap();
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["full_name"].as_str().unwrap().to_string())
            .collect()
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
