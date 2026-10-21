//! Line survival: how many of the lines a merged change added are still on
//! the default branch 14 and 30 days after it merged.
//!
//! The Scorecard's "Surviving net lines/day" is built from this, because raw
//! net lines reward churn and stubs. For each merged repository leg of the
//! outcome ledger a background job runs `git blame` on the last first-parent
//! commit of the default branch at or before the merge plus 14 (and 30) days,
//! and counts the lines of the files the change touched that blame still
//! attributes to the change's commit (to any of the commits a merge commit
//! brought in). A rewrite of a line moves it to the rewriting commit, a
//! revert removes it, and a file rename keeps it, because blame follows
//! renames. Generated, vendored and lock files are left out, by the
//! repository's `linguist-generated` / `linguist-vendored` attributes at the
//! merge commit and by [`default_skipped`].
//!
//! The job never runs on a request: one wake-up every ten minutes measures at
//! most [`PASS_LIMIT`] legs, one repository after another, each git process
//! niced, with a pause between legs. A leg is stored only once measured, so a
//! crash, a missing repository or a commit that is not on the default branch
//! leaves it due; [`Attempts`] keeps such a leg from being retried on every
//! wake-up. Downtime needs no special handling: the tip of each window is
//! chosen by date, so a late measurement counts the same lines an on-time
//! one would have. Schema: `db/migrations/0027_line_survival.sql`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::WebState;
use super::pipeline::EventStore;

pub(crate) const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// The two observation marks, in days after the merge.
pub(crate) const WINDOWS_DAYS: [i64; 2] = [14, 30];
const WAKE_EVERY: Duration = Duration::from_secs(10 * 60);
const PASS_LIMIT: usize = 50;
const RETRY_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
const PAUSE_BETWEEN_LEGS: Duration = Duration::from_millis(250);
/// Commits a merge commit may bring in before the leg is left unmeasured.
const MAX_BRANCH_COMMITS: usize = 2000;
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A merged leg that lacks a measurement whose mark has passed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DueLeg {
    pub lot_id: String,
    pub repo: String,
    pub sha: String,
    pub merged_ms: i64,
    pub needs_14d: bool,
    pub needs_30d: bool,
}

/// What one pass measured for a leg; `None` where the window was not asked
/// for.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Measured {
    pub added: Option<i64>,
    pub deleted: Option<i64>,
    pub surviving_14d: Option<i64>,
    pub surviving_30d: Option<i64>,
}

/// When this process last tried each leg (`repo@sha`), so a leg that cannot be
/// measured is retried on a slow cadence. Entries of legs that are no longer
/// due are dropped each pass, which bounds the map by the due list.
pub(crate) type Attempts = HashMap<String, Instant>;

/// Spawn the job. `JERYU_LINE_SURVIVAL=0` turns it off.
pub(super) fn spawn(state: Arc<WebState>) {
    if std::env::var("JERYU_LINE_SURVIVAL").is_ok_and(|v| v.trim() == "0") {
        eprintln!("line survival: disabled by JERYU_LINE_SURVIVAL=0");
        return;
    }
    tokio::spawn(async move {
        let mut attempts = Attempts::new();
        let mut ticker = tokio::time::interval(WAKE_EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let state = state.clone();
            attempts = tokio::task::spawn_blocking(move || {
                let mut attempts = attempts;
                let git_bin = state.repo_manager.config().git_bin.clone();
                let resolve = |full_name: &str| {
                    let (owner, name) = full_name.split_once('/')?;
                    let managed = state.repo_manager.open_parts(owner, name).ok()?;
                    let branch = state
                        .core
                        .list_repositories(None)
                        .into_iter()
                        .find(|repo| repo.full_name == full_name)?
                        .default_branch;
                    Some((managed.path, branch))
                };
                let now = chrono::Utc::now().timestamp_millis();
                run_pass(&state.events, &git_bin, &resolve, now, &mut attempts);
                attempts
            })
            .await
            .unwrap_or_default();
        }
    });
}

