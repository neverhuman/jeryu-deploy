//! Read externally maintained Markdown cards without converting or rewriting them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;

use super::queue::{FamilyConfig, QueuedTodo, git};
use super::todo_file::TodoFile;
use super::types::TodoStatus;

const MAX_BLOB: usize = 1024 * 1024;
const MAX_TOTAL: usize = 32 * MAX_BLOB;

/// Read one immutable queue tree, including receipts and optional PR observations.
/// Symlinks are never treated as card contents or followed outside this tree.
fn blobs(git_bin: &str, dir: &Path, commit: &str) -> Result<BTreeMap<String, String>, String> {
    let listing = git(
        git_bin,
        dir,
        &[
            "ls-tree",
            "-lz",
            commit,
            "tasks/",
            "status/",
            "index.json",
            "tracking/observations.json",
        ],
        &[],
        None,
    )?;
    let mut entries = Vec::new();
    let mut total = 0usize;
    for record in listing
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(record).map_err(|_| "task path is not UTF-8")?;
        let (meta, path) = record.split_once('\t').ok_or("invalid task tree entry")?;
        let parts: Vec<_> = meta.split_whitespace().collect();
        if parts.len() != 4 || parts[1] != "blob" {
            continue;
        }
        if !(path.ends_with(".md") || path == "index.json" || path == "tracking/observations.json")
        {
            continue;
        }
        if parts[0] == "120000" {
            return Err(format!(
                "task input {path} must be a regular Git blob, not a symlink"
            ));
        }
        let size: usize = parts[3]
            .parse()
            .map_err(|_| "invalid task tree blob size")?;
        total = total.checked_add(size).ok_or("task input size overflow")?;
        if size > MAX_BLOB || total > MAX_TOTAL {
            return Err(format!(
                "task input {path} exceeds the bounded card/receipt read budget"
            ));
        }
        entries.push((path.to_string(), parts[2].to_string()));
    }
    let input: String = entries.iter().map(|(_, oid)| format!("{oid}\n")).collect();
    let batch = git(
        git_bin,
        dir,
        &["cat-file", "--batch"],
        &[],
        Some(input.as_bytes()),
    )?;
    let mut cursor = 0usize;
    let mut files = BTreeMap::new();
    for (path, _) in entries {
        let remaining = batch.get(cursor..).ok_or("truncated task blob batch")?;
        let newline = remaining
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or("missing task blob header")?;
        let header =
            std::str::from_utf8(&remaining[..newline]).map_err(|_| "invalid task blob header")?;
        let size = header
            .split_whitespace()
            .nth(2)
            .and_then(|size| size.parse::<usize>().ok())
            .ok_or("invalid task blob size")?;
        if size > MAX_BLOB {
            return Err(format!(
                "task input {path} exceeds the bounded card/receipt read budget"
            ));
        }
        cursor += newline + 1;
        let end = cursor.checked_add(size).ok_or("task blob size overflow")?;
        let text = std::str::from_utf8(batch.get(cursor..end).ok_or("truncated task blob")?)
            .map_err(|_| format!("task input {path} is not UTF-8"))?;
        if batch.get(end) != Some(&b'\n') {
            return Err("task blob batch delimiter missing".to_string());
        }
        files.insert(path, text.to_string());
        cursor = end + 1;
    }
    Ok(files)
}

/// An ID in existing cards: three capital letters, two digits, optional Fnn.
fn task_id(word: &str) -> Option<String> {
    let word = word
        .strip_prefix('T')
        .and_then(|rest| {
            let (rank, rest) = rest.split_once('-')?;
            rank.bytes()
                .all(|byte| byte.is_ascii_digit())
                .then_some(rest)
        })
        .unwrap_or(word);
    let mut parts = word.split('-');
    let family = parts.next()?;
    let number = parts.next()?;
    if family.len() != 3
        || !family.bytes().all(|byte| byte.is_ascii_uppercase())
        || number.len() != 2
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let mut id = format!("{family}-{number}");
    if let Some(followup) = parts.next()
        && followup.len() == 3
        && followup.starts_with('F')
        && followup[1..].bytes().all(|byte| byte.is_ascii_digit())
    {
        id.push('-');
        id.push_str(followup);
    }
    Some(id)
}

fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let mut columns = line.strip_prefix('|')?.split('|').map(str::trim);
        (columns.next()? == key).then(|| columns.next()).flatten()
    })
}

fn dependencies(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-')
        .filter_map(task_id)
        .collect()
}

fn native_id(family: &str, task: &str) -> String {
    format!("{family}-{}", task.to_ascii_lowercase())
}

#[derive(Default, Deserialize)]
struct IndexEntry {
    #[serde(default)]
    title: String,
    #[serde(default)]
    deps: String,
    #[serde(default)]
    crit: bool,
    #[serde(default)]
    wave: String,
}

#[derive(Deserialize)]
struct Observation {
    task: String,
    status: String,
    progress: String,
    #[serde(default)]
    prs: Vec<ObservedPr>,
}

#[derive(Deserialize)]
struct ObservedPr {
    state: String,
    url: String,
    #[serde(rename = "headRefOid")]
    head: String,
}

#[derive(Deserialize)]
struct Observations {
    observed_at: String,
    tasks: Vec<Observation>,
}

pub(super) fn read(
    git_bin: &str,
    dir: &Path,
    commit: &str,
    config: &FamilyConfig,
) -> Result<Vec<QueuedTodo>, String> {
    let files = blobs(git_bin, dir, commit)?;
    let repo = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .trim_end_matches(".git");
    let owner = dir
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let source = format!("/repos/jeryu/{owner}/{repo}/blob/{commit}/");
    let index: BTreeMap<String, IndexEntry> = files
        .get("index.json")
        .map(|text| serde_json::from_str(text).map_err(|error| format!("task index.json: {error}")))
        .transpose()?
        .unwrap_or_default();
    let observations: Option<Observations> = files
        .get("tracking/observations.json")
        .map(|text| {
            serde_json::from_str(text).map_err(|error| format!("task observations: {error}"))
        })
        .transpose()?;
    let mut seen = BTreeSet::new();
    let mut cards = Vec::new();
    for (path, text) in files.iter().filter(|(path, _)| path.starts_with("tasks/")) {
        let filename = path.trim_start_matches("tasks/").trim_end_matches(".md");
        let id = task_id(filename)
            .ok_or_else(|| format!("task card {path}: unrecognized filename ID"))?;
        if !seen.insert(id.clone()) {
            return Err(format!("task card {path}: duplicate ID {id}"));
        }
        let heading = text
            .lines()
            .find_map(|line| line.strip_prefix("# "))
            .ok_or_else(|| format!("task card {path}: missing heading"))?;
        let meta = index.get(&id);
        let title = meta
            .filter(|meta| !meta.title.is_empty())
            .map(|meta| meta.title.as_str())
            .unwrap_or(heading);
        let title = title
            .strip_prefix(&id)
            .map(|title| title.trim_start_matches([' ', '—', '·', '-']))
            .filter(|title| !title.is_empty())
            .unwrap_or(title);
        let mut todo = TodoFile::new(
            native_id(&config.name, &id),
            config.name.clone(),
            format!("[{id}] {title}"),
        );
        todo.repos = config.repos.iter().map(|repo| repo.name.clone()).collect();
        todo.priority = if meta.is_some_and(|meta| meta.crit) {
            1
        } else if meta.is_some_and(|meta| meta.wave == "W0") {
            2
        } else {
            3
        };
        let deps = field(text, "Depends on")
            .or_else(|| meta.map(|meta| meta.deps.as_str()))
            .unwrap_or_else(|| {
                text.lines()
                    .take(8)
                    .find_map(|line| {
                        line.split_once("Depends on ")
                            .or_else(|| line.split_once("deps "))
                            .map(|(_, deps)| deps)
                    })
                    .unwrap_or("")
            });
        todo.blocked_by = dependencies(deps)
            .into_iter()
            .filter(|dep| dep != &id)
            .map(|dep| native_id(&config.name, &dep))
            .collect();
        let receipt_path = format!("status/{id}.md");
        let receipt = files.get(&receipt_path);
        let status = receipt
            .and_then(|text| {
                text.lines()
                    .take(8)
                    .find(|line| line.contains("Status:"))
                    .or_else(|| {
                        text.lines().take(8).find(|line| {
                            line.trim_start_matches([' ', '*', '`'])
                                .to_ascii_lowercase()
                                .starts_with("blocked:")
                        })
                    })
                    .or_else(|| {
                        text.lines()
                            .next()
                            .filter(|line| line.to_ascii_lowercase().contains("blocked"))
                    })
            })
            .or_else(|| field(text, "Status"))
            .unwrap_or("unknown");
        let normalized = status.to_ascii_lowercase();
        todo.status = if normalized.contains("blocked") {
            TodoStatus::Blocked
        } else if normalized.contains("ready_for_review")
            || normalized.contains("merged")
            || normalized.contains("done")
        {
            TodoStatus::Handoff
        } else if normalized.contains("claimed") {
            TodoStatus::Claimed
        } else {
            TodoStatus::Open
        };
        todo.claim_by = field(text, "Assigned screen / host")
            .unwrap_or("")
            .to_string();
        todo.note = format!("Externally managed; source status: {status}");
        let mut evidence = String::new();
        if let Some(snapshot) = &observations
            && let Some(observation) = snapshot.tasks.iter().find(|entry| entry.task == id)
        {
            // Snapshots are explicitly dated evidence, not live external API calls.
            let open = observation.prs.iter().any(|pr| pr.state == "OPEN");
            let all_merged = !observation.prs.is_empty()
                && observation.prs.iter().all(|pr| pr.state == "MERGED");
            if open {
                todo.status = TodoStatus::Handoff;
            } else if all_merged && observation.status == "done" {
                todo.status = TodoStatus::Done;
                todo.merged = true;
            } else if all_merged {
                todo.status = TodoStatus::Handoff;
            }
            todo.note.push_str(&format!(
                "; PR snapshot {}: {}",
                snapshot.observed_at, observation.progress
            ));
            evidence.push_str(&format!("\n\n## PR observations\n\nObserved {} (refresh the committed snapshot for newer PR state).\n", snapshot.observed_at));
            for pr in &observation.prs {
                // Never turn an untrusted URL into an arbitrary Markdown destination.
                if pr.url.starts_with("https://github.com/")
                    && pr
                        .url
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ":/._-".contains(ch))
                {
                    evidence.push_str(&format!(
                        "\n- [{}]({}) — {}; head `{}`",
                        pr.url, pr.url, pr.state, pr.head
                    ));
                }
            }
        }
        let receipt_link = if receipt.is_some() {
            format!(" · [Worker receipt]({source}{receipt_path})")
        } else {
            String::new()
        };
        todo.body = format!(
            "[Master plan]({source}master_plan.md) · [Original card]({source}{path}){receipt_link}\n\n{}\n\n{text}{evidence}",
            todo.note
        );
        cards.push(QueuedTodo {
            path: path.clone(),
            todo,
        });
    }
    if cards.is_empty() {
        return Err("task_cards=true but no Markdown cards were found in tasks/".to_string());
    }
    for card in &cards {
        for dep in &card.todo.blocked_by {
            if !cards.iter().any(|candidate| &candidate.todo.id == dep) {
                return Err(format!(
                    "task card {}: dependency {dep} has no card",
                    card.path
                ));
            }
        }
    }
    Ok(cards)
}

#[cfg(test)]
mod tests;
