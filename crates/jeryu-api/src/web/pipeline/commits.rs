//! Commits on each repository's default branch, counted by author time, and
//! the lines they added and deleted: the code-change series of the Activity
//! band (`GET /api/v1/events/histogram`).
//!
//! A commit is not a pipeline event, so it is not in `pipeline_events`: it is
//! read from git. Only the default branch counts, because a branch is rebased
//! and pushed again and its commits would be counted once per push; merge
//! commits are left out, so each commit is one piece of work. The author time
//! is when the work was written. On a rebase-merged main the committer time is
//! when its pull request landed instead, so a commit written ten days ago that
//! lands today raises the bar of ten days ago, not today's.
//!
//! Lines are counted as git's `--numstat` counts them, per file, without
//! binary files (git counts no lines in them) and without the files a tool
//! writes rather than a person ([`generated`]): a lockfile refresh or a
//! rebuilt bundle is thousands of lines nobody wrote.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use jeryu_core::Repository;

use super::super::WebState;
use super::super::shift::rfc3339_ms;

/// A read of a repository whose push time has not moved is reused this long;
/// past it the lookback no longer covers the oldest bucket of a 720 h window.
const REUSE_HOURS: i64 = 24;
/// How far back one read of a repository reaches: the longest window the
/// histogram draws (720 h) plus the day a read is reused for.
const LOOKBACK_HOURS: i64 = 720 + REUSE_HOURS;
/// A repository with no recorded push time is read again after this long.
const UNPUSHED_REUSE_MS: i64 = 10 * 60 * 1000;
/// Most commits read from one repository. A history imported in one push is
/// counted up to this.
const MAX_COMMITS: &str = "--max-count=20000";
/// Starts a commit's header line in the log, which no path can contain.
const HEADER: char = '\u{1}';

/// One commit: when it was written and the lines it changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Commit {
    pub authored_ms: i64,
    pub added: i64,
    pub deleted: i64,
}

/// What one bucket holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Change {
    pub commits: i64,
    pub lines_added: i64,
    pub lines_deleted: i64,
}

/// Files a tool writes: lockfiles, built bundles and generated code. Their
/// lines are left out of the counts; the commit that touches them still counts.
pub(super) fn generated(path: &str) -> bool {
    const LOCKFILES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "npm-shrinkwrap.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "poetry.lock",
        "uv.lock",
        "go.sum",
    ];
    const DIRS: &[&str] = &["dist", "generated", "vendor", "node_modules"];
    let file = path.rsplit('/').next().unwrap_or(path);
    LOCKFILES.contains(&file)
        || file.ends_with(".min.js")
        || file.ends_with(".min.css")
        || path.split('/').rev().skip(1).any(|dir| DIRS.contains(&dir))
}

/// The path a `--numstat` line names: the new side of a rename, whether git
/// wrote it as `old => new` or `dir/{old => new}/file`.
fn numstat_path(raw: &str) -> String {
    match (raw.find('{'), raw.find(" => "), raw.find('}')) {
        (Some(open), Some(arrow), Some(close)) if open < arrow && arrow < close => {
            let new = &raw[arrow + 4..close];
            format!("{}{new}{}", &raw[..open], &raw[close + 1..]).replace("//", "/")
        }
        (_, Some(arrow), _) => raw[arrow + 4..].to_string(),
        _ => raw.to_string(),
    }
}

/// The commits a `git log --numstat --format=<HEADER>%at` run printed.
pub(super) fn parse_log(out: &str) -> Vec<Commit> {
    let mut commits: Vec<Commit> = Vec::new();
    for line in out.lines() {
        if let Some(header) = line.strip_prefix(HEADER) {
            if let Ok(seconds) = header.trim().parse::<i64>() {
                commits.push(Commit {
                    authored_ms: seconds * 1000,
                    ..Commit::default()
                });
            }
            continue;
        }
        let mut fields = line.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // A binary file is `-\t-\tpath`: no lines to count.
        let (Ok(added), Ok(deleted)) = (added.parse::<i64>(), deleted.parse::<i64>()) else {
            continue;
        };
        if generated(&numstat_path(path)) {
            continue;
        }
        if let Some(commit) = commits.last_mut() {
            commit.added += added;
            commit.deleted += deleted;
        }
    }
    commits
}

/// One repository's commits, as of one read.
#[derive(Clone, Debug)]
struct Read {
    pushed_at: Option<DateTime<Utc>>,
    default_branch: String,
    read_ms: i64,
    commits: Vec<Commit>,
}

