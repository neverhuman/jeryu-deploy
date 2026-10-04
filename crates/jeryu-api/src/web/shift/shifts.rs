//! Shift branches (`bulletshift/<date>`, `nightshift/<date>`) across a
//! family's repos, and the one-PR-per-repo review of a shift.

use std::collections::BTreeMap;

use jeryu_core::{CreatePullRequestRequest, PullRequest, PullRequestState};

use super::super::WebState;
use super::queue::{Queue, QueuedTodo, git, resolve};
use super::types::{CreatedPr, ShiftBranch, ShiftPr, ShiftRepo};
use crate::github::pulls::pull_request_web_path;

/// `(kind, date)` when `branch` is `<prefix>/<date>` for one of the prefixes.
pub(super) fn classify(queue: &Queue, branch: &str) -> Option<(&'static str, String)> {
    let family = &queue.family;
    for (kind, prefix) in [
        ("bulletshift", &family.bulletshift_prefix),
        ("nightshift", &family.nightshift_prefix),
    ] {
        if let Some(date) = branch
            .strip_prefix(prefix.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            .filter(|d| !d.is_empty())
        {
            return Some((kind, date.to_string()));
        }
    }
    None
}

fn is_open(pr: &PullRequest) -> bool {
    !matches!(
        pr.state,
        PullRequestState::Closed | PullRequestState::Merged
    )
}

fn state_name(state: &PullRequestState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "open".to_string())
}

/// A pull request as a todo links to it.
pub(super) fn pr_summary(repo: &str, pr: &PullRequest) -> super::types::TodoPr {
    super::types::TodoPr {
        repo: repo.to_string(),
        number: pr.number,
        state: state_name(&pr.state),
        url: pull_request_web_path(&pr.owner, &pr.repo, pr.number),
    }
}

/// The PR for `branch` into `base`: the open one if any, else the newest.
pub(super) fn find_pr(
    state: &WebState,
    owner: &str,
    repo: &str,
    branch: &str,
    base: &str,
) -> Option<PullRequest> {
    let mut prs: Vec<PullRequest> = state
        .core
        .list_pull_requests(owner, repo, None)
        .ok()?
        .into_iter()
        .filter(|pr| pr.head.ref_name == branch && pr.base.ref_name == base)
        .collect();
    prs.sort_by_key(|pr| (is_open(pr), pr.number));
    prs.pop()
}

fn ahead_behind(git_bin: &str, dir: &std::path::Path, base: &str, branch: &str) -> (i64, i64) {
    let range = format!("refs/heads/{base}...refs/heads/{branch}");
    git(
        git_bin,
        dir,
        &["rev-list", "--left-right", "--count", &range],
        &[],
        None,
    )
    .ok()
    .and_then(|out| {
        let text = String::from_utf8_lossy(&out).into_owned();
        let mut parts = text.split_whitespace();
        let behind = parts.next()?.parse().ok()?;
        let ahead = parts.next()?.parse().ok()?;
        Some((ahead, behind))
    })
    .unwrap_or((0, 0))
}

/// Todo ids trailed by the commits in `base..branch`.
fn trailers_in_range(
    git_bin: &str,
    dir: &std::path::Path,
    base: &str,
    branch: &str,
) -> std::collections::BTreeSet<String> {
    let range = format!("refs/heads/{base}..refs/heads/{branch}");
    let mut ids = std::collections::BTreeSet::new();
    let Ok(out) = git(
        git_bin,
        dir,
        &[
            "log",
            "-n",
            "200",
            "--format=%(trailers:key=Todo,valueonly,separator=%x2C)",
            &range,
        ],
        &[],
        None,
    ) else {
        return ids;
    };
    for line in String::from_utf8_lossy(&out).lines() {
        for id in line.split(',').map(str::trim).filter(|id| !id.is_empty()) {
            ids.insert(id.to_string());
        }
    }
    ids
}

/// Todo ids carried by commits in `base..branch` that no base commit carries.
/// A linear-history merge replays commits under new shas, so `ahead` alone
/// cannot tell merged work from work that never landed; the trailer can.
pub(super) fn unmerged_todos(
    git_bin: &str,
    dir: &std::path::Path,
    base: &str,
    branch: &str,
) -> Vec<String> {
    let on_base = super::truth::scan_trailers(git_bin, dir, &format!("refs/heads/{base}"));
    trailers_in_range(git_bin, dir, base, branch)
        .into_iter()
        .filter(|id| !on_base.contains_key(id))
        .collect()
}

