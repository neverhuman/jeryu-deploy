//! The green ref: `refs/green/<default branch>`, the newest commit of a
//! repository's default branch that the gate has proved green.
//!
//! The gate posts `<name>/required` on a pull-request head and on a merge-queue
//! commit, and `<name>/main-health` on the default branch's own head. Nothing
//! posted a status on a default-branch commit that landed by a rebase, so a
//! reader that wanted "the newest proven commit of main" had to resolve the
//! branch head and then hunt for a status of exactly that commit; when the
//! landing rebased, or when the gate had not reached the new tip yet, it found
//! none and waited. The release automation waited there.
//!
//! The green ref answers the question with a ref instead of a search. Every
//! time the gate posts a green `<name>/main-health` for a commit the default
//! branch carries, the forge moves `refs/green/<default branch>` forward under
//! compare-and-swap. Readers (`scripts/release/auto-pin.sh`,
//! `scripts/release/auto-stage.sh`, `GET /api/v1/releases`) resolve that ref
//! and never look at a status again.
//!
//! The decision is made here, on the forge, out of the gate's own status: no
//! file in the tree the gate tests takes part in it, and a reader cannot be
//! talked into treating an ungated commit as green: clients cannot push
//! `refs/green/*` at all, and the shell readers also check that the commit is
//! on main and carries a green `<name>/main-health`. What may post the status is
//! what may move the ref, so the ref is exactly as trustworthy as the
//! `<name>/main-health` context already was -- and the combined status readers
//! used before mixed in every other context anyone had posted on that commit.

use jeryu_gitd::refs::RefService;
use jeryu_gitd::repo::Repository;
use jeryu_gitd::{RepoManager, merge_base};

/// The actor recorded for a green-ref move in the ref audit trail.
const GREEN_REF_ACTOR: &str = "forge/green-ref";

/// The namespace only the forge writes. The release automation trusts what a
/// ref here names, so receive-pack refuses every client command on it
/// (`crate::git_transport`), an administrator's included.
const GREEN_REF_PREFIX: &str = "refs/green/";

/// `refs/green/<default branch>` for a repository whose default branch is
/// `default_branch`.
pub(crate) fn green_ref_name(default_branch: &str) -> String {
    format!("{GREEN_REF_PREFIX}{default_branch}")
}

/// Whether `name` is in the green-ref namespace, which clients cannot push.
pub(crate) fn is_green_ref(name: &str) -> bool {
    name.starts_with(GREEN_REF_PREFIX)
}

/// What one green `main-health` status did to the green ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Advance {
    /// The ref now names `to`; it named `from` before (`None`: it did not exist).
    Moved { from: Option<String>, to: String },
    /// The ref already named this commit.
    Unchanged(String),
    /// The commit is not one the default branch carries, so it is not a verdict
    /// about main and never becomes the green ref.
    NotOnDefaultBranch,
    /// A verdict about a commit the branch has already moved past, arriving
    /// after a newer one: it must not rewind the ref.
    Stale { green: String },
    /// Nothing could be decided (no git data, an unreadable ref, a refused
    /// update, or a compare-and-swap another writer won). The ref stays as it
    /// is and the next green verdict moves it.
    Unavailable(String),
}

/// A full-length hexadecimal object id. A status is posted at a URL path
/// segment, so the sha is checked against this before it reaches git's argv.
fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether `ancestor` is reachable from `descendant`, or is it.
fn is_ancestor(
    git_bin: &str,
    repo: &Repository,
    ancestor: &str,
    descendant: &str,
) -> Result<bool, String> {
    merge_base(git_bin, repo, ancestor, descendant)
        .map(|base| base.as_deref() == Some(ancestor))
        .map_err(|err| err.to_string())
}

