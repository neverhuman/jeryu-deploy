//! Family queues read straight from the bare repos this server hosts.
//!
//! A queue is any hosted repo named `<family>-todo` whose `queue` branch has a
//! `family.toml`. Reads use `git ls-tree`/`cat-file` on the bare repo; writes
//! build a commit with a private index and move `refs/heads/queue` with a
//! compare-and-swap through the protected ref service, retrying a lost race.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use jeryu_gitd::RepoManager;
use jeryu_gitd::refs::RefService;
use serde::Deserialize;

use super::todo_file::TodoFile;
use super::types::FamilyRepo;

pub(crate) const QUEUE_REF: &str = "refs/heads/queue";
const CAS_ATTEMPTS: usize = 5;

/// A family's queue, as discovered on disk.
#[derive(Clone, Debug)]
pub(crate) struct Queue {
    pub owner: String,
    pub repo: String,
    pub path: PathBuf,
    pub head: String,
    pub family: FamilyConfig,
}

impl Queue {
    pub(crate) fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// The parts of `family.toml` the server needs.
#[derive(Clone, Debug)]
pub(crate) struct FamilyConfig {
    pub name: String,
    pub base_branch: String,
    pub landing: String,
    pub shift_tz: String,
    pub bulletshift_prefix: String,
    pub nightshift_prefix: String,
    pub repos: Vec<FamilyRepo>,
}

#[derive(Deserialize)]
struct FamilyToml {
    #[serde(default)]
    family: FamilySection,
    #[serde(default)]
    repo: Vec<FamilyRepo>,
}

#[derive(Default, Deserialize)]
struct FamilySection {
    name: Option<String>,
    base_branch: Option<String>,
    landing: Option<String>,
    shift_tz: Option<String>,
    bulletshift_prefix: Option<String>,
    nightshift_prefix: Option<String>,
}

pub(crate) fn parse_family_toml(text: &str, fallback_name: &str) -> Result<FamilyConfig, String> {
    let parsed: FamilyToml =
        toml::from_str(text).map_err(|err| format!("family.toml is invalid: {err}"))?;
    let section = parsed.family;
    let mut repos = parsed.repo;
    repos.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.name.cmp(&b.name)));
    Ok(FamilyConfig {
        name: section.name.unwrap_or_else(|| fallback_name.to_string()),
        base_branch: section.base_branch.unwrap_or_else(|| "main".to_string()),
        landing: section.landing.unwrap_or_else(|| "batch".to_string()),
        shift_tz: section
            .shift_tz
            .unwrap_or_else(|| "America/Los_Angeles".to_string()),
        bulletshift_prefix: section
            .bulletshift_prefix
            .unwrap_or_else(|| "bulletshift".to_string()),
        nightshift_prefix: section
            .nightshift_prefix
            .unwrap_or_else(|| "nightshift".to_string()),
        repos,
    })
}

/// Run git in a bare repo and return stdout, or stderr as the error.
pub(crate) fn git(
    git_bin: &str,
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let mut command = Command::new(git_bin);
    command
        .args(args)
        .current_dir(dir)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|err| format!("git {}: {err}", args.join(" ")))?;
    if let Some(input) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        pipe.write_all(input)
            .map_err(|err| format!("git {}: {err}", args.join(" ")))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|err| format!("git {}: {err}", args.join(" ")))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Resolve a revision to a commit oid; `None` when it does not exist.
pub(crate) fn resolve(git_bin: &str, dir: &Path, rev: &str) -> Option<String> {
    let spec = format!("{rev}^{{commit}}");
    git(
        git_bin,
        dir,
        &["rev-parse", "--verify", "--quiet", &spec],
        &[],
        None,
    )
    .ok()
    .map(|out| String::from_utf8_lossy(&out).trim().to_string())
    .filter(|oid| !oid.is_empty())
}

fn show_file(git_bin: &str, dir: &Path, commit: &str, path: &str) -> Option<String> {
    let spec = format!("{commit}:{path}");
    git(git_bin, dir, &["cat-file", "blob", &spec], &[], None)
        .ok()
        .map(|out| String::from_utf8_lossy(&out).into_owned())
}

