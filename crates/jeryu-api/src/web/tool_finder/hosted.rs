//! Scan sources read straight from the hosted bare repositories.
//!
//! The tool-finder was written against a host that keeps a working-tree
//! checkout of every split repo beside its family manifest. The hosted forge
//! keeps bare repositories only, so that discovery finds nothing there. This
//! module gives the scan a second source: it discovers split families from the
//! hosted repositories themselves — the same way `pipeline::pins` discovers
//! consumers — and materializes each member's default branch into a scratch
//! directory the scanner can walk.
//!
//! A repository names a family when its default branch has a root
//! `repos.manifest.toml` — the family authority, whose `required_repos` lists
//! every member — or a root `*-split.lock.toml`, whose `[[repo]]` entries name
//! them. The named members are resolved back to hosted repositories, so only
//! repositories this forge actually serves are ever read.
//!
//! Everything here is bounded: a repository cap, a per-file, per-repo and
//! whole-scan byte cap, a file-count cap, and a wall-clock deadline. Whatever a
//! bound drops is reported as a [`SkippedRepo`] rather than silently lost, and
//! the scratch directory is removed when the [`ScanWorkspace`] drops.
//!
//! Visibility: private repositories ARE materialized, because the scan reads
//! them as the server, not as a caller. That is only safe while every
//! `/api/v1/tool-finder/` route stays global-admin-only (`admin_only_path` in
//! `super::super::auth`) — findings name files and line ranges from every repo
//! in the family. Do not widen those routes without first splitting results
//! per reader.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jeryu_core::Repository;
use serde::Serialize;

use super::super::WebState;
use super::super::pipeline::pins::hosted_repository;
use super::super::shift::run_git as git;

/// Root file whose `required_repos` names a family's members.
const FAMILY_MANIFEST: &str = "repos.manifest.toml";
/// Root file suffix whose `[[repo]]` entries name a family's members.
const LOCK_SUFFIX: &str = "-split.lock.toml";

/// Most repositories one scan will materialize.
const MAX_REPOS: usize = 64;
/// Largest blob worth fingerprinting; bigger files are generated or data.
const MAX_FILE_BYTES: u64 = 512 * 1024;
/// Most bytes one repository may contribute.
const MAX_REPO_BYTES: u64 = 64 * 1024 * 1024;
/// Most bytes one scan may write to the scratch directory.
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// Most files one scan may write.
const MAX_TOTAL_FILES: usize = 200_000;
/// Wall clock the whole materialization may take.
const MATERIALIZE_DEADLINE: Duration = Duration::from_secs(300);
/// Blobs fetched per `git cat-file --batch` call, so peak memory stays a
/// fraction of the per-repo byte cap.
const BATCH_FILES: usize = 256;

/// One repository the scan read, and where its sources came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ScanSourceRepo {
    /// `owner/name` of the hosted repository.
    pub repo: String,
    pub family: Option<String>,
    /// The branch the sources were read from; `None` for a working-tree root.
    pub branch: Option<String>,
    /// The commit the sources were read at; `None` for a working-tree root.
    pub commit: Option<String>,
    pub private: bool,
}

/// One repository (or part of one) the scan did not read, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SkippedRepo {
    /// `owner/name`, or the family name for a whole-family skip.
    pub repo: String,
    /// A stable slug: `unreadable`, `no_default_branch`, `no_sources`,
    /// `repo_budget`, `byte_budget`, `file_budget`, `deadline`, `write_failed`.
    pub reason: &'static str,
    /// One sentence a reader can act on.
    pub detail: String,
}

impl SkippedRepo {
    fn new(repo: impl Into<String>, reason: &'static str, detail: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            reason,
            detail: detail.into(),
        }
    }
}

/// What hosted discovery found, before anything is materialized.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostedFamilies {
    /// Family members, in `owner/name` order.
    pub repos: Vec<ScanSourceRepo>,
    pub skipped: Vec<SkippedRepo>,
}

/// Why hosted sources could not be prepared.
#[derive(Debug)]
pub(crate) enum HostedSourceError {
    /// No hosted repository's default branch names a split family.
    NotConfigured,
    /// The scratch directory could not be created.
    Workspace(std::io::Error),
}

impl std::fmt::Display for HostedSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => formatter.write_str(
                "no hosted repository's default branch has a root repos.manifest.toml \
                 or *-split.lock.toml, so no split family is configured on this server",
            ),
            Self::Workspace(error) => {
                write!(formatter, "create the scan scratch directory: {error}")
            }
        }
    }
}