impl Read {
    /// Whether this read still answers for `repo` at `now_ms`. Every branch
    /// move advances `pushed_at`, so an unmoved push time is an unmoved
    /// default branch.
    fn answers(&self, repo: &Repository, now_ms: i64) -> bool {
        let reuse_ms = if self.pushed_at.is_some() {
            REUSE_HOURS * 3_600_000
        } else {
            UNPUSHED_REUSE_MS
        };
        self.pushed_at == repo.pushed_at
            && self.default_branch == repo.default_branch
            && now_ms - self.read_ms < reuse_ms
    }
}

/// Keyed by the bare repository's path, which no two hosted repositories
/// share (a name can be reused after a rename; a path is the repository).
fn reads() -> &'static Mutex<HashMap<PathBuf, Read>> {
    static READS: OnceLock<Mutex<HashMap<PathBuf, Read>>> = OnceLock::new();
    READS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The non-merge commits on `branch` committed since `since_ms`. A
/// repository with no such branch yet (empty, or not on disk) has none.
fn read_git(state: &WebState, path: &Path, branch: &str, since_ms: i64) -> Vec<Commit> {
    let since = format!("--since={}", rfc3339_ms(since_ms));
    let format = format!("--format={HEADER}%at");
    let rev = format!("refs/heads/{branch}");
    let Ok(out) = super::super::shift::run_git(
        &state.repo_manager.config().git_bin,
        path,
        &[
            "log",
            "--no-merges",
            "--numstat",
            MAX_COMMITS,
            &since,
            &format,
            "--end-of-options",
            &rev,
        ],
        &[],
        None,
    ) else {
        return Vec::new();
    };
    parse_log(&String::from_utf8_lossy(&out))
}

/// `repo`'s commits, from the last read when it still answers.
fn commits_of(state: &WebState, repo: &Repository, now_ms: i64) -> Vec<Commit> {
    let Ok(resolved) = state.repo_manager.resolve_parts(&repo.owner, &repo.name) else {
        return Vec::new();
    };
    let path = resolved.path.to_path_buf();
    if let Some(read) = reads()
        .lock()
        .ok()
        .and_then(|reads| reads.get(&path).cloned())
        && read.answers(repo, now_ms)
    {
        return read.commits;
    }
    let since_ms = now_ms - LOOKBACK_HOURS * 3_600_000;
    let commits = read_git(state, &path, &repo.default_branch, since_ms);
    if let Ok(mut reads) = reads().lock() {
        reads.insert(
            path,
            Read {
                pushed_at: repo.pushed_at,
                default_branch: repo.default_branch.clone(),
                read_ms: now_ms,
                commits: commits.clone(),
            },
        );
    }
    commits
}

/// The repositories a histogram scope covers: the family's, narrowed to one
/// `owner/name` when `repo` names it, or every hosted repository.
fn in_scope(state: &WebState, family: Option<&str>, repo: Option<&str>) -> Vec<Repository> {
    let repos = match family {
        Some(key) => super::super::family::repositories(state, key),
        None => state.github.core().list_repositories(None),
    };
    match repo.map(str::trim).filter(|slug| !slug.is_empty()) {
        Some(slug) => repos
            .into_iter()
            .filter(|repo| repo.full_name.eq_ignore_ascii_case(slug))
            .collect(),
        None => repos,
    }
}

/// What was authored in each bucket of `[from_ms, to_ms)`, oldest first,
/// over the repositories `family` and `repo` select.
pub(super) fn per_bucket(
    state: &WebState,
    family: Option<&str>,
    repo: Option<&str>,
    (from_ms, to_ms, bucket_ms): (i64, i64, i64),
    now_ms: i64,
) -> Vec<Change> {
    let count = usize::try_from((to_ms - from_ms) / bucket_ms).unwrap_or(0);
    let mut buckets = vec![Change::default(); count];
    for repository in in_scope(state, family, repo) {
        for commit in commits_of(state, &repository, now_ms) {
            if !(from_ms..to_ms).contains(&commit.authored_ms) {
                continue;
            }
            if let Some(bucket) = usize::try_from((commit.authored_ms - from_ms) / bucket_ms)
                .ok()
                .and_then(|index| buckets.get_mut(index))
            {
                bucket.commits += 1;
                bucket.lines_added += commit.added;
                bucket.lines_deleted += commit.deleted;
            }
        }
    }
    buckets
}
