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
//! [`landing_commit`] answers a second question, asked only when something is
//! being closed: *which* commit of the base carries the work, so the comment
//! the close leaves behind can name it.
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
    if pr.base.ref_name.is_empty() {
        return None;
    }
    let head = live_head(state, pr)?;
    let resolved = state.repo_manager.resolve_parts(&pr.owner, &pr.repo).ok()?;
    let git_bin = &state.repo_manager.config().git_bin;
    let base = rev_parse(
        git_bin,
        &resolved.path,
        &format!("refs/heads/{}^{{commit}}", pr.base.ref_name),
    )?;
    let key = (resolved.path.clone(), base.clone(), head.clone());
    if let Some(answer) = cache().lock().expect("landed cache lock").get(&key) {
        return *answer;
    }
    let answer = landed(git_bin, &resolved.path, &base, &head);
    let mut held = cache().lock().expect("landed cache lock");
    if held.len() >= CACHE_LIMIT {
        held.clear();
    }
    held.insert(key, answer);
    answer
}

/// Read the source branch, never a caller-supplied or outdated stored SHA.
/// A missing source or branch is unknown, so it cannot suppress review.
pub(in crate::web) fn live_head(state: &WebState, pr: &PullRequest) -> Option<String> {
    let (owner, repo) = pr.source_repository.trim().split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    let source = state
        .repo_manager
        .resolve_parts(owner, repo.trim_end_matches(".git"))
        .ok()?;
    let branch = pr
        .head
        .ref_name
        .split_once(':')
        .map_or(pr.head.ref_name.as_str(), |(_, branch)| branch);
    rev_parse(
        &state.repo_manager.config().git_bin,
        &source.path,
        &format!("refs/heads/{branch}^{{commit}}"),
    )
}

/// The commit on the base branch that already carries `pr`'s work, named in
/// the comment the close leaves behind so the reason outlives the pull
/// request. `None` when the pull request still has something to land, or when
/// git cannot say.
///
/// The same two tests as [`already_landed`] decide it; this only answers
/// *which* commit. A head already in the base's history landed as itself. One
/// whose content is there arrived as some other commit, found by the patch
/// identity of the head's own newest commit (`git patch-id`, which is how a
/// cherry-pick is recognised across a rewritten sha); a squash or a reworded
/// rebase has no twin, and then the base's own tip is the commit to name,
/// because that is the state the work is in.
pub(in crate::web) fn landing_commit(state: &WebState, pr: &PullRequest) -> Option<String> {
    let head = live_head(state, pr)?;
    let resolved = state.repo_manager.resolve_parts(&pr.owner, &pr.repo).ok()?;
    let git_bin = &state.repo_manager.config().git_bin;
    let base = rev_parse(
        git_bin,
        &resolved.path,
        &format!("refs/heads/{}^{{commit}}", pr.base.ref_name),
    )?;
    match landed(git_bin, &resolved.path, &base, &head)? {
        Landed::Ancestor => Some(head),
        Landed::Content => {
            Some(landing_twin(git_bin, &resolved.path, &base, &head).unwrap_or(base))
        }
    }
}

/// The commit of `base` carrying the same diff as the newest commit `head` has
/// of its own, or `None` when there is none to find.
fn landing_twin(git_bin: &str, repo: &Path, base: &str, head: &str) -> Option<String> {
    let newest = rev_list(git_bin, repo, &format!("{base}..{head}"), 1)?
        .into_iter()
        .next()?;
    let identity = patch_identity(git_bin, repo, &newest)?;
    // Only what the base gained since the head left it can be the landing
    // commit, and a pull request longer than this is not one of these.
    rev_list(git_bin, repo, &format!("{head}..{base}"), 200)?
        .into_iter()
        .find(|commit| patch_identity(git_bin, repo, commit).as_deref() == Some(&identity))
}

/// The commits in `range`, newest first, merges left out: `git patch-id` has
/// no answer for a merge.
fn rev_list(git_bin: &str, repo: &Path, range: &str, max: usize) -> Option<Vec<String>> {
    let output = Command::new(git_bin)
        .arg("-C")
        .arg(repo)
        .args([
            "rev-list",
            "--no-merges",
            &format!("--max-count={max}"),
            "--end-of-options",
            range,
        ])
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    })
}

/// What `commit` changes, as the identity two commits with the same diff
/// share. A root commit is read as the diff against the empty tree (`--root`),
/// so a pull request whose first commit is the repository's first one still
/// gets an answer.
fn patch_identity(git_bin: &str, repo: &Path, commit: &str) -> Option<String> {
    let diff = Command::new(git_bin)
        .arg("-C")
        .arg(repo)
        .args([
            "diff-tree",
            "--patch",
            "--no-commit-id",
            "--no-color",
            "--no-ext-diff",
            "--root",
            "--end-of-options",
            commit,
        ])
        .output()
        .ok()?;
    if !diff.status.success() {
        return None;
    }
    let mut child = Command::new(git_bin)
        .arg("-C")
        .arg(repo)
        .args(["patch-id", "--stable"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    std::io::Write::write_all(child.stdin.as_mut()?, &diff.stdout).ok()?;
    drop(child.stdin.take());
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let identity = line.lines().next()?.split_whitespace().next()?.to_string();
    (!identity.is_empty()).then_some(identity)
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
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options", rev])
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

    #[test]
    fn stored_head_cannot_hide_live_work_and_cache_follows_the_branch() {
        let fx = crate::web::merge_queue_tests::Fixture::new("Justfile", "gate\n");
        let mut pr = fx
            .state
            .core
            .get_pull_request("alice", "jeryu", fx.number)
            .unwrap();
        pr.head.sha = fx.main();
        assert_eq!(live_head(&fx.state, &pr), Some(fx.head.clone()));
        assert_eq!(already_landed(&fx.state, &pr), None);
        let mut current = pr.clone();
        current.head.sha = live_head(&fx.state, &pr).unwrap();
        assert!(
            crate::web::pulls::diff::changed_paths(&fx.state, &current)
                .contains(&"Justfile".to_string())
        );

        run(&fx.bare, &["update-ref", "refs/heads/feature", &fx.main()]);
        assert_eq!(already_landed(&fx.state, &pr), Some(Landed::Ancestor));
        pr.head.ref_name = "missing".to_string();
        assert_eq!(already_landed(&fx.state, &pr), None);
        pr.source_repository = "unverified".to_string();
        assert_eq!(live_head(&fx.state, &pr), None);
    }

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
        // A squash has no commit with the branch's own patch, so there is no
        // twin to name and `landing_commit` falls back to the base's tip; a
        // cherry-pick does, and that commit is the one named.
        assert_eq!(landing_twin("git", &bare, &main, &squashed), None);
        let picked = run(&work, &["rev-parse", "main~1"]);
        run(&work, &["checkout", "--quiet", "-b", "picked", &base]);
        let pick_head = commit(&work, "c.txt", "c\n", "c again, by hand");
        run(
            &work,
            &["push", "--quiet", bare.to_str().unwrap(), "picked"],
        );
        assert_eq!(
            landing_twin("git", &bare, &main, &pick_head).as_deref(),
            Some(merged.as_str()),
            "the cherry-picked twin on the base is the commit to name ({picked})"
        );
    }
}