/// A scratch directory holding one materialized tree per repository. Dropping
/// it removes the directory, so a scan never leaves disk behind.
pub(crate) struct ScanWorkspace {
    root: PathBuf,
    /// `(repo_id, tree_root)` pairs, ready for the codegraph scanner.
    pub roots: Vec<(String, PathBuf)>,
    pub repos: Vec<ScanSourceRepo>,
    pub skipped: Vec<SkippedRepo>,
}

impl Drop for ScanWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Root-tree entry names of a branch, non-recursive.
fn root_entries(git_bin: &str, dir: &Path, branch: &str) -> Vec<String> {
    let tree = format!("refs/heads/{branch}");
    git(git_bin, dir, &["ls-tree", "--name-only", &tree], &[], None)
        .map(|out| {
            String::from_utf8_lossy(&out)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn blob(git_bin: &str, dir: &Path, branch: &str, path: &str) -> Option<String> {
    let spec = format!("refs/heads/{branch}:{path}");
    git(git_bin, dir, &["cat-file", "blob", &spec], &[], None)
        .ok()
        .map(|out| String::from_utf8_lossy(&out).into_owned())
}

/// The member names a family file lists.
///
/// The authority manifest names them in `required_repos`, with the control
/// plane also under `[control_plane]` and every other member a `[[repo]]` row.
/// A release lock names them in its `[[repo]]` rows only. Reading all three
/// covers both files, and a name no repository here answers to is reported as
/// a skip rather than dropped.
pub(crate) fn member_names(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Ok(table) = text.parse::<toml::Table>() else {
        return names;
    };
    let mut insert = |name: &str| {
        let name = name.trim();
        if !name.is_empty() {
            names.insert(name.rsplit('/').next().unwrap_or(name).to_string());
        }
    };
    for required in table
        .get("required_repos")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
    {
        insert(required);
    }
    if let Some(control) = table
        .get("control_plane")
        .and_then(toml::Value::as_table)
        .and_then(|control| control.get("name"))
        .and_then(toml::Value::as_str)
    {
        insert(control);
    }
    for entry in table
        .get("repo")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        let field = |key: &str| entry.get(key).and_then(toml::Value::as_str);
        if let Some(name) = field("name").or_else(|| field("jeryu_slug")) {
            insert(name);
        }
    }
    names
}

/// The family a root file belongs to: the manifest's own `repo_family`, else
/// the forge's grouping for the repository, else the lock file's own prefix.
fn family_name(repo: &Repository, text: &str, file: &str) -> Option<String> {
    text.parse::<toml::Table>()
        .ok()
        .and_then(|table| {
            table
                .get("repo_family")
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| repo.family.clone())
        .or_else(|| {
            file.strip_suffix(LOCK_SUFFIX)
                .map(|prefix| format!("{prefix}-split"))
        })
        .filter(|family| !family.trim().is_empty())
}

/// Every hosted repository that belongs to a split family named by some hosted
/// repository's default branch.
pub(crate) fn discover(state: &WebState) -> HostedFamilies {
    let git_bin = &state.repo_manager.config().git_bin;
    let all = state.core.list_repositories(None);
    let active: Vec<&Repository> = all
        .iter()
        .filter(|repo| !repo.archived && !repo.disabled)
        .collect();
    // `owner/name` -> family. A repository named by two families keeps the
    // first in `owner/name` order, so the answer does not depend on listing
    // order.
    let mut members: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut skipped: Vec<SkippedRepo> = Vec::new();
    let mut claim = |repo: &Repository, family: Option<String>| {
        members
            .entry(repo.full_name.clone())
            .or_insert_with(|| family);
    };

    for repo in &active {
        let Ok(opened) = state.repo_manager.open_parts(&repo.owner, &repo.name) else {
            continue;
        };
        let files = root_entries(git_bin, &opened.path, &repo.default_branch);
        let family_files: Vec<&String> = files
            .iter()
            .filter(|file| *file == FAMILY_MANIFEST || file.ends_with(LOCK_SUFFIX))
            .collect();
        if family_files.is_empty() {
            continue;
        }
        for file in family_files {
            let Some(text) = blob(git_bin, &opened.path, &repo.default_branch, file) else {
                skipped.push(SkippedRepo::new(
                    &repo.full_name,
                    "unreadable",
                    format!("{file} on {} could not be read", repo.default_branch),
                ));
                continue;
            };
            let family = family_name(repo, &text, file);
            claim(repo, family.clone());
            for name in member_names(&text) {
                match hosted_repository(&all, &repo.owner, &name) {
                    Some(member) if !member.archived && !member.disabled => {
                        claim(member, family.clone());
                    }
                    Some(member) => skipped.push(SkippedRepo::new(
                        &member.full_name,
                        "unreadable",
                        format!(
                            "named by {file} in {} but archived or disabled",
                            repo.full_name
                        ),
                    )),
                    None => skipped.push(SkippedRepo::new(
                        name,
                        "unreadable",
                        format!("named by {file} in {} but not hosted here", repo.full_name),
                    )),
                }
            }
        }
    }

    let by_full_name: BTreeMap<&str, &Repository> = active
        .iter()
        .map(|repo| (repo.full_name.as_str(), *repo))
        .collect();
    let repos = members
        .into_iter()
        .filter_map(|(full_name, family)| {
            let repo = by_full_name.get(full_name.as_str())?;
            Some(ScanSourceRepo {
                repo: full_name,
                family: family.or_else(|| repo.family.clone()),
                branch: Some(repo.default_branch.clone()),
                commit: None,
                private: repo.private,
            })
        })
        .collect();
    HostedFamilies { repos, skipped }
}

/// Budgets shared by every repository in one materialization.
struct Budget {
    deadline: Instant,
    bytes_left: u64,
    files_left: usize,
}

impl Budget {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + MATERIALIZE_DEADLINE,
            bytes_left: MAX_TOTAL_BYTES,
            files_left: MAX_TOTAL_FILES,
        }
    }

    fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

/// One blob to write out.
struct Blob {
    oid: String,
    rel: String,
    size: u64,
}

/// Reject anything that could write outside the destination tree. Git does not
/// produce such paths, but the scan writes them, so it checks.
fn safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && Path::new(rel)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(name) if name != ".git"))
}

