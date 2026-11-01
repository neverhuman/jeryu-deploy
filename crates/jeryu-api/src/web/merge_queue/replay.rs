//! Git mechanics of the merge queue: replay a pull request's commits onto the
//! current base tip, prove the replay carries exactly the reviewed change, and
//! manage the forge-owned `refs/queue/*` and `refs/queue-meta/*` refs.
//!
//! Every git call runs in the repository's bare directory (or a throwaway
//! worktree of it), passes revisions after `--end-of-options`, and only ever
//! receives validated 40-hex object ids or `refs/heads|queue|queue-meta/...`
//! names built here.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Why a pull request could not be queued on the current base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReplayFailure {
    /// Replaying onto the tip conflicted in these paths; the author must rebase.
    Conflict(Vec<String>),
    /// The replay applied but does not carry the same change (a file dropped,
    /// added or renamed, or a hunk that differs). Never landed.
    Mismatch(String),
    /// The PR range contains merge commits, which the first cut does not replay.
    MergeCommits,
    /// Git itself failed.
    Git(String),
}

impl std::fmt::Display for ReplayFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayFailure::Conflict(paths) => {
                write!(
                    f,
                    "replaying onto the base conflicts in: {}",
                    paths.join(", ")
                )
            }
            ReplayFailure::Mismatch(detail) => {
                write!(f, "the replay does not carry the reviewed change: {detail}")
            }
            ReplayFailure::MergeCommits => {
                write!(
                    f,
                    "the pull request contains merge commits; rebase it onto the base"
                )
            }
            ReplayFailure::Git(detail) => write!(f, "git failed: {detail}"),
        }
    }
}

pub(super) struct Git<'a> {
    pub(super) bin: &'a str,
    pub(super) bare: &'a Path,
}

