//! Per-repository merge settings the forge itself acts on.
//!
//! The one setting today is GitHub's `delete_branch_on_merge`: after a pull
//! request merges, the forge deletes its head branch (see
//! `GithubRouter::delete_merged_head`). It is **on** unless an administrator
//! turns it off for a repository, because a merged branch left behind is noise
//! nothing else cleans up. It is read and written the way GitHub spells it,
//! as a field of `GET` / `PATCH /repos/{owner}/{repo}`, so `gh repo edit
//! --delete-branch-on-merge=false` works unchanged.
//!
//! Values are keyed by repository id, so a rename or transfer keeps them. With
//! a store attached (the web server attaches `<data_dir>/shift.sqlite`, schema
//! from `db/migrations/0013_repo_merge_settings.sql`) they survive a restart;
//! without one — unit tests, embedding callers — they live in this process.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// What a repository uses when nobody has set it.
pub(crate) const DELETE_BRANCH_ON_MERGE_DEFAULT: bool = true;

#[derive(Clone, Debug, Default)]
pub(crate) struct RepoMergeSettings {
    delete_branch_on_merge: Arc<Mutex<HashMap<String, bool>>>,
    #[cfg(feature = "web")]
    store: Option<store::RepoMergeSettingsStore>,
}

impl RepoMergeSettings {
    /// Attaches the durable store at `path` and loads what it holds. Errors
    /// carry the sqlite failure so the caller can refuse to start rather than
    /// run with settings that silently revert to the default.
    #[cfg(feature = "web")]
    pub(crate) fn with_store(path: &std::path::Path) -> Result<Self, String> {
        let store = store::RepoMergeSettingsStore::open(path)?;
        let loaded = store.load()?;
        Ok(Self {
            delete_branch_on_merge: Arc::new(Mutex::new(loaded)),
            store: Some(store),
        })
    }

    /// Whether a merged pull request's head branch is deleted in `repo_id`.
    pub(crate) fn delete_branch_on_merge(&self, repo_id: &str) -> bool {
        self.lock()
            .get(repo_id)
            .copied()
            .unwrap_or(DELETE_BRANCH_ON_MERGE_DEFAULT)
    }

    /// Records the setting for `repo_id`. The durable write comes first, so a
    /// store that cannot be written is reported to the caller instead of
    /// leaving a value that reverts on the next restart.
    pub(crate) fn set_delete_branch_on_merge(
        &self,
        repo_id: &str,
        value: bool,
        actor: &str,
    ) -> Result<(), String> {
        #[cfg(feature = "web")]
        if let Some(store) = &self.store {
            store.set(repo_id, value, actor)?;
        }
        #[cfg(not(feature = "web"))]
        let _ = actor;
        self.lock().insert(repo_id.to_string(), value);
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, bool>> {
        self.delete_branch_on_merge
            .lock()
            .expect("repo merge settings lock")
    }
}

#[cfg(feature = "web")]
mod store {
    //! The sqlite half, in `<data_dir>/shift.sqlite`.

    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use rusqlite::{Connection, params};

    #[derive(Clone)]
    pub(super) struct RepoMergeSettingsStore {
        inner: Arc<Mutex<Connection>>,
    }

    impl std::fmt::Debug for RepoMergeSettingsStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("RepoMergeSettingsStore")
        }
    }

    impl RepoMergeSettingsStore {
        pub(super) fn open(path: &Path) -> Result<Self, String> {
            let conn = Connection::open(path).map_err(|err| err.to_string())?;
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .map_err(|err| err.to_string())?;
            crate::web::shift::migrate_shift_store(&conn)?;
            Ok(Self {
                inner: Arc::new(Mutex::new(conn)),
            })
        }

        pub(super) fn load(&self) -> Result<HashMap<String, bool>, String> {
            let conn = self.lock();
            let mut stmt = conn
                .prepare("SELECT repo_id, delete_branch_on_merge FROM repo_merge_settings")
                .map_err(|err| err.to_string())?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
                })
                .map_err(|err| err.to_string())?;
            rows.collect::<Result<_, _>>()
                .map_err(|err| err.to_string())
        }

        pub(super) fn set(&self, repo_id: &str, value: bool, actor: &str) -> Result<(), String> {
            self.lock()
                .execute(
                    "INSERT INTO repo_merge_settings
                       (repo_id, delete_branch_on_merge, updated_ms, updated_by)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(repo_id) DO UPDATE SET
                       delete_branch_on_merge = excluded.delete_branch_on_merge,
                       updated_ms = excluded.updated_ms,
                       updated_by = excluded.updated_by",
                    params![
                        repo_id,
                        i64::from(value),
                        chrono::Utc::now().timestamp_millis(),
                        actor
                    ],
                )
                .map(|_| ())
                .map_err(|err| err.to_string())
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
            self.inner
                .lock()
                .expect("repo merge settings store mutex poisoned")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_nobody_configured_deletes_merged_branches() {
        let settings = RepoMergeSettings::default();
        assert!(settings.delete_branch_on_merge("repo-1"));
    }

    #[test]
    fn turning_it_off_is_per_repository() {
        let settings = RepoMergeSettings::default();
        settings
            .set_delete_branch_on_merge("repo-1", false, "ada")
            .unwrap();
        assert!(!settings.delete_branch_on_merge("repo-1"));
        assert!(settings.delete_branch_on_merge("repo-2"));
    }

    #[cfg(feature = "web")]
    #[test]
    fn the_setting_survives_reopening_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shift.sqlite");
        RepoMergeSettings::with_store(&path)
            .unwrap()
            .set_delete_branch_on_merge("repo-1", false, "ada")
            .unwrap();
        let reopened = RepoMergeSettings::with_store(&path).unwrap();
        assert!(!reopened.delete_branch_on_merge("repo-1"));
        assert!(reopened.delete_branch_on_merge("repo-2"));
    }
}