/// The blobs of a branch that are worth writing, plus the count dropped for
/// being too large, a symlink, or a submodule.
fn listed_blobs(git_bin: &str, dir: &Path, branch: &str) -> (Vec<Blob>, usize) {
    let tree = format!("refs/heads/{branch}");
    let Ok(out) = git(
        git_bin,
        dir,
        &["ls-tree", "-r", "-l", "-z", &tree],
        &[],
        None,
    ) else {
        return (Vec::new(), 0);
    };
    let text = String::from_utf8_lossy(&out).into_owned();
    let mut blobs = Vec::new();
    let mut dropped = 0usize;
    for record in text.split('\0').filter(|record| !record.is_empty()) {
        let Some((meta, rel)) = record.split_once('\t') else {
            continue;
        };
        let mut fields = meta.split_whitespace();
        let (Some(mode), Some(kind), Some(oid), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if kind != "blob" || mode == "120000" || !safe_rel(rel) {
            dropped += 1;
            continue;
        }
        let Ok(size) = size.parse::<u64>() else {
            dropped += 1;
            continue;
        };
        if size > MAX_FILE_BYTES {
            dropped += 1;
            continue;
        }
        blobs.push(Blob {
            oid: oid.to_string(),
            rel: rel.to_string(),
            size,
        });
    }
    (blobs, dropped)
}

/// Write one batch of blobs with a single `git cat-file --batch`. The stream is
/// `<oid> <kind> <size>\n<contents>\n` per request line.
fn write_batch(git_bin: &str, dir: &Path, dest: &Path, batch: &[Blob]) -> Result<(), String> {
    let stdin = batch
        .iter()
        .map(|blob| blob.oid.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let out = git(
        git_bin,
        dir,
        &["cat-file", "--batch"],
        &[],
        Some(format!("{stdin}\n").as_bytes()),
    )?;
    let mut at = 0usize;
    for blob in batch {
        let header_end = out[at..]
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| format!("cat-file ended before {}", blob.rel))?;
        let header = String::from_utf8_lossy(&out[at..at + header_end]).into_owned();
        at += header_end + 1;
        if header.ends_with("missing") {
            return Err(format!("cat-file missing object for {}", blob.rel));
        }
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|size| size.parse().ok())
            .ok_or_else(|| format!("cat-file header {header:?} for {}", blob.rel))?;
        let end = at
            .checked_add(size)
            .filter(|end| *end <= out.len())
            .ok_or_else(|| format!("cat-file truncated at {}", blob.rel))?;
        let path = dest.join(&blob.rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(&path, &out[at..end]).map_err(|error| error.to_string())?;
        // Skip the contents plus git's trailing newline.
        at = (end + 1).min(out.len());
    }
    Ok(())
}

/// Write one repository's default branch into `dest`. Returns the bytes
/// written and the files a bound dropped.
fn materialize_repo(
    git_bin: &str,
    dir: &Path,
    branch: &str,
    dest: &Path,
    budget: &mut Budget,
) -> Result<(u64, usize), String> {
    let (blobs, mut dropped) = listed_blobs(git_bin, dir, branch);
    if blobs.is_empty() {
        return Ok((0, dropped));
    }
    std::fs::create_dir_all(dest).map_err(|error| error.to_string())?;
    let mut written = 0u64;
    let mut batch: Vec<Blob> = Vec::with_capacity(BATCH_FILES);
    let mut stopped = false;
    for blob in blobs {
        if stopped
            || written + blob.size > MAX_REPO_BYTES
            || blob.size > budget.bytes_left
            || budget.files_left == 0
        {
            dropped += 1;
            continue;
        }
        written += blob.size;
        budget.bytes_left -= blob.size;
        budget.files_left -= 1;
        batch.push(blob);
        if batch.len() == BATCH_FILES {
            write_batch(git_bin, dir, dest, &batch)?;
            batch.clear();
            // The deadline is checked between batches, never mid-file, so a
            // materialized tree is always complete up to where it stopped.
            stopped = budget.expired();
        }
    }
    if !batch.is_empty() {
        write_batch(git_bin, dir, dest, &batch)?;
    }
    Ok((written, dropped))
}

/// A scratch directory unique to this process and moment.
fn workspace_root() -> Result<PathBuf, std::io::Error> {
    let root = std::env::temp_dir().join(format!(
        "jeryu-tool-finder-{}-{}",
        std::process::id(),
        super::epoch_ms()
    ));
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

/// Materialize every discovered family member's default branch into a scratch
/// directory the codegraph scanner can walk.
pub(crate) fn materialize(
    state: &WebState,
    families: HostedFamilies,
) -> Result<ScanWorkspace, HostedSourceError> {
    let HostedFamilies { repos, mut skipped } = families;
    if repos.is_empty() {
        return Err(HostedSourceError::NotConfigured);
    }
    let git_bin = &state.repo_manager.config().git_bin;
    let root = workspace_root().map_err(HostedSourceError::Workspace)?;
    let mut workspace = ScanWorkspace {
        root,
        roots: Vec::new(),
        repos: Vec::new(),
        skipped: Vec::new(),
    };
    let mut budget = Budget::new();
    for (index, mut source) in repos.into_iter().enumerate() {
        if index >= MAX_REPOS {
            skipped.push(SkippedRepo::new(
                &source.repo,
                "repo_budget",
                format!("over the {MAX_REPOS}-repository cap for one scan"),
            ));
            continue;
        }
        if budget.expired() {
            skipped.push(SkippedRepo::new(
                &source.repo,
                "deadline",
                format!(
                    "the {}s materialization deadline passed first",
                    MATERIALIZE_DEADLINE.as_secs()
                ),
            ));
            continue;
        }
        let Some((owner, name)) = source.repo.split_once('/') else {
            continue;
        };
        let Ok(opened) = state.repo_manager.open_parts(owner, name) else {
            skipped.push(SkippedRepo::new(
                &source.repo,
                "unreadable",
                "no bare repository under the forge git storage",
            ));
            continue;
        };
        let branch = source.branch.clone().unwrap_or_default();
        let Some(commit) = super::super::shift::resolve_commit(
            git_bin,
            &opened.path,
            &format!("refs/heads/{branch}"),
        ) else {
            skipped.push(SkippedRepo::new(
                &source.repo,
                "no_default_branch",
                format!("the default branch {branch:?} has no commit"),
            ));
            continue;
        };
        let dest = workspace.root.join(owner).join(name);
        match materialize_repo(git_bin, &opened.path, &branch, &dest, &mut budget) {
            Ok((0, _)) => skipped.push(SkippedRepo::new(
                &source.repo,
                "no_sources",
                format!("{branch} holds no file under the {MAX_FILE_BYTES}-byte per-file cap"),
            )),
            Ok((_, dropped)) => {
                if dropped > 0 {
                    skipped.push(SkippedRepo::new(
                        &source.repo,
                        "file_budget",
                        format!("{dropped} files left out: too large, a link, or over budget"),
                    ));
                }
                source.commit = Some(commit);
                workspace.roots.push((source.repo.clone(), dest));
                workspace.repos.push(source);
            }
            Err(error) => skipped.push(SkippedRepo::new(&source.repo, "write_failed", error)),
        }
    }
    if workspace.roots.is_empty() {
        return Err(HostedSourceError::NotConfigured);
    }
    workspace.skipped = skipped;
    Ok(workspace)
}
