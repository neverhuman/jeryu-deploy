//! What the queue file cannot say about a done todo: whether its commits are
//! on the base branch, whether production runs them, and which pull request
//! carries them.
//!
//! todoq only writes `merged = true` for a todo another todo waits on, so the
//! file reads `false` for nearly all landed work. The server derives the truth
//! from the repositories it hosts: a commit counts as merged when it is an
//! ancestor of the family's base branch, or, because shift PRs are rebased
//! onto a linear history and get new shas, when a base-branch commit carries
//! the trailer `Todo: <id>`. It counts as released when the merged commit is
//! an ancestor of what production runs.
//!
//! `GET /api/v1/shift/todos` is polled every 30 seconds, so the work is
//! bounded: one `rev-parse` per family repo per request, and per todo only
//! when its base branch or production deployment moved. A todo that is merged
//! stays merged and one that is released stays released without another look.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::super::WebState;
use super::queue::{Queue, git, resolve};
use super::shifts::{find_pr, pr_summary};
use super::types::{ShiftTodo, TodoPr, TodoStatus};

/// How far back the base branch is searched for `Todo:` trailers.
const TRAILER_SCAN_COMMITS: &str = "500";
const PRODUCTION: &str = "production";

#[derive(Clone, Default)]
pub(crate) struct TruthCache {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    todos: HashMap<String, Derived>,
    /// Per bare repo: the base head the scan ran at, and todo id -> commit.
    trailers: HashMap<PathBuf, (String, HashMap<String, String>)>,
}

/// A todo's derived state and the inputs it was derived from.
#[derive(Clone, Debug, Default, PartialEq)]
struct Derived {
    /// `repo=sha@base_head` for every commit: a moved base re-checks the merge.
    merge_inputs: String,
    /// `repo=deployed_sha` for every commit: a new deployment re-checks release.
    release_inputs: String,
    /// Per repo, the commit on the base branch that carries the todo's work.
    landed: BTreeMap<String, String>,
    /// Every commit is on its base branch.
    merged: bool,
    released: Option<bool>,
}

/// What one family repo looks like right now.
struct RepoFacts {
    owner: String,
    path: PathBuf,
    base_head: Option<String>,
    /// The commit of this repo that production runs, when a deployment says.
    deployed: Option<String>,
}

/// The owner a family repo is hosted under: the queue's owner, or, when the
/// family's code lives under another owner (the jain queue is
/// `jain-split/jain-todo` while its repos are `veox/*`), the only hosted
/// repository with that name.
pub(super) fn hosted_owner(state: &WebState, queue: &Queue, name: &str) -> Option<String> {
    if state.repo_manager.open_parts(&queue.owner, name).is_ok() {
        return Some(queue.owner.clone());
    }
    let mut owners = state
        .core
        .list_repositories(None)
        .into_iter()
        .filter(|repo| repo.name == name)
        .map(|repo| repo.owner);
    match (owners.next(), owners.next()) {
        (Some(owner), None) => Some(owner),
        _ => None,
    }
}

fn family_facts(state: &WebState, queue: &Queue) -> BTreeMap<String, RepoFacts> {
    let git_bin = &state.repo_manager.config().git_bin;
    let mut facts = BTreeMap::new();
    let mut payloads = Vec::new();
    for repo in &queue.family.repos {
        let Some(owner) = hosted_owner(state, queue, &repo.name) else {
            continue;
        };
        let Ok(resolved) = state.repo_manager.open_parts(&owner, &repo.name) else {
            continue;
        };
        let base = format!("refs/heads/{}", queue.family.base_branch);
        let current = state
            .core
            .deployment_environments(&owner, &repo.name)
            .ok()
            .into_iter()
            .flatten()
            .find(|env| env.environment == PRODUCTION)
            .and_then(|env| env.current)
            .map(|current| current.deployment);
        if let Some(deployment) = &current {
            payloads.push(deployment.payload.clone());
        }
        facts.insert(
            repo.name.clone(),
            RepoFacts {
                owner,
                base_head: resolve(git_bin, &resolved.path, &base),
                path: resolved.path,
                deployed: current.map(|deployment| deployment.sha),
            },
        );
    }
    // A repo that ships inside another's release (jeryu-web inside
    // jeryu-deploy) is named in that deployment's payload, e.g.
    // `jeryu_web_commit`.
    for (name, fact) in &mut facts {
        if fact.deployed.is_some() {
            continue;
        }
        let key = format!("{}_commit", name.replace('-', "_"));
        fact.deployed = payloads
            .iter()
            .find_map(|payload| payload.get(&key)?.as_str().map(str::to_string));
    }
    facts
}

fn is_ancestor(git_bin: &str, dir: &Path, ancestor: &str, descendant: &str) -> bool {
    git(
        git_bin,
        dir,
        &["merge-base", "--is-ancestor", ancestor, descendant],
        &[],
        None,
    )
    .is_ok()
}