/// The open pull request, on another head than `branch`, that carries the most
/// of `todos`. When a shift PR hits a queue conflict the operator closes it and
/// opens a replacement from a branch cherry-picked onto the base: those todos
/// are under review there, not waiting for a pull request of their own.
pub(super) fn review_elsewhere(
    git_bin: &str,
    dir: &std::path::Path,
    base: &str,
    branch: &str,
    todos: &[String],
    prs: &[PullRequest],
) -> Option<(ShiftPr, Vec<String>)> {
    prs.iter()
        .filter(|pr| is_open(pr) && pr.base.ref_name == base && pr.head.ref_name != branch)
        .filter_map(|pr| {
            let carried = trailers_in_range(git_bin, dir, base, &pr.head.ref_name);
            let found: Vec<String> = todos
                .iter()
                .filter(|id| carried.contains(id.as_str()))
                .cloned()
                .collect();
            (!found.is_empty()).then(|| {
                (
                    ShiftPr {
                        number: pr.number,
                        state: state_name(&pr.state),
                        url: pull_request_web_path(&pr.owner, &pr.repo, pr.number),
                    },
                    found,
                )
            })
        })
        .max_by_key(|(pr, found)| (found.len(), pr.number))
}

/// Shift branches of one family, newest date first.
pub(crate) fn list(state: &WebState, queue: &Queue, todos: &[QueuedTodo]) -> Vec<ShiftBranch> {
    let git_bin = state.repo_manager.config().git_bin.clone();
    let base = &queue.family.base_branch;
    let mut branches: BTreeMap<String, ShiftBranch> = BTreeMap::new();
    for repo in &queue.family.repos {
        // A family's code may be hosted under another owner than its queue
        // (the jain queue is jain-split/jain-todo, its repos are veox/*).
        let Some(owner) = super::truth::hosted_owner(state, queue, &repo.name) else {
            continue;
        };
        let Ok(repository) = state.repo_manager.open_parts(&owner, &repo.name) else {
            continue;
        };
        let Ok(listing) = git(
            &git_bin,
            &repository.path,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads/",
            ],
            &[],
            None,
        ) else {
            continue;
        };
        for line in String::from_utf8_lossy(&listing).lines() {
            let Some((name, head)) = line.split_once(' ') else {
                continue;
            };
            let Some(branch) = name.strip_prefix("refs/heads/") else {
                continue;
            };
            let Some((kind, date)) = classify(queue, branch) else {
                continue;
            };
            let (ahead, behind) = ahead_behind(&git_bin, &repository.path, base, branch);
            let pr = find_pr(state, &owner, &repo.name, branch, base).map(|pr| ShiftPr {
                number: pr.number,
                state: state_name(&pr.state),
                url: pull_request_web_path(&pr.owner, &pr.repo, pr.number),
            });
            let entry = branches
                .entry(branch.to_string())
                .or_insert_with(|| ShiftBranch {
                    family: queue.family.name.clone(),
                    family_label: crate::web::family::label(&queue.family.name),
                    branch: branch.to_string(),
                    kind: kind.to_string(),
                    date,
                    repos: Vec::new(),
                    todo_ids: todos
                        .iter()
                        .filter(|t| t.todo.shift == branch)
                        .map(|t| t.todo.id.clone())
                        .collect(),
                });
            let unmerged_todos = if ahead > 0 {
                unmerged_todos(&git_bin, &repository.path, base, branch)
            } else {
                Vec::new()
            };
            // Only a closed pull request means the branch was replaced; while
            // its own is open or merged, that one is the review.
            let replaced = pr.as_ref().is_some_and(|pr| pr.state == "closed");
            let (review_pr, reviewed_todos) = if replaced && !unmerged_todos.is_empty() {
                review_elsewhere(
                    &git_bin,
                    &repository.path,
                    base,
                    branch,
                    &unmerged_todos,
                    &state
                        .core
                        .list_pull_requests(&owner, &repo.name, None)
                        .unwrap_or_default(),
                )
                .map_or((None, Vec::new()), |(pr, todos)| (Some(pr), todos))
            } else {
                (None, Vec::new())
            };
            entry.repos.push(ShiftRepo {
                repo: repo.name.clone(),
                head: head.to_string(),
                ahead,
                behind,
                pr,
                unmerged_todos,
                review_pr,
                reviewed_todos,
            });
        }
    }
    branches.into_values().collect()
}

