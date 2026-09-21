//! gitd-backed [`RepoMaterializer`] for the unified `jeryu serve`.
//!
//! When the forge core creates a repository, this materializer also creates the
//! matching bare git repository on disk so clone URLs resolve and `git
//! clone`/`push` work over the mounted smart-HTTP transport. It is idempotent:
//! an already-present bare repository is success, not an error. The same
//! materializer moves the bare repository on a rename or transfer, and
//! [`CoreRedirects`] lets the git transport follow the old name afterwards.

use std::sync::Arc;

use jeryu_core::{ForgeCore, ForgeError, RepoMaterializer, RepoRelocator, Result};
use jeryu_gitd::{RepoId, RepoManager, RepoRedirects};

/// Creates bare git repositories on disk via a shared [`RepoManager`].
#[derive(Debug)]
pub struct GitMaterializer {
    manager: Arc<RepoManager>,
}

impl GitMaterializer {
    /// Wrap a shared [`RepoManager`].
    #[must_use]
    pub fn new(manager: Arc<RepoManager>) -> Self {
        Self { manager }
    }
}

impl RepoMaterializer for GitMaterializer {
    fn materialize(&self, owner: &str, name: &str, _default_branch: &str) -> Result<()> {
        let id = RepoId::new(owner, name).map_err(|err| {
            ForgeError::Validation(format!("invalid repository id {owner}/{name}: {err}"))
        })?;
        // Idempotent: a bare repository that already exists is success.
        let repo = match self.manager.open(&id) {
            Ok(repo) => repo,
            Err(_) => self.manager.create_bare(&id).map_err(|err| {
                ForgeError::Storage(format!("create bare repository {owner}/{name}: {err}"))
            })?,
        };
        self.manager
            .install_pre_receive_hook(&repo)
            .map_err(|err| {
                ForgeError::Storage(format!(
                    "install pre-receive hook for {owner}/{name}: {err}"
                ))
            })?;
        Ok(())
    }
}

impl RepoRelocator for GitMaterializer {
    fn relocate(
        &self,
        from_owner: &str,
        from_name: &str,
        to_owner: &str,
        to_name: &str,
    ) -> Result<()> {
        let repo_id = |owner: &str, name: &str| {
            RepoId::new(owner, name).map_err(|err| {
                ForgeError::Validation(format!("invalid repository id {owner}/{name}: {err}"))
            })
        };
        let from = repo_id(from_owner, from_name)?;
        let to = repo_id(to_owner, to_name)?;
        self.manager.relocate_bare(&from, &to).map_err(|err| {
            ForgeError::Storage(format!("move bare repository {from} to {to}: {err}"))
        })?;
        Ok(())
    }
}

/// Resolves a renamed or transferred repository's old slug to its current one
/// through the forge core, so clone, fetch and push by an old URL keep
/// reaching the moved bare repository.
#[derive(Debug)]
pub struct CoreRedirects {
    core: ForgeCore,
}

impl CoreRedirects {
    /// Follow aliases recorded in `core` (a clone shares its state).
    #[must_use]
    pub fn new(core: ForgeCore) -> Self {
        Self { core }
    }
}

impl RepoRedirects for CoreRedirects {
    fn redirect(&self, owner: &str, name: &str) -> Option<RepoId> {
        let repo = self.core.get_repository(owner, name).ok()?;
        if repo.owner == owner && repo.name == name {
            return None;
        }
        RepoId::new(&repo.owner, &repo.name).ok()
    }
}
