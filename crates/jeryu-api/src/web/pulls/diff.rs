//! Pull request diffs computed from the forge's bare repository.
//!
//! The stored `changed_files` list is whatever the PR creator supplied (empty
//! for GitHub-compatible creates), so the review cockpit reads the real
//! `git diff base...head` instead and only falls back to the stored list when
//! git cannot answer (repo not materialized, objects missing).

use super::*;

/// Diff body lines served before the response is marked `truncated`.
const MAX_DIFF_LINES: usize = 20_000;

pub(super) fn pull_request_diff(state: &WebState, pr: &PullRequest) -> PullRequestDiff {
    let files = git_diff_files(state, pr).unwrap_or_else(|| stored_files(pr));
    let (files, truncated) = cap_lines(files);
    PullRequestDiff {
        head_sha: pr.head.sha.clone(),
        base_sha: pr.base.sha.clone(),
        files,
        truncated,
    }
}

fn stored_files(pr: &PullRequest) -> Vec<PullRequestDiffFile> {
    pr.changed_files
        .iter()
        .map(|path| empty_file(path.clone(), None, "modified"))
        .collect()
}

fn git_diff_files(state: &WebState, pr: &PullRequest) -> Option<Vec<PullRequestDiffFile>> {
    if pr.base.sha.is_empty() || pr.head.sha.is_empty() {
        return None;
    }
    let resolved = state.repo_manager.resolve_parts(&pr.owner, &pr.repo).ok()?;
    let output = std::process::Command::new(&state.repo_manager.config().git_bin)
        .arg("-C")
        .arg(&resolved.path)
        .args([
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            "--unified=3",
        ])
        .arg(format!("{}...{}", pr.base.sha, pr.head.sha))
        .arg("--")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_unified_diff(&String::from_utf8_lossy(&output.stdout)))
}

fn empty_file(path: String, old_path: Option<String>, status: &'static str) -> PullRequestDiffFile {
    PullRequestDiffFile {
        path,
        old_path,
        status,
        additions: 0,
        deletions: 0,
        risk: None,
        is_binary: false,
        hunks: Vec::new(),
    }
}

/// Parses `git diff` output into per-file entries with hunks and line counts.
pub(in crate::web) fn parse_unified_diff(text: &str) -> Vec<PullRequestDiffFile> {
    let mut files: Vec<PullRequestDiffFile> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            files.push(header_file(rest));
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if let Some(hunk) = file.hunks.last_mut() {
            match line.as_bytes().first() {
                Some(b'+') => {
                    file.additions += 1;
                    hunk.lines.push(line.to_string());
                    continue;
                }
                Some(b'-') => {
                    file.deletions += 1;
                    hunk.lines.push(line.to_string());
                    continue;
                }
                Some(b' ') | Some(b'\\') => {
                    hunk.lines.push(line.to_string());
                    continue;
                }
                _ => {}
            }
        }
        if let Some(hunk) = parse_hunk_header(line) {
            file.hunks.push(hunk);
        } else if line.starts_with("new file mode") {
            file.status = "added";
        } else if line.starts_with("deleted file mode") {
            file.status = "removed";
        } else if let Some(from) = line.strip_prefix("rename from ") {
            file.status = "renamed";
            file.old_path = Some(from.to_string());
        } else if let Some(to) = line.strip_prefix("rename to ") {
            file.path = to.to_string();
        } else if line.starts_with("Binary files ") {
            file.is_binary = true;
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            file.path = path.to_string();
        } else if let Some(path) = line.strip_prefix("--- a/")
            && file.status == "removed"
        {
            file.path = path.to_string();
        }
    }
    files
}

/// `a/<old> b/<new>` — ambiguous when paths contain ` b/`, so later
/// `+++`/`rename to` lines overwrite the guess.
fn header_file(rest: &str) -> PullRequestDiffFile {
    let path = rest
        .rsplit_once(" b/")
        .map(|(_, new)| new)
        .unwrap_or(rest)
        .to_string();
    empty_file(path, None, "modified")
}

fn parse_hunk_header(line: &str) -> Option<PullRequestDiffHunk> {
    let body = line.strip_prefix("@@ -")?;
    let (ranges, _) = body.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let (old_start, old_lines) = parse_range(old)?;
    let (new_start, new_lines) = parse_range(new)?;
    Some(PullRequestDiffHunk {
        header: line.to_string(),
        old_start,
        old_lines,
        new_start,
        new_lines,
        lines: Vec::new(),
    })
}

fn parse_range(range: &str) -> Option<(u32, u32)> {
    match range.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

/// Drops hunk bodies past the line budget; file entries and counts are kept.
///
/// Shared with the single-commit route, which serves the same diff shape.
pub(in crate::web) fn cap_lines(
    mut files: Vec<PullRequestDiffFile>,
) -> (Vec<PullRequestDiffFile>, bool) {
    let mut budget = MAX_DIFF_LINES;
    let mut truncated = false;
    for file in &mut files {
        let lines: usize = file.hunks.iter().map(|hunk| hunk.lines.len()).sum();
        if lines > budget {
            file.hunks.clear();
            truncated = true;
        } else {
            budget -= lines;
        }
    }
    (files, truncated)
}

#[cfg(test)]
mod tests {
    use super::parse_unified_diff;

    const SAMPLE: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@ fn main() {
 keep
-old
+new
+added
 tail
diff --git a/docs/new.md b/docs/new.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/docs/new.md
@@ -0,0 +1 @@
+hello
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 4444444..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/old/name.rs b/new/name.rs
similarity index 100%
rename from old/name.rs
rename to new/name.rs
diff --git a/logo.png b/logo.png
index 5555555..6666666 100644
Binary files a/logo.png and b/logo.png differ
";

    #[test]
    fn parses_modified_added_removed_renamed_and_binary_files() {
        let files = parse_unified_diff(SAMPLE);
        assert_eq!(files.len(), 5);

        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!(files[0].status, "modified");
        assert_eq!((files[0].additions, files[0].deletions), (2, 1));
        let hunk = &files[0].hunks[0];
        assert_eq!(
            (
                hunk.old_start,
                hunk.old_lines,
                hunk.new_start,
                hunk.new_lines
            ),
            (1, 3, 1, 4)
        );
        assert_eq!(hunk.lines, vec![" keep", "-old", "+new", "+added", " tail"]);

        assert_eq!(
            (files[1].path.as_str(), files[1].status),
            ("docs/new.md", "added")
        );
        assert_eq!(files[1].hunks[0].new_lines, 1);
        assert_eq!(
            (files[2].path.as_str(), files[2].status),
            ("gone.txt", "removed")
        );
        assert_eq!(files[2].deletions, 1);
        assert_eq!(
            (files[3].path.as_str(), files[3].status),
            ("new/name.rs", "renamed")
        );
        assert_eq!(files[3].old_path.as_deref(), Some("old/name.rs"));
        assert!(files[4].is_binary);
        assert!(files[4].hunks.is_empty());
    }
}
