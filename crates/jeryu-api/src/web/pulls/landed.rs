//! Whether an open pull request's change is already on its base branch.
//!
//! A pull request stays open after its change landed by another route: a
//! squash or cherry-pick through a replacement pull request, a direct push, a
//! merge that never flipped the record. Nothing about its checks or reviews
//! says so, so it sits in the flow as "ready to merge" or "waiting on review"
//! when the only step left is to close it.
//!
//! Two tests, against the LIVE tip of the base branch (a pull request's stored
//! base sha is frozen when it opens):
//!
//! - [`Landed::Ancestor`]: the head commit is already in the base's history.
//! - [`Landed::Content`]: merging the head into the base would change nothing —
//!   `git merge-tree` of the two produces the base's own tree, without
//!   conflicts. This is what catches a squash or a cherry-pick.
//!
//! `merge-tree` writes the trees it builds, so it runs against a throwaway
//! object directory with the repository's objects as an alternate: reading the
//! inbox never writes to a repository. Answers are cached by repository, base
//! tip and head, which only change when somebody pushes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use jeryu_core::PullRequest;

use super::WebState;

/// How a pull request's change is already on its base branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Landed {
    /// The head commit is in the base branch's history.
    Ancestor,
    /// Merging the head would leave the base's tree unchanged.
    Content,
}

/// Cached answers before the cache is dropped and rebuilt: enough for every
/// open pull request on a busy forge, small enough never to matter.
const CACHE_LIMIT: usize = 4096;

type CacheKey = (PathBuf, String, String);

fn cache() -> &'static Mutex<HashMap<CacheKey, Option<Landed>>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, Option<Landed>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `pr`'s change is already on its base branch, or `None` when it is
/// not or git cannot tell (repository not materialized, head not fetched).
pub(in crate::web) fn already_landed(state: &WebState, pr: &PullRequest) -> Option<Landed> {
    if pr.head.sha.is_empty() || pr.base.ref_name.is_empty() {
        return None;
    }
    let resolved = state.repo_manager.resolve_parts(&pr.owner, &pr.repo).ok()?;
    let git_bin = &state.repo_manager.config().git_bin;
    let base = rev_parse(
        git_bin,
        &resolved.path,
        &format!("refs/heads/{}^{{commit}}", pr.base.ref_name),
    )?;
    let key = (resolved.path.clone(), base.clone(), pr.head.sha.clone());
    if let Some(answer) = cache().lock().expect("landed cache lock").get(&key) {
        return *answer;
    }
    let answer = landed(git_bin, &resolved.path, &base, &pr.head.sha);
    let mut held = cache().lock().expect("landed cache lock");
    if held.len() >= CACHE_LIMIT {
        held.clear();
    }
    held.insert(key, answer);
    answer
}

fn landed(git_bin: &str, repo: &Path, base: &str, head: &str) -> Option<Landed> {
    // A head this repository does not hold (a fork's, not yet fetched) is
    // something git cannot answer about, not a change that has landed.
    rev_parse(git_bin, repo, &format!("{head}^{{commit}}"))?;
    let ancestor = Command::new(git_bin)
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", head, base])
        .status()
        .ok()?;
    if ancestor.success() {
        return Some(Landed::Ancestor);
    }
    let base_tree = rev_parse(git_bin, repo, &format!("{base}^{{tree}}"))?;
    (merged_tree(git_bin, repo, base, head)? == base_tree).then_some(Landed::Content)
}

/// The tree `git merge-tree` builds for merging `head` into `base`, or `None`
/// when it conflicts or git cannot build it (a git older than 2.38 has no
/// `--write-tree`, which then reads as "not landed").
fn merged_tree(git_bin: &str, repo: &Path, base: &str, head: &str) -> Option<String> {
    let scratch = scratch_dir()?;
    let output = Command::new(git_bin)
        .arg("-C")
        .arg(repo)
        .env("GIT_OBJECT_DIRECTORY", &scratch)
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", repo.join("objects"))
        .args(["merge-tree", "--write-tree", base, head])
        .output();
    let _ = std::fs::remove_dir_all(&scratch);
    let output = output.ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::to_string)
}

/// A new, empty, owner-only directory for one `merge-tree` run. It is created
/// exclusively (never reused), so a directory another local user made first at
/// a guessed name is skipped rather than written into.
fn scratch_dir() -> Option<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    for _ in 0..8 {
        let scratch = std::env::temp_dir().join(format!(
            "jeryu-landed-{}-{nanos:x}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&scratch) {
            Ok(()) => return Some(scratch),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

fn rev_parse(git_bin: &str, repo: &Path, rev: &str) -> Option<String> {
    let output = Command::new(git_bin)
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", rev])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cwd: &Path, args: &[&str]) -> String {
        let output = crate::test_git::git_command()
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "jeryu-test")
            .env("GIT_AUTHOR_EMAIL", "jeryu-test@example.com")
            .env("GIT_COMMITTER_NAME", "jeryu-test")
            .env("GIT_COMMITTER_EMAIL", "jeryu-test@example.com")
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit(work: &Path, file: &str, body: &str, message: &str) -> String {
        std::fs::write(work.join(file), body).unwrap();
        run(work, &["add", "."]);
        run(work, &["commit", "--quiet", "-m", message]);
        run(work, &["rev-parse", "HEAD"])
    }

    fn object_count(bare: &Path) -> String {
        run(bare, &["count-objects", "-v"])
    }

    /// `main` holds a squash of `squashed` (two commits, one change) and a
    /// merge of `merged`; `open` changes something main never got.
    #[test]
    fn heads_already_on_the_base_are_told_apart_from_open_work() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        run(&work, &["init", "--quiet", "-b", "main"]);
        let base = commit(&work, "shared.txt", "one\n", "base");

        run(&work, &["checkout", "--quiet", "-b", "squashed"]);
        commit(&work, "a.txt", "a\n", "a, first half");
        let squashed = commit(&work, "a.txt", "a\nmore\n", "a, second half");
        run(&work, &["checkout", "--quiet", "-b", "open", &base]);
        let open = commit(&work, "b.txt", "b\n", "never landed");
        run(&work, &["checkout", "--quiet", "-b", "merged", &base]);
        let merged = commit(&work, "c.txt", "c\n", "merged for real");

        run(&work, &["checkout", "--quiet", "main"]);
        run(&work, &["merge", "--quiet", "--squash", "squashed"]);
        run(&work, &["commit", "--quiet", "-m", "squash of a"]);
        run(
            &work,
            &["merge", "--quiet", "--no-ff", "-m", "merge c", "merged"],
        );
        commit(&work, "shared.txt", "one\ntwo\n", "main moves on");
        let main = run(&work, &["rev-parse", "HEAD"]);

        let bare = dir.path().join("repo.git");
        run(
            &work,
            &["clone", "--quiet", "--bare", ".", bare.to_str().unwrap()],
        );
        for branch in ["squashed", "open", "merged"] {
            run(&work, &["push", "--quiet", bare.to_str().unwrap(), branch]);
        }
        let before = object_count(&bare);

        assert_eq!(landed("git", &bare, &main, &merged), Some(Landed::Ancestor));
        assert_eq!(
            landed("git", &bare, &main, &squashed),
            Some(Landed::Content)
        );
        assert_eq!(landed("git", &bare, &main, &open), None);
        assert_eq!(
            landed(
                "git",
                &bare,
                &main,
                "0123456789abcdef0123456789abcdef01234567"
            ),
            None,
            "a head the repository does not hold is unknown, not landed"
        );
        assert_eq!(
            object_count(&bare),
            before,
            "the check never writes to the repository"
        );
    }
}
