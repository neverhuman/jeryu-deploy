//! Instance-wide settings an administrator sets from the web UI, kept in
//! `<data_dir>/shift.sqlite` (`site_settings`, `db/migrations/0005_site_settings.sql`).
//!
//! The one key today is `internal_wiki`: the repository the left navigation
//! links to as the instance's wiki. It is stored by repository id, so a rename
//! or move keeps it, and resolved on every read, so a deleted repository reads
//! as unset instead of leaving a dead link.
//!
//! - `GET /api/v1/site-settings`: what the navigation needs, for any caller.
//!   The wiki is reported only to a caller who may read that repository, so a
//!   private wiki's name never reaches an account without access.
//! - `GET /api/v1/admin/site-settings`: the stored values and who set them.
//! - `PUT /api/v1/admin/site-settings`: `{"internal_wiki": "owner/name" | null}`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use jeryu_core::{AccountSummary, Repository, UserRole};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::auth::forbidden;
use super::shift::{migrate_shift_store, rfc3339_ms};
use super::{WebState, api_error};

const INTERNAL_WIKI: &str = "internal_wiki";

/// One stored setting and its provenance.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StoredSetting {
    pub value: String,
    pub updated_ms: i64,
    pub updated_by: String,
}

#[derive(Clone)]
pub(crate) struct SiteSettingsStore {
    inner: Arc<Mutex<Connection>>,
}