/// Todo id -> the newest base-branch commit carrying `Todo: <id>`.
pub(super) fn scan_trailers(git_bin: &str, dir: &Path, base_head: &str) -> HashMap<String, String> {
    let Ok(out) = git(
        git_bin,
        dir,
        &[
            "log",
            "-n",
            TRAILER_SCAN_COMMITS,
            "--format=%H%x09%(trailers:key=Todo,valueonly,separator=%x2C)",
            base_head,
        ],
        &[],
        None,
    ) else {
        return HashMap::new();
    };
    let mut found = HashMap::new();
    for line in String::from_utf8_lossy(&out).lines() {
        let Some((sha, ids)) = line.split_once('\t') else {
            continue;
        };
        for id in ids.split(',').map(str::trim).filter(|id| !id.is_empty()) {
            found
                .entry(id.to_string())
                .or_insert_with(|| sha.to_string());
        }
    }
    found
}

impl TruthCache {
    /// Fill `merged`, `released` and `pr` on one family's todos.
    pub(crate) fn enrich(&self, state: &WebState, queue: &Queue, todos: &mut [ShiftTodo]) {
        let facts = family_facts(state, queue);
        let git_bin = state.repo_manager.config().git_bin.clone();
        let mut prs: HashMap<(String, String), Option<TodoPr>> = HashMap::new();
        let mut inner = self.inner.lock().expect("shift truth mutex poisoned");
        for todo in todos {
            if !todo.shift.is_empty() {
                todo.prs = todo
                    .commits
                    .keys()
                    .filter_map(|repo| {
                        prs.entry((repo.clone(), todo.shift.clone()))
                            .or_insert_with(|| {
                                let fact = facts.get(repo)?;
                                let base = &queue.family.base_branch;
                                find_pr(state, &fact.owner, repo, &todo.shift, base)
                                    .map(|pr| pr_summary(repo, &pr))
                            })
                            .clone()
                    })
                    .collect();
                todo.pr = todo.prs.first().cloned();
            }
            if todo.status != TodoStatus::Done || todo.commits.is_empty() {
                continue;
            }
            let derived = derive(&mut inner, &git_bin, &facts, todo);
            todo.merged = todo.merged || derived.merged;
            todo.released = derived.released;
        }
    }
}

fn derive(
    inner: &mut Inner,
    git_bin: &str,
    facts: &BTreeMap<String, RepoFacts>,
    todo: &ShiftTodo,
) -> Derived {
    let mut merge_inputs = String::new();
    let mut release_inputs = String::new();
    for (repo, sha) in &todo.commits {
        let fact = facts.get(repo);
        let base = fact.and_then(|f| f.base_head.as_deref()).unwrap_or("-");
        let deployed = fact.and_then(|f| f.deployed.as_deref()).unwrap_or("-");
        merge_inputs.push_str(&format!("{repo}={sha}@{base};"));
        release_inputs.push_str(&format!("{repo}={deployed};"));
    }
    let cached = inner.todos.get(&todo.id).cloned().unwrap_or_default();
    let mut derived = cached.clone();
    // Merged work stays merged; otherwise look again when a base moved.
    if !cached.merged && cached.merge_inputs != merge_inputs {
        derived.landed.clear();
        for (repo, sha) in &todo.commits {
            let Some(fact) = facts.get(repo) else {
                continue;
            };
            let Some(base_head) = &fact.base_head else {
                continue;
            };
            let landed = if is_ancestor(git_bin, &fact.path, sha, base_head) {
                Some(sha.clone())
            } else {
                let scanned = inner
                    .trailers
                    .get(&fact.path)
                    .is_some_and(|(at, _)| at == base_head);
                if !scanned {
                    let found = scan_trailers(git_bin, &fact.path, base_head);
                    inner
                        .trailers
                        .insert(fact.path.clone(), (base_head.clone(), found));
                }
                inner
                    .trailers
                    .get(&fact.path)
                    .and_then(|(_, found)| found.get(&todo.id).cloned())
            };
            if let Some(landed) = landed {
                derived.landed.insert(repo.clone(), landed);
            }
        }
        derived.merged = todo
            .commits
            .keys()
            .all(|repo| derived.landed.contains_key(repo));
    }
    derived.merge_inputs = merge_inputs;
    // Released work stays released; otherwise look again after a deployment
    // or once the merge is known.
    let stale = cached.release_inputs != release_inputs || cached.landed != derived.landed;
    if cached.released != Some(true) && stale {
        let mut known = Vec::new();
        for repo in todo.commits.keys() {
            let Some(fact) = facts.get(repo) else {
                continue;
            };
            let Some(deployed) = &fact.deployed else {
                continue;
            };
            known.push(derived.landed.get(repo).is_some_and(|landed| {
                landed == deployed || is_ancestor(git_bin, &fact.path, landed, deployed)
            }));
        }
        derived.released = (!known.is_empty()).then(|| known.iter().all(|released| *released));
    }
    derived.release_inputs = release_inputs;
    inner.todos.insert(todo.id.clone(), derived.clone());
    derived
}

#[cfg(test)]
mod tests;