/// Every `<owner>/<family>-todo` repo with a `queue` branch and a `family.toml`.
pub(crate) fn discover(manager: &RepoManager) -> Vec<Queue> {
    let root = &manager.config().storage_root;
    let git_bin = &manager.config().git_bin;
    let mut queues = Vec::new();
    let Ok(owners) = std::fs::read_dir(root) else {
        return queues;
    };
    let mut owners: Vec<_> = owners.flatten().collect();
    owners.sort_by_key(|entry| entry.file_name());
    for owner in owners {
        let Ok(repos) = std::fs::read_dir(owner.path()) else {
            continue;
        };
        let mut repos: Vec<_> = repos.flatten().collect();
        repos.sort_by_key(|entry| entry.file_name());
        for repo in repos {
            let file_name = repo.file_name().to_string_lossy().into_owned();
            let Some(name) = file_name.strip_suffix(".git") else {
                continue;
            };
            let Some(family_name) = name.strip_suffix("-todo") else {
                continue;
            };
            let path = repo.path();
            let Some(head) = resolve(git_bin, &path, QUEUE_REF) else {
                continue;
            };
            let Some(text) = show_file(git_bin, &path, &head, "family.toml") else {
                continue;
            };
            let Ok(family) = parse_family_toml(&text, family_name) else {
                continue;
            };
            queues.push(Queue {
                owner: owner.file_name().to_string_lossy().into_owned(),
                repo: name.to_string(),
                path,
                head,
                family,
            });
        }
    }
    queues
}

/// A todo file on the queue branch.
#[derive(Clone, Debug)]
pub(crate) struct QueuedTodo {
    pub path: String,
    pub todo: TodoFile,
}

