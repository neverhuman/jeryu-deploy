//! Who may work on a todo: an account with write access to its code.
//!
//! A person who is not an administrator may take a todo on, or be handed one,
//! only when they can write to a repository the todo names. A todo that names
//! none (an untriaged one) could be about any of the family's repositories, so
//! it needs write access to every one the forge hosts: a grant on one
//! repository never reaches work on a sibling. An account that only signed up,
//! or that cannot read a private family, gets nothing.

use super::super::WebState;
use super::queue::Queue;
use super::truth::hosted_owner;

/// What a login must be able to write to work on a todo.
#[derive(Clone, Debug)]
pub(crate) enum WorkScope {
    /// Any one of the todo's own repositories.
    AnyOf(Vec<(String, String)>),
    /// Every hosted repository of the family (the todo names none).
    AllOf(Vec<(String, String)>),
}

fn hosted(state: &WebState, queue: &Queue, names: Vec<String>) -> Vec<(String, String)> {
    names
        .into_iter()
        .filter_map(|name| hosted_owner(state, queue, &name).map(|owner| (owner, name)))
        .collect()
}

/// The scope of the work a todo describes.
pub(crate) fn work_scope(state: &WebState, queue: &Queue, todo_repos: &[String]) -> WorkScope {
    if todo_repos.is_empty() {
        let names = queue
            .family
            .repos
            .iter()
            .map(|repo| repo.name.clone())
            .collect();
        WorkScope::AllOf(hosted(state, queue, names))
    } else {
        WorkScope::AnyOf(hosted(state, queue, todo_repos.to_vec()))
    }
}

/// True when `login` may work in `scope`. A scope with nothing hosted here
/// lets nobody in.
pub(crate) fn may_work(state: &WebState, scope: &WorkScope, login: &str) -> bool {
    let writes =
        |(owner, name): &(String, String)| state.core.user_can_write_repo(login, owner, name);
    match scope {
        WorkScope::AnyOf(repos) => repos.iter().any(writes),
        WorkScope::AllOf(repos) => !repos.is_empty() && repos.iter().all(writes),
    }
}

/// Of the `(family, todo id)` pairs in `wanted`, the ones `login` may work on,
/// read from each family's current queue. A family or todo the queue no
/// longer holds is left out.
pub(crate) fn workable_todos(
    state: &WebState,
    login: &str,
    wanted: &std::collections::BTreeSet<(String, String)>,
) -> std::collections::BTreeSet<(String, String)> {
    let mut allowed = std::collections::BTreeSet::new();
    for queue in super::queue::discover(&state.repo_manager) {
        if !wanted
            .iter()
            .any(|(family, _)| *family == queue.family.name)
        {
            continue;
        }
        let Ok(todos) = super::queue_todos(state, &queue) else {
            continue;
        };
        for queued in todos {
            let key = (queue.family.name.clone(), queued.todo.id.clone());
            if wanted.contains(&key)
                && may_work(state, &work_scope(state, &queue, &queued.todo.repos), login)
            {
                allowed.insert(key);
            }
        }
    }
    allowed
}