/// Measure the legs that are due, one repository at a time. Returns how many
/// legs were stored.
pub(crate) fn run_pass(
    store: &EventStore,
    git_bin: &str,
    resolve: &dyn Fn(&str) -> Option<(PathBuf, String)>,
    now_ms: i64,
    attempts: &mut Attempts,
) -> usize {
    let due = match store.line_survival_due(now_ms, PASS_LIMIT * 4) {
        Ok(due) => due,
        Err(err) => {
            eprintln!("line survival: reading the due legs failed: {err}");
            return 0;
        }
    };
    let keys: BTreeSet<String> = due.iter().map(|d| attempt_key(&d.repo, &d.sha)).collect();
    attempts.retain(|key, _| keys.contains(key));
    let mut todo: Vec<&DueLeg> = due
        .iter()
        .filter(|d| {
            attempts
                .get(&attempt_key(&d.repo, &d.sha))
                .is_none_or(|at| at.elapsed() >= RETRY_AFTER)
        })
        .take(PASS_LIMIT)
        .collect();
    // One repository at a time: legs of a repository run back to back.
    todo.sort_by(|a, b| a.repo.cmp(&b.repo).then(a.merged_ms.cmp(&b.merged_ms)));
    let mut stored = 0;
    for leg in todo {
        attempts.insert(attempt_key(&leg.repo, &leg.sha), Instant::now());
        let Some((git_dir, branch)) = resolve(&leg.repo) else {
            continue;
        };
        let result = measure(git_bin, &git_dir, &branch, leg)
            .and_then(|measured| store.record_line_survival(&leg.lot_id, &leg.repo, &measured));
        match result {
            Ok(()) => stored += 1,
            Err(err) => eprintln!(
                "line survival: {} at {} not measured: {err}",
                leg.repo, leg.sha
            ),
        }
        std::thread::sleep(PAUSE_BETWEEN_LEGS);
    }
    stored
}

fn attempt_key(repo: &str, sha: &str) -> String {
    format!("{repo}@{sha}")
}

/// Run git (niced when `nice` exists) in `git_dir`, returning stdout.
fn git(git_bin: &str, git_dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let run = |mut cmd: Command| {
        cmd.arg("--git-dir")
            .arg(git_dir)
            .args(["-c", "core.quotepath=false"])
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .output()
    };
    let mut niced = Command::new("nice");
    niced.args(["-n", "19", git_bin]);
    let output = match run(niced) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => run(Command::new(git_bin)),
        other => other,
    }
    .map_err(|err| format!("git {}: {err}", args.first().unwrap_or(&"")))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn is_hex_sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Measure one leg: the diff of its merge once, and the survival of each
/// window it still needs.
pub(crate) fn measure(
    git_bin: &str,
    git_dir: &Path,
    branch: &str,
    leg: &DueLeg,
) -> Result<Measured, String> {
    if !is_hex_sha(&leg.sha) {
        return Err("the recorded sha is not a hex commit id".into());
    }
    if branch.is_empty() || branch.starts_with('-') || branch.chars().any(char::is_control) {
        return Err("the default branch name is not usable".into());
    }
    let sha_arg = format!("{}^{{commit}}", leg.sha);
    let commit = String::from_utf8_lossy(&git(
        git_bin,
        git_dir,
        &["rev-parse", "--verify", "--quiet", &sha_arg],
    )?)
    .trim()
    .to_string();
    let parents = String::from_utf8_lossy(&git(
        git_bin,
        git_dir,
        &["rev-list", "--parents", "-n", "1", &commit],
    )?)
    .split_whitespace()
    .skip(1)
    .map(ToString::to_string)
    .collect::<Vec<_>>();
    let base = parents.first().map_or(EMPTY_TREE, String::as_str);
    // The commits whose lines count as this change's: the commit itself, or
    // for a merge commit every commit it brought in.
    let mut authors: BTreeSet<String> = BTreeSet::from([commit.clone()]);
    if parents.len() > 1 {
        let range = format!("{base}..{commit}");
        let max = MAX_BRANCH_COMMITS.to_string();
        let listed = git(git_bin, git_dir, &["rev-list", "--max-count", &max, &range])?;
        authors.extend(String::from_utf8_lossy(&listed).lines().map(str::to_string));
    }
    let numstat = git(
        git_bin,
        git_dir,
        &["diff", "--numstat", "-z", "-M", base, &commit],
    )?;
    let candidates = parse_numstat(&numstat);
    let skipped = skipped_paths(git_bin, git_dir, &commit, &candidates);
    let files: Vec<&FileStat> = candidates
        .iter()
        .filter(|f| !skipped.contains(&f.path))
        .collect();
    let mut measured = Measured {
        added: Some(files.iter().map(|f| f.added).sum()),
        deleted: Some(files.iter().map(|f| f.deleted).sum()),
        ..Measured::default()
    };
    for (days, wanted) in [
        (WINDOWS_DAYS[0], leg.needs_14d),
        (WINDOWS_DAYS[1], leg.needs_30d),
    ] {
        if !wanted {
            continue;
        }
        let at_secs = (leg.merged_ms + days * DAY_MS) / 1000;
        let surviving = surviving_at(git_bin, git_dir, branch, &commit, &authors, &files, at_secs)?;
        if days == WINDOWS_DAYS[0] {
            measured.surviving_14d = Some(surviving);
        } else {
            measured.surviving_30d = Some(surviving);
        }
    }
    Ok(measured)
}