impl SiteSettingsStore {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|err| err.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|err| err.to_string())?;
        migrate_shift_store(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    pub(crate) fn get(&self, key: &str) -> Result<Option<StoredSetting>, String> {
        let conn = self.inner.lock().expect("site settings mutex poisoned");
        conn.query_row(
            "SELECT value, updated_ms, updated_by FROM site_settings WHERE key = ?1",
            params![key],
            |row| {
                Ok(StoredSetting {
                    value: row.get(0)?,
                    updated_ms: row.get(1)?,
                    updated_by: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(|err| err.to_string())
    }

    /// Store `value` under `key`, or remove the key when `value` is `None`.
    pub(crate) fn set(
        &self,
        key: &str,
        value: Option<&str>,
        updated_by: &str,
        now_ms: i64,
    ) -> Result<(), String> {
        let conn = self.inner.lock().expect("site settings mutex poisoned");
        match value {
            Some(value) => conn.execute(
                "INSERT INTO site_settings (key, value, updated_ms, updated_by)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key) DO UPDATE SET
                   value = excluded.value,
                   updated_ms = excluded.updated_ms,
                   updated_by = excluded.updated_by",
                params![key, value, now_ms, updated_by],
            ),
            None => conn.execute("DELETE FROM site_settings WHERE key = ?1", params![key]),
        }
        .map(|_| ())
        .map_err(|err| err.to_string())
    }
}

/// The wiki repository as the navigation and the wiki reader address it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct WikiRepository {
    pub id: String,
    pub host: String,
    pub owner: String,
    pub name: String,
    pub full_name: String,
    pub default_branch: String,
    pub private: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct SiteSettingsResponse {
    pub internal_wiki: Option<WikiRepository>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AdminSiteSettingsResponse {
    /// The configured wiki, or `None` when unset or its repository is gone.
    pub internal_wiki: Option<WikiRepository>,
    /// True when a repository id is stored but no longer resolves.
    pub internal_wiki_missing: bool,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AdminSiteSettingsUpdate {
    /// `owner/name` of the repository to use, or `null` to clear it.
    pub internal_wiki: Option<String>,
}

fn wiki_repository(repo: &Repository) -> WikiRepository {
    WikiRepository {
        id: repo.id.to_string(),
        host: "jeryu".to_string(),
        owner: repo.owner.clone(),
        name: repo.name.clone(),
        full_name: repo.full_name.clone(),
        default_branch: repo.default_branch.clone(),
        private: repo.private,
    }
}

fn find_by_id(state: &WebState, id: &str) -> Option<Repository> {
    state
        .core
        .list_repositories(None)
        .into_iter()
        .find(|repo| repo.id.to_string() == id)
}

fn can_read(state: &WebState, account: &AccountSummary, repo: &Repository) -> bool {
    account.role == UserRole::Admin
        || !repo.private
        || state
            .core
            .user_can_read_repo(&account.login, &repo.owner, &repo.name)
}

fn storage_error(error: &str) -> AxumResponse {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "storage_failed",
        &format!("could not read site settings: {error}"),
    )
}

fn admin_view(state: &WebState) -> Result<AdminSiteSettingsResponse, String> {
    let stored = state.site_settings.get(INTERNAL_WIKI)?;
    let repo = stored
        .as_ref()
        .and_then(|setting| find_by_id(state, &setting.value));
    Ok(AdminSiteSettingsResponse {
        internal_wiki_missing: stored.is_some() && repo.is_none(),
        internal_wiki: repo.as_ref().map(wiki_repository),
        updated_by: stored.as_ref().map(|setting| setting.updated_by.clone()),
        updated_at: stored
            .as_ref()
            .map(|setting| rfc3339_ms(setting.updated_ms)),
    })
}

pub(crate) async fn get_site_settings(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> AxumResponse {
    let stored = match state.site_settings.get(INTERNAL_WIKI) {
        Ok(stored) => stored,
        Err(error) => return storage_error(&error),
    };
    let internal_wiki = stored
        .and_then(|setting| find_by_id(&state, &setting.value))
        .filter(|repo| can_read(&state, &account, repo))
        .map(|repo| wiki_repository(&repo));
    Json(SiteSettingsResponse { internal_wiki }).into_response()
}

pub(crate) async fn admin_get_site_settings(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> AxumResponse {
    if account.role != UserRole::Admin {
        return forbidden("admin role required");
    }
    match admin_view(&state) {
        Ok(view) => Json(view).into_response(),
        Err(error) => storage_error(&error),
    }
}

pub(crate) async fn admin_put_site_settings(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Json(update): Json<AdminSiteSettingsUpdate>,
) -> AxumResponse {
    if account.role != UserRole::Admin {
        return forbidden("admin role required");
    }
    let repo_id = match update
        .internal_wiki
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        None => None,
        Some(full_name) => {
            let Some((owner, name)) = full_name
                .split_once('/')
                .filter(|(owner, name)| !owner.is_empty() && !name.is_empty())
            else {
                return api_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_input",
                    "internal_wiki must be owner/name or null",
                );
            };
            match state.core.get_repository(owner, name) {
                Ok(repo) => Some(repo.id.to_string()),
                Err(_) => {
                    return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
                }
            }
        }
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    if let Err(error) =
        state
            .site_settings
            .set(INTERNAL_WIKI, repo_id.as_deref(), &account.login, now_ms)
    {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("could not save site settings: {error}"),
        );
    }
    match admin_view(&state) {
        Ok(view) => Json(view).into_response(),
        Err(error) => storage_error(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_and_clear_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SiteSettingsStore::open(&dir.path().join("shift.sqlite")).unwrap();
        assert_eq!(store.get(INTERNAL_WIKI).unwrap(), None);

        store
            .set(INTERNAL_WIKI, Some("repo-1"), "admin", 10)
            .unwrap();
        store
            .set(INTERNAL_WIKI, Some("repo-2"), "other-admin", 20)
            .unwrap();
        assert_eq!(
            store.get(INTERNAL_WIKI).unwrap(),
            Some(StoredSetting {
                value: "repo-2".into(),
                updated_ms: 20,
                updated_by: "other-admin".into(),
            })
        );

        store.set(INTERNAL_WIKI, None, "admin", 30).unwrap();
        assert_eq!(store.get(INTERNAL_WIKI).unwrap(), None);
    }

    #[test]
    fn a_reopened_store_keeps_its_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shift.sqlite");
        SiteSettingsStore::open(&path)
            .unwrap()
            .set(INTERNAL_WIKI, Some("repo-1"), "admin", 10)
            .unwrap();
        let reopened = SiteSettingsStore::open(&path).unwrap();
        assert_eq!(
            reopened.get(INTERNAL_WIKI).unwrap().unwrap().value,
            "repo-1"
        );
    }
}