/// Read every parseable `todos/*.md` at `commit`. Unparseable files are skipped.
pub(crate) fn read_todos(
    git_bin: &str,
    dir: &Path,
    commit: &str,
) -> Result<Vec<QueuedTodo>, String> {
    let listing = git(
        git_bin,
        dir,
        &["ls-tree", "-z", commit, "todos/"],
        &[],
        None,
    )?;
    let mut entries = Vec::new();
    for record in listing.split(|b| *b == 0) {
        let record = String::from_utf8_lossy(record);
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split_whitespace();
        let (Some(_mode), Some(kind), Some(oid)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if kind == "blob" && path.ends_with(".md") {
            entries.push((path.to_string(), oid.to_string()));
        }
    }
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = String::new();
    for (_, oid) in &entries {
        input.push_str(oid);
        input.push('\n');
    }
    let batch = git(
        git_bin,
        dir,
        &["cat-file", "--batch"],
        &[],
        Some(input.as_bytes()),
    )?;
    let mut todos = Vec::new();
    let mut cursor = 0usize;
    for (path, _) in entries {
        let Some(newline) = batch[cursor..].iter().position(|b| *b == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&batch[cursor..cursor + newline]).into_owned();
        cursor += newline + 1;
        let size: usize = header
            .split_whitespace()
            .nth(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let end = (cursor + size).min(batch.len());
        let text = String::from_utf8_lossy(&batch[cursor..end]).into_owned();
        cursor = end + 1;
        if let Ok(todo) = TodoFile::parse(&text) {
            todos.push(QueuedTodo { path, todo });
        }
    }
    Ok(todos)
}

/// One file to write (`Some`) or delete (`None`) in a queue commit.
pub(crate) type Change = (String, Option<String>);

/// Why a queue write did not happen.
#[derive(Debug)]
pub(crate) enum WriteError<E> {
    /// The caller's mutation refused (validation, not found, ...).
    Rejected(E),
    /// git failed, or every CAS attempt lost its race.
    Git(String),
}

/// Commit `mutate`'s changes on top of the current queue head and move the
/// ref with a compare-and-swap. On a lost race, re-read and re-run `mutate`.
pub(crate) fn commit_change<T, E>(
    manager: &RepoManager,
    queue: &Queue,
    actor: &str,
    message: &str,
    mut mutate: impl FnMut(&[QueuedTodo]) -> Result<(Vec<Change>, T), E>,
) -> Result<T, WriteError<E>> {
    let git_bin = manager.config().git_bin.clone();
    let repository = manager
        .resolve_parts(&queue.owner, &queue.repo)
        .map_err(|err| WriteError::Git(err.to_string()))?;
    let refs = RefService::new(manager.clone());
    let mut last_error = String::new();
    for _ in 0..CAS_ATTEMPTS {
        let old = resolve(&git_bin, &queue.path, QUEUE_REF)
            .ok_or_else(|| WriteError::Git("queue branch vanished".to_string()))?;
        let todos = read_todos(&git_bin, &queue.path, &old).map_err(WriteError::Git)?;
        let (changes, result) = mutate(&todos).map_err(WriteError::Rejected)?;
        let new = build_commit(&git_bin, &queue.path, &old, actor, message, &changes)
            .map_err(WriteError::Git)?;
        match refs.update_ref(
            &repository,
            &format!("shift:{actor}"),
            QUEUE_REF,
            &new,
            Some(&old),
        ) {
            Ok(()) => return Ok(result),
            Err(err) => {
                last_error = err.to_string();
                // Only a moved ref is worth retrying; anything else is final.
                if resolve(&git_bin, &queue.path, QUEUE_REF).as_deref() == Some(old.as_str()) {
                    return Err(WriteError::Git(last_error));
                }
            }
        }
    }
    Err(WriteError::Git(format!(
        "queue write lost {CAS_ATTEMPTS} races in a row: {last_error}"
    )))
}

fn build_commit(
    git_bin: &str,
    dir: &Path,
    parent: &str,
    actor: &str,
    message: &str,
    changes: &[Change],
) -> Result<String, String> {
    let index = std::env::temp_dir().join(format!("jeryu-shift-index-{}", uuid::Uuid::new_v4()));
    let index_str = index.to_string_lossy().into_owned();
    let result = (|| {
        let env = [("GIT_INDEX_FILE", index_str.as_str())];
        git(git_bin, dir, &["read-tree", parent], &env, None)?;
        for (path, contents) in changes {
            match contents {
                Some(text) => {
                    let blob = git(
                        git_bin,
                        dir,
                        &["hash-object", "-w", "--stdin"],
                        &[],
                        Some(text.as_bytes()),
                    )?;
                    let blob = String::from_utf8_lossy(&blob).trim().to_string();
                    let info = format!("100644,{blob},{path}");
                    git(
                        git_bin,
                        dir,
                        &["update-index", "--add", "--cacheinfo", &info],
                        &env,
                        None,
                    )?;
                }
                None => {
                    // `--force-remove` needs a work tree; a zero-mode
                    // `--index-info` record removes the path in a bare repo.
                    let record = format!("0 {}\t{path}\n", "0".repeat(40));
                    git(
                        git_bin,
                        dir,
                        &["update-index", "--index-info"],
                        &env,
                        Some(record.as_bytes()),
                    )?;
                }
            }
        }
        let tree = git(git_bin, dir, &["write-tree"], &env, None)?;
        let tree = String::from_utf8_lossy(&tree).trim().to_string();
        let email = format!("{actor}@jeryu");
        let commit_env = [
            ("GIT_AUTHOR_NAME", actor),
            ("GIT_AUTHOR_EMAIL", email.as_str()),
            ("GIT_COMMITTER_NAME", "jeryu"),
            ("GIT_COMMITTER_EMAIL", "jeryu@localhost"),
        ];
        let commit = git(
            git_bin,
            dir,
            &["commit-tree", &tree, "-p", parent, "-m", message],
            &commit_env,
            None,
        )?;
        Ok(String::from_utf8_lossy(&commit).trim().to_string())
    })();
    let _ = std::fs::remove_file(&index);
    result
}

#[cfg(test)]
mod tests;