/// Lines of `files` blamed on `authors` at the last first-parent commit of
/// `branch` made at or before `at_secs`.
fn surviving_at(
    git_bin: &str,
    git_dir: &Path,
    branch: &str,
    commit: &str,
    authors: &BTreeSet<String>,
    files: &[&FileStat],
    at_secs: i64,
) -> Result<i64, String> {
    let reference = format!("refs/heads/{branch}");
    let before = format!("--before={at_secs}");
    let tip = String::from_utf8_lossy(&git(
        git_bin,
        git_dir,
        &["rev-list", "-1", "--first-parent", &before, &reference],
    )?)
    .trim()
    .to_string();
    if tip.is_empty() {
        return Err("the default branch has no commit at that time".into());
    }
    if git(
        git_bin,
        git_dir,
        &["merge-base", "--is-ancestor", commit, &tip],
    )
    .is_err()
    {
        return Err("the commit is not on the default branch at that time".into());
    }
    let moved = parse_name_status(&git(
        git_bin,
        git_dir,
        &["diff", "--name-status", "-z", "-M", commit, &tip],
    )?);
    let mut total = 0;
    for file in files {
        let target = match moved.get(&file.path) {
            Some(Some(new)) => new.as_str(),
            Some(None) => continue,
            None => file.path.as_str(),
        };
        let blame = git(
            git_bin,
            git_dir,
            &["blame", "-M", "--porcelain", &tip, "--", target],
        )?;
        total += blamed_lines(&blame, authors).min(file.added);
    }
    Ok(total)
}

/// One file of the merge diff, by its path in the merged tree.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FileStat {
    pub path: String,
    pub added: i64,
    pub deleted: i64,
}

/// `git diff --numstat -z -M`: `added\tdeleted\tpath\0`, and for a rename
/// `added\tdeleted\t\0old\0new\0`. Binary files (`-`) are left out.
pub(crate) fn parse_numstat(raw: &[u8]) -> Vec<FileStat> {
    let mut out = Vec::new();
    let mut parts = raw.split(|b| *b == 0).map(String::from_utf8_lossy);
    while let Some(head) = parts.next() {
        let mut fields = head.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            let _old = parts.next();
            parts.next().map(|p| p.into_owned()).unwrap_or_default()
        } else {
            path.to_string()
        };
        if let (Ok(added), Ok(deleted)) = (added.parse::<i64>(), deleted.parse::<i64>())
            && !path.is_empty()
        {
            out.push(FileStat {
                path,
                added,
                deleted,
            });
        }
    }
    out
}

/// `git diff --name-status -z -M`: where each path went. `Some(new)` for a
/// rename, `None` for a deletion; a path that is not listed is unchanged.
pub(crate) fn parse_name_status(raw: &[u8]) -> HashMap<String, Option<String>> {
    let mut moved = HashMap::new();
    let mut parts = raw.split(|b| *b == 0).map(String::from_utf8_lossy);
    while let Some(status) = parts.next() {
        match status.chars().next() {
            Some('R') => {
                if let (Some(old), Some(new)) = (parts.next(), parts.next()) {
                    moved.insert(old.into_owned(), Some(new.into_owned()));
                }
            }
            Some('C') => {
                let _ = (parts.next(), parts.next());
            }
            Some('D') => {
                if let Some(old) = parts.next() {
                    moved.insert(old.into_owned(), None);
                }
            }
            Some(_) => {
                let _ = parts.next();
            }
            None => {}
        }
    }
    moved
}

