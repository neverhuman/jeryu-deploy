//! Who may work in a family: an account with write access to its code.
//!
//! A person who is not an administrator may take a todo on, or be handed one,
//! only when they can write to a repository the todo names (to any of the
//! family's repositories when it names none). That bounds the people actions
//! to the people already trusted with the code, so an account that only
//! signed up, or that cannot read a private family, cannot touch its queue.

use super::super::WebState;
use super::queue::Queue;
use super::truth::hosted_owner;

/// The `(owner, name)` of each hosted repository the work touches: the
/// todo's own repos, or every family repo when it names none.
pub(crate) fn work_repos(
    state: &WebState,
    queue: &Queue,
    todo_repos: &[String],
) -> Vec<(String, String)> {
    let names: Vec<String> = if todo_repos.is_empty() {
        queue
            .family
            .repos
            .iter()
            .map(|repo| repo.name.clone())
            .collect()
    } else {
        todo_repos.to_vec()
    };
    names
        .into_iter()
        .filter_map(|name| hosted_owner(state, queue, &name).map(|owner| (owner, name)))
        .collect()
}

/// True when `login` may write to at least one of `repos`. An empty list
/// (nothing hosted here) lets nobody in.
pub(crate) fn may_work(state: &WebState, repos: &[(String, String)], login: &str) -> bool {
    repos
        .iter()
        .any(|(owner, name)| state.core.user_can_write_repo(login, owner, name))
}