/// Move `refs/green/<default_branch>` to `sha`, if that is the newest commit of
/// the default branch the gate has proved green.
///
/// Called for a green `<name>/main-health` status, after it is recorded. The
/// move is a compare-and-swap on the oid the ref was read at
/// ([`RefService::update_ref`]), so two gate slots finishing at once cannot
/// lose one of their verdicts: the loser's update is refused and reported as
/// [`Advance::Unavailable`], leaving a ref the winner already advanced.
///
/// Nothing is recorded before the ref actually moves, so a crash anywhere in
/// here leaves the previous green commit in place -- behind, never wrong -- and
/// the next tip's green verdict advances it.
pub(crate) fn advance(
    manager: &RepoManager,
    owner: &str,
    name: &str,
    default_branch: &str,
    sha: &str,
) -> Advance {
    if !is_object_id(sha) {
        return Advance::Unavailable(format!("{sha} is not a full object id"));
    }
    let repo = match manager.open_parts(owner, name) {
        Ok(repo) => repo,
        Err(err) => return Advance::Unavailable(err.to_string()),
    };
    let git_bin = manager.config().git_bin.clone();
    let refs = RefService::new(manager.clone());
    let head = match refs.resolve_commit(&repo, &format!("refs/heads/{default_branch}")) {
        Ok(Some(head)) => head,
        Ok(None) => return Advance::Unavailable(format!("{default_branch} resolves to nothing")),
        Err(err) => return Advance::Unavailable(err.to_string()),
    };
    let oid = match refs.resolve_commit(&repo, sha) {
        Ok(Some(oid)) => oid,
        Ok(None) => return Advance::Unavailable(format!("{sha} names no commit here")),
        Err(err) => return Advance::Unavailable(err.to_string()),
    };
    match is_ancestor(&git_bin, &repo, &oid, &head) {
        Ok(true) => {}
        Ok(false) => return Advance::NotOnDefaultBranch,
        Err(err) => return Advance::Unavailable(err),
    }
    let green_name = green_ref_name(default_branch);
    let green = match refs.resolve_commit(&repo, &green_name) {
        Ok(green) => green,
        Err(err) => return Advance::Unavailable(err.to_string()),
    };
    if let Some(current) = &green {
        if *current == oid {
            return Advance::Unchanged(oid);
        }
        // A verdict about an older commit of the branch, arriving after a newer
        // one, must not rewind the ref: a reader would stage, or pin, less than
        // what is proved. The one move that is not forward is off a commit the
        // branch no longer carries, which a rewritten history leaves behind;
        // there the green ref has to follow the branch or it never moves again.
        let forward = match is_ancestor(&git_bin, &repo, current, &oid) {
            Ok(forward) => forward,
            Err(err) => return Advance::Unavailable(err),
        };
        if !forward {
            match is_ancestor(&git_bin, &repo, current, &head) {
                Ok(true) => {
                    return Advance::Stale {
                        green: current.clone(),
                    };
                }
                Ok(false) => {}
                Err(err) => return Advance::Unavailable(err),
            }
        }
    }
    match refs.update_ref(&repo, GREEN_REF_ACTOR, &green_name, &oid, green.as_deref()) {
        Ok(()) => Advance::Moved {
            from: green,
            to: oid,
        },
        Err(err) => Advance::Unavailable(err.to_string()),
    }
}

/// The commit `refs/green/<default_branch>` names, or `None` when the gate has
/// not proved this branch green yet (and when the repository has no git data).
pub(crate) fn green_commit(
    manager: &RepoManager,
    owner: &str,
    name: &str,
    default_branch: &str,
) -> Option<String> {
    let repo = manager.open_parts(owner, name).ok()?;
    RefService::new(manager.clone())
        .resolve_commit(&repo, &green_ref_name(default_branch))
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn green_ref_is_named_after_the_default_branch() {
        assert_eq!(green_ref_name("main"), "refs/green/main");
        assert_eq!(green_ref_name("trunk"), "refs/green/trunk");
        assert!(is_green_ref(&green_ref_name("main")));
        assert!(!is_green_ref("refs/heads/refs/green/main"));
        assert!(!is_green_ref("refs/greenish/main"));
    }

    #[test]
    fn only_full_object_ids_reach_git() {
        assert!(is_object_id(&"a".repeat(40)));
        assert!(is_object_id(&"0".repeat(64)));
        assert!(!is_object_id("--help"));
        assert!(!is_object_id("refs/heads/main"));
        assert!(!is_object_id(&"a".repeat(39)));
        assert!(!is_object_id(&format!("{}z", "a".repeat(39))));
    }
}
