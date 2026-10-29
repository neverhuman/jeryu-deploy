//! Authoritative repository identity for recorded lots.
//!
//! A ledger row keeps the `owner/name` a repository had when the event was
//! recorded. A rename or a transfer moves the repository and the forge keeps
//! the vacated slug as an alias of the same repository id, so the recorded
//! slug still names the same repository even though its own URLs 404. Every
//! read of *current* state for a recorded lot (its pull request's state, its
//! title, its replacement chain) resolves the recorded slug through
//! [`resolve`] first, so a lot that moved is reconciled against the
//! repository it actually lives in.
//!
//! Resolution is the forge's own rule ([`jeryu_core::ForgeCore::get_repository`]):
//! a repository hosted at the recorded slug wins over an alias, so a slug a
//! different repository has since taken resolves to that repository and never
//! to the moved one. A slug nobody hosts and nobody aliases resolves to
//! itself, which keeps the recorded identity, and the lookup that follows
//! finds nothing — the lot stays in recorded work instead of being closed on
//! a guess.

use crate::web::WebState;

/// The `owner/name` the repository recorded as `repo` is hosted under now, or
/// the recorded pair when the slug resolves to nothing.
pub(crate) fn resolve(state: &WebState, repo: &str) -> Option<(String, String)> {
    let (owner, name) = repo.split_once('/')?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(match state.core.get_repository(owner, name) {
        Ok(repo) => (repo.owner, repo.name),
        Err(_) => (owner.to_string(), name.to_string()),
    })
}

/// The current pull request a recorded `repo#number` names, looked up under
/// the repository's current slug.
pub(crate) fn pull(state: &WebState, repo: &str, number: u64) -> Option<jeryu_core::PullRequest> {
    let (owner, name) = resolve(state, repo)?;
    state.core.get_pull_request(&owner, &name, number).ok()
}