/// `git blame --porcelain`: every line has a header `<sha> <orig> <final>
/// [<group size>]` and then its text after a tab. Counts the lines whose
/// header sha is one of `authors`.
pub(crate) fn blamed_lines(porcelain: &[u8], authors: &BTreeSet<String>) -> i64 {
    let mut current = false;
    let mut count = 0;
    for line in porcelain.split(|b| *b == b'\n') {
        if line.first() == Some(&b'\t') {
            if current {
                count += 1;
            }
            continue;
        }
        let mut words = line.split(|b| *b == b' ');
        if let Some(sha) = words.next()
            && sha.len() == 40
            && sha.iter().all(u8::is_ascii_hexdigit)
            && words
                .next()
                .is_some_and(|w| w.iter().all(u8::is_ascii_digit))
        {
            current = authors.contains(String::from_utf8_lossy(sha).as_ref());
        }
    }
    count
}

/// Files that are lock files or generated or vendored output by name.
pub(crate) fn default_skipped(path: &str) -> bool {
    const NAMES: [&str; 14] = [
        "Cargo.lock",
        "package-lock.json",
        "npm-shrinkwrap.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "poetry.lock",
        "Pipfile.lock",
        "Gemfile.lock",
        "composer.lock",
        "go.sum",
        "flake.lock",
        "uv.lock",
        "bun.lockb",
        "deno.lock",
    ];
    const SUFFIXES: [&str; 6] = [".lock", ".min.js", ".min.css", ".map", ".pb.go", ".snap"];
    const DIRS: [&str; 4] = ["node_modules", "vendor", "dist", "third_party"];
    let name = path.rsplit('/').next().unwrap_or(path);
    NAMES.contains(&name)
        || SUFFIXES.iter().any(|s| name.ends_with(s))
        || path.split('/').rev().skip(1).any(|dir| DIRS.contains(&dir))
}

/// Paths to leave out: those the repository marks `linguist-generated` or
/// `linguist-vendored` at `commit`, or [`default_skipped`] names, unless the
/// repository sets the attribute off for the path.
fn skipped_paths(
    git_bin: &str,
    git_dir: &Path,
    commit: &str,
    files: &[FileStat],
) -> BTreeSet<String> {
    let mut attrs: HashMap<String, bool> = HashMap::new();
    if !files.is_empty() {
        let source = format!("--source={commit}");
        let mut paths = Vec::new();
        for file in files {
            paths.extend_from_slice(file.path.as_bytes());
            paths.push(0);
        }
        if let Ok(raw) = git_stdin(
            git_bin,
            git_dir,
            &[
                "check-attr",
                &source,
                "-z",
                "--stdin",
                "linguist-generated",
                "linguist-vendored",
            ],
            &paths,
        ) {
            attrs = parse_check_attr(&raw);
        }
    }
    files
        .iter()
        .filter(|f| {
            attrs
                .get(&f.path)
                .copied()
                .unwrap_or_else(|| default_skipped(&f.path))
        })
        .map(|f| f.path.clone())
        .collect()
}

/// `git check-attr -z`: `path\0attr\0value\0`. A path is true when an
/// attribute is set, false when every attribute set for it is unset, and
/// absent when none is specified.
pub(crate) fn parse_check_attr(raw: &[u8]) -> HashMap<String, bool> {
    let mut out: HashMap<String, bool> = HashMap::new();
    let mut parts = raw.split(|b| *b == 0).map(String::from_utf8_lossy);
    while let (Some(path), Some(_attr), Some(value)) = (parts.next(), parts.next(), parts.next()) {
        match value.as_ref() {
            "set" | "true" => {
                out.insert(path.into_owned(), true);
            }
            "unset" | "false" => {
                out.entry(path.into_owned()).or_insert(false);
            }
            _ => {}
        }
    }
    out
}

fn git_stdin(
    git_bin: &str,
    git_dir: &Path,
    args: &[&str],
    input: &[u8],
) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut child = Command::new(git_bin)
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| err.to_string())?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let data = input.to_vec();
    // Written from a thread so a full stdout pipe cannot block the write.
    let writer = std::thread::spawn(move || stdin.write_all(&data));
    let output = child.wait_with_output().map_err(|err| err.to_string())?;
    let _ = writer.join();
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err("git check-attr failed".into())
    }
}

#[cfg(test)]
mod tests;