impl Git<'_> {
    fn run_in(&self, dir: &Path, args: &[&str]) -> Result<String, ReplayFailure> {
        self.run_in_env(dir, args, &[])
    }

    fn run_in_env(
        &self,
        dir: &Path,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<String, ReplayFailure> {
        let out = Command::new(self.bin)
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .envs(env.iter().copied())
            .output()
            .map_err(|err| ReplayFailure::Git(err.to_string()))?;
        if !out.status.success() {
            return Err(ReplayFailure::Git(format!(
                "git {}: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    pub(super) fn run(&self, args: &[&str]) -> Result<String, ReplayFailure> {
        self.run_in(self.bare, args)
    }

    pub(super) fn resolve(&self, rev: &str) -> Option<String> {
        self.run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{rev}^{{commit}}"),
        ])
        .ok()
        .filter(|sha| is_sha(sha))
    }

    pub(super) fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        Command::new(self.bin)
            .arg("-C")
            .arg(self.bare)
            .args([
                "merge-base",
                "--is-ancestor",
                "--end-of-options",
                ancestor,
                descendant,
            ])
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Like [`Self::is_ancestor`], but a git failure is an error, not "no":
    /// `merge-base --is-ancestor` exits 1 for "not an ancestor" and higher for
    /// a real failure.
    pub(super) fn try_is_ancestor(
        &self,
        ancestor: &str,
        descendant: &str,
    ) -> Result<bool, ReplayFailure> {
        let status = Command::new(self.bin)
            .arg("-C")
            .arg(self.bare)
            .args([
                "merge-base",
                "--is-ancestor",
                "--end-of-options",
                ancestor,
                descendant,
            ])
            .status()
            .map_err(|err| ReplayFailure::Git(err.to_string()))?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(ReplayFailure::Git(format!(
                "git merge-base --is-ancestor {ancestor} {descendant} failed"
            ))),
        }
    }

    pub(super) fn update_ref(&self, name: &str, sha: &str) -> Result<(), ReplayFailure> {
        self.run(&["update-ref", name, sha]).map(|_| ())
    }

    pub(super) fn delete_ref(&self, name: &str) {
        let _ = self.run(&["update-ref", "-d", name]);
    }

    /// Store `json` as a blob and point `name` at it.
    pub(super) fn write_blob_ref(&self, name: &str, json: &str) -> Result<(), ReplayFailure> {
        let mut child = Command::new(self.bin)
            .arg("-C")
            .arg(self.bare)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| ReplayFailure::Git(err.to_string()))?;
        {
            use std::io::Write;
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| ReplayFailure::Git("no stdin".into()))?;
            stdin
                .write_all(json.as_bytes())
                .map_err(|err| ReplayFailure::Git(err.to_string()))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|err| ReplayFailure::Git(err.to_string()))?;
        let blob = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || !is_sha(&blob) {
            return Err(ReplayFailure::Git("hash-object failed".to_string()));
        }
        self.update_ref(name, &blob)
    }

    /// `(ref name, blob contents)` for every ref under `prefix`.
    pub(super) fn read_blob_refs(&self, prefix: &str) -> Vec<(String, String)> {
        self.try_read_blob_refs(prefix).unwrap_or_default()
    }

    /// [`Self::read_blob_refs`], but failing to enumerate the refs is an error.
    ///
    /// Two git processes whatever the queue holds: the audit ingest reads every
    /// `refs/queue-meta/` ref, and one `cat-file` per ref grew that cost with
    /// the queue.
    pub(super) fn try_read_blob_refs(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, ReplayFailure> {
        let listing = self.run(&["for-each-ref", "--format=%(refname) %(objectname)", prefix])?;
        let refs: Vec<(&str, &str)> = listing
            .lines()
            .filter_map(|line| line.split_once(' '))
            .collect();
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        let mut request = String::new();
        for (_, blob) in &refs {
            request.push_str(blob);
            request.push('\n');
        }
        let batch = self.run_with_stdin(&["cat-file", "--batch"], &request)?;
        let bodies = batch_bodies(&batch);
        refs.iter()
            .map(|(name, blob)| {
                bodies
                    .get(*blob)
                    .map(|body| (name.to_string(), body.clone()))
                    .ok_or_else(|| ReplayFailure::Git(format!("unreadable queue metadata: {name}")))
            })
            .collect()
    }

    /// Drain both output pipes while the input writer runs. A full batch can
    /// exceed both pipe buffers; writing it synchronously before reading
    /// output deadlocks cat-file and the caller holding the queue mutex.
    fn run_with_stdin(&self, args: &[&str], input: &str) -> Result<Vec<u8>, ReplayFailure> {
        use std::io::Write;
        let mut child = Command::new(self.bin)
            .arg("-C")
            .arg(self.bare)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| ReplayFailure::Git(err.to_string()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ReplayFailure::Git("no stdin".into()))?;
        std::thread::scope(|scope| {
            let writer = scope.spawn(move || stdin.write_all(input.as_bytes()));
            let output = child.wait_with_output();
            let written = writer
                .join()
                .map_err(|_| ReplayFailure::Git("git input writer panicked".into()))?;
            let out = output.map_err(|err| ReplayFailure::Git(err.to_string()))?;
            if !out.status.success() {
                return Err(ReplayFailure::Git(format!(
                    "git {}: {}",
                    args.first().copied().unwrap_or_default(),
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            written.map_err(|err| ReplayFailure::Git(err.to_string()))?;
            Ok(out.stdout)
        })
    }

    /// Committer time (unix seconds) of `sha`.
    pub(super) fn committer_time(&self, sha: &str) -> Result<i64, ReplayFailure> {
        let out = self.run(&["log", "-1", "--format=%ct", "--end-of-options", sha])?;
        out.trim()
            .parse()
            .map_err(|_| ReplayFailure::Git(format!("no committer time for {sha}")))
    }

    /// Replay `merge_base..pr_head` onto `base_tip` and return the new tip.
    /// When the PR already sits on the tip, the PR head itself is the queue
    /// commit (it was gated as the PR head).
    ///
    /// `committed_after` is a monotonic nonce: the replayed commits get a
    /// committer time strictly later than it, so a rebuild on an unchanged tip
    /// still yields a new sha without waiting for the clock.
    pub(super) fn replay(
        &self,
        base_tip: &str,
        pr_head: &str,
        committed_after: Option<i64>,
    ) -> Result<String, ReplayFailure> {
        if self.is_ancestor(base_tip, pr_head) {
            return Ok(pr_head.to_string());
        }
        let merge_base = self.run(&["merge-base", "--end-of-options", base_tip, pr_head])?;
        let merge_base = merge_base.trim();
        if merge_base.is_empty() {
            return Err(ReplayFailure::Git(format!(
                "no merge base between {base_tip} and {pr_head}"
            )));
        }
        let range = format!("{merge_base}..{pr_head}");
        if !self
            .run(&["rev-list", "--merges", "--end-of-options", &range])?
            .is_empty()
        {
            return Err(ReplayFailure::MergeCommits);
        }
        let commits = self.run(&["rev-list", "--reverse", "--end-of-options", &range])?;
        let committer_date = committed_after.map(|after| {
            format!(
                "@{} +0000",
                committer_seconds(chrono::Utc::now().timestamp(), after)
            )
        });
        let env: Vec<(&str, &str)> = committer_date
            .as_deref()
            .map(|date| ("GIT_COMMITTER_DATE", date))
            .into_iter()
            .collect();

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let work: PathBuf =
            std::env::temp_dir().join(format!("jeryu-merge-queue-{}-{nonce}", std::process::id()));
        let work_str = work.to_string_lossy().to_string();
        self.run(&[
            "worktree", "add", "--detach", "--quiet", &work_str, base_tip,
        ])?;
        let result = (|| {
            for commit in commits.lines().filter(|line| is_sha(line)) {
                if let Err(err) = self.run_in_env(
                    &work,
                    &[
                        "-c",
                        "user.name=Jeryu merge queue",
                        "-c",
                        "user.email=merge-queue@jeryu.invalid",
                        // Repository hooks never run on the forge's behalf.
                        "-c",
                        "core.hooksPath=/dev/null",
                        "cherry-pick",
                        "--allow-empty",
                        "--keep-redundant-commits",
                        commit,
                    ],
                    &env,
                ) {
                    let conflicted = self
                        .run_in(&work, &["diff", "--name-only", "--diff-filter=U"])
                        .map(|paths| paths.lines().map(str::to_string).collect::<Vec<_>>())
                        .unwrap_or_default();
                    let _ = self.run_in(&work, &["cherry-pick", "--abort"]);
                    return Err(if conflicted.is_empty() {
                        err
                    } else {
                        ReplayFailure::Conflict(conflicted)
                    });
                }
            }
            self.run_in(&work, &["rev-parse", "HEAD"])
        })();
        let _ = self.run(&["worktree", "remove", "--force", &work_str]);
        let _ = std::fs::remove_dir_all(&work);
        let _ = self.run(&["worktree", "prune"]);
        let queue_sha = result?;
        self.ensure_same_change(merge_base, pr_head, base_tip, &queue_sha)?;
        Ok(queue_sha)
    }

    /// The replay must carry the reviewed change exactly: the same paths with
    /// the same status (renames included), then the same hunks per path, line
    /// numbers aside.
    fn ensure_same_change(
        &self,
        merge_base: &str,
        pr_head: &str,
        base_tip: &str,
        queue_sha: &str,
    ) -> Result<(), ReplayFailure> {
        let reviewed_paths = self.name_status(merge_base, pr_head)?;
        let queued_paths = self.name_status(base_tip, queue_sha)?;
        if reviewed_paths != queued_paths {
            return Err(ReplayFailure::Mismatch(format!(
                "files differ: reviewed {reviewed_paths:?}, queued {queued_paths:?}"
            )));
        }
        if self.hunks(merge_base, pr_head)? != self.hunks(base_tip, queue_sha)? {
            return Err(ReplayFailure::Mismatch("a hunk differs".to_string()));
        }
        Ok(())
    }

    fn name_status(&self, from: &str, to: &str) -> Result<Vec<String>, ReplayFailure> {
        let out = self.run(&[
            "diff",
            "--name-status",
            "-M",
            "--no-color",
            "--end-of-options",
            from,
            to,
        ])?;
        let mut lines: Vec<String> = out
            .lines()
            // R<score> varies with context; keep the status letter and paths.
            .map(|line| {
                let mut fields = line.split('\t');
                let status = fields
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(1)
                    .collect::<String>();
                std::iter::once(status)
                    .chain(fields.map(str::to_string))
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect();
        lines.sort();
        Ok(lines)
    }

    fn hunks(&self, from: &str, to: &str) -> Result<String, ReplayFailure> {
        let out = self.run(&[
            "diff",
            "-U0",
            "-M",
            "--no-color",
            "--no-ext-diff",
            "--end-of-options",
            from,
            to,
        ])?;
        Ok(out
            .lines()
            .filter(|line| !line.starts_with("index "))
            .map(|line| {
                if line.starts_with("@@") {
                    // Positions move with the base; retain the enclosing
                    // context so the same lines in another function need review.
                    match line.rfind("@@") {
                        Some(end) if end > 1 => format!("@@{}", &line[end + 2..]),
                        _ => "@@".to_string(),
                    }
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// Committer time for a rebuild: the clock, but never at or before `after`.
fn committer_seconds(now: i64, after: i64) -> i64 {
    now.max(after.saturating_add(1))
}

pub(super) fn is_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Refs only the merge queue may write; pushes to them are refused.
pub(crate) fn is_queue_owned_ref(name: &str) -> bool {
    name.starts_with("refs/queue/") || name.starts_with("refs/queue-meta/")
}

/// `object id -> contents` for the output of `git cat-file --batch`: each
/// record is `<oid> <type> <size>` on one line, then exactly `<size>` bytes and
/// a newline. A `<oid> missing` line has no body and is skipped, so a ref whose
/// blob was pruned is absent rather than empty.
fn batch_bodies(output: &[u8]) -> std::collections::HashMap<String, String> {
    let mut bodies = std::collections::HashMap::new();
    let mut rest = output;
    while let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
        let header = String::from_utf8_lossy(&rest[..end]).to_string();
        rest = &rest[end + 1..];
        let mut fields = header.split(' ');
        let Some(oid) = fields.next() else { break };
        let (Some(_kind), Some(size)) = (fields.next(), fields.next()) else {
            continue; // `missing` or `ambiguous`: no body follows
        };
        let Ok(size) = size.parse::<usize>() else {
            break;
        };
        if rest.len() < size {
            break;
        }
        bodies.insert(
            oid.to_string(),
            String::from_utf8_lossy(&rest[..size]).to_string(),
        );
        rest = &rest[(size + 1).min(rest.len())..];
    }
    bodies
}

#[cfg(test)]
mod tests {
    use super::{batch_bodies, committer_seconds};

    #[test]
    fn committer_seconds_always_passes_the_previous_attempt() {
        assert_eq!(committer_seconds(100, 100), 101);
        assert_eq!(committer_seconds(100, 250), 251);
        assert_eq!(committer_seconds(300, 100), 300);
    }

    #[test]
    fn batch_reader_drains_output_while_writing_more_than_pipe_capacity() {
        let (send, recv) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let root = tempfile::tempdir().unwrap();
            let git = super::Git {
                bin: "git",
                bare: root.path(),
            };
            git.run(&["init", "--quiet", "--bare"]).unwrap();
            let body = "x".repeat(2048);
            git.write_blob_ref("refs/queue-meta/load/0", &body).unwrap();
            let oid = git.run(&["rev-parse", "refs/queue-meta/load/0"]).unwrap();
            let refs = (1..4096)
                .map(|n| format!("update refs/queue-meta/load/{n} {oid}\n"))
                .collect::<String>();
            git.run_with_stdin(&["update-ref", "--stdin"], &refs)
                .unwrap();
            let entries = git.try_read_blob_refs("refs/queue-meta/load/").unwrap();
            assert_eq!(entries.len(), 4096);
            assert!(entries.iter().all(|(_, value)| value == &body));
            send.send(()).unwrap();
        });
        recv.recv_timeout(std::time::Duration::from_secs(30))
            .expect("batch reader deadlocked or failed under pipe backpressure");
    }

    #[test]
    fn batch_git_failure_retains_stderr_even_when_input_pipe_closes() {
        let root = tempfile::tempdir().unwrap();
        let git = super::Git {
            bin: "git",
            bare: root.path(),
        };
        let error = git
            .run_with_stdin(&["not-a-real-git-command"], &"x".repeat(262144))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a git command"), "{error}");
    }

    #[test]
    fn batch_bodies_reads_every_record_and_skips_a_missing_one() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let gone = "c".repeat(40);
        let mut output = Vec::new();
        output.extend_from_slice(format!("{a} blob 7\n{{\"n\":1}}\n").as_bytes());
        output.extend_from_slice(format!("{gone} missing\n").as_bytes());
        output.extend_from_slice(format!("{b} blob 2\n{{}}\n").as_bytes());
        let bodies = batch_bodies(&output);
        assert_eq!(bodies.get(&a).map(String::as_str), Some("{\"n\":1}"));
        assert_eq!(bodies.get(&b).map(String::as_str), Some("{}"));
        assert!(!bodies.contains_key(&gone));
    }
}