/// The PR body: one line per todo that landed on the shift.
pub(crate) fn pr_body(branch: &str, todos: &[&QueuedTodo]) -> String {
    let mut body = format!(
        "Shift `{branch}`: {} todo(s), opened from the jeryu Shift page.\n\n",
        todos.len()
    );
    for queued in todos {
        let todo = &queued.todo;
        let workers: Vec<String> = todo
            .attempts_list()
            .into_iter()
            .map(|a| a.by)
            .filter(|b| !b.is_empty())
            .collect();
        let worker = workers
            .last()
            .cloned()
            .unwrap_or_else(|| todo.claim_by.clone());
        let commits: Vec<String> = todo
            .commits
            .iter()
            .map(|(repo, sha)| format!("{repo}@{}", sha.get(..10).unwrap_or(sha)))
            .collect();
        body.push_str(&format!(
            "- `{}` {} (requested by {}, worked by {}){}\n",
            todo.id,
            todo.title,
            if todo.requested_by.is_empty() {
                "unknown"
            } else {
                &todo.requested_by
            },
            if worker.is_empty() {
                "unknown"
            } else {
                &worker
            },
            if commits.is_empty() {
                String::new()
            } else {
                format!(": {}", commits.join(", "))
            }
        ));
    }
    body
}

/// Open (or find) the shift PR in every family repo that has `branch`.
pub(crate) fn open_prs(
    state: &WebState,
    queue: &Queue,
    todos: &[QueuedTodo],
    branch: &str,
) -> Result<Vec<CreatedPr>, String> {
    if classify(queue, branch).is_none() {
        return Err(format!(
            "{branch:?} is not a shift branch ({}/<date> or {}/<date>)",
            queue.family.bulletshift_prefix, queue.family.nightshift_prefix
        ));
    }
    let git_bin = state.repo_manager.config().git_bin.clone();
    let base = &queue.family.base_branch;
    let on_shift: Vec<&QueuedTodo> = todos.iter().filter(|t| t.todo.shift == branch).collect();
    let title = format!("{branch}: {} todos", on_shift.len());
    let body = pr_body(branch, &on_shift);
    let author = &state.shift.pr_author;
    let mut out = Vec::new();
    for repo in &queue.family.repos {
        let Some(owner) = super::truth::hosted_owner(state, queue, &repo.name) else {
            continue;
        };
        let Ok(repository) = state.repo_manager.open_parts(&owner, &repo.name) else {
            continue;
        };
        let Some(head_sha) = resolve(&git_bin, &repository.path, &format!("refs/heads/{branch}"))
        else {
            continue;
        };
        if let Some(pr) = find_pr(state, &owner, &repo.name, branch, base)
            && is_open(&pr)
        {
            out.push(CreatedPr {
                repo: repo.name.clone(),
                number: pr.number,
                url: pull_request_web_path(&pr.owner, &pr.repo, pr.number),
                created: false,
            });
            continue;
        }
        let base_sha = resolve(&git_bin, &repository.path, &format!("refs/heads/{base}"));
        let pr = state
            .core
            .create_pull_request(
                &owner,
                &repo.name,
                author,
                CreatePullRequestRequest {
                    title: title.clone(),
                    body: Some(body.clone()),
                    head: branch.to_string(),
                    base: base.clone(),
                    head_sha: Some(head_sha.clone()),
                    base_sha,
                    source_repository: None,
                    draft: false,
                    commits: Vec::new(),
                    changed_files: Vec::new(),
                },
            )
            .map_err(|err| format!("{}: {err}", repo.name))?;
        crate::ci_bridge::seed_pull_request_head(
            &state.core,
            state.repo_manager.as_ref(),
            &owner,
            &repo.name,
            &format!("refs/heads/{}", pr.head.ref_name),
            &pr.head.sha,
            "",
        );
        out.push(CreatedPr {
            repo: repo.name.clone(),
            number: pr.number,
            url: pull_request_web_path(&pr.owner, &pr.repo, pr.number),
            created: true,
        });
    }
    Ok(out)
}
