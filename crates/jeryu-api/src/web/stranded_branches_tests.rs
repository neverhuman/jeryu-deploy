use std::path::{Path, PathBuf};
use std::sync::Arc;

use jeryu_core::{CreatePullRequestRequest, CreateRepositoryRequest, ForgeCore};

use super::*;

fn git(cwd: &Path, args: &[&str]) -> String {
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

struct Fixture {
    _storage: tempfile::TempDir,
    _work: PathBuf,
    state: Arc<WebState>,
    dayshift_sha: String,
    closed_late_sha: String,
}

impl Fixture {
    fn new() -> Self {
        let storage = tempfile::tempdir().expect("storage");
        let bare = storage.path().join("alice").join("jeryu.git");
        let work = storage.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "--quiet", "-b", "main"]);
        std::fs::write(work.join("shared.txt"), "one\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "base"]);
        let base = git(&work, &["rev-parse", "HEAD"]);

        git(
            &work,
            &["checkout", "--quiet", "-b", "dayshift/2026-10-11", &base],
        );
        std::fs::write(work.join("dayshift.txt"), "dayshift\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "dayshift work"]);
        let dayshift_sha = git(&work, &["rev-parse", "HEAD"]);

        git(
            &work,
            &["checkout", "--quiet", "-b", "landed-by-patch", &base],
        );
        std::fs::write(work.join("landed.txt"), "landed\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "landed by cherry-pick"]);
        let landed_sha = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["checkout", "--quiet", "main"]);
        git(&work, &["cherry-pick", "--quiet", &landed_sha]);

        git(&work, &["checkout", "--quiet", "-b", "feature/open-pr"]);
        std::fs::write(work.join("open.txt"), "open\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "open pull work"]);
        let open_sha = git(&work, &["rev-parse", "HEAD"]);
        let open_base = git(&work, &["rev-parse", "main"]);

        git(&work, &["checkout", "--quiet", "main"]);
        git(&work, &["checkout", "--quiet", "-b", "feature/closed-pr"]);
        std::fs::write(work.join("closed.txt"), "closed\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "closed pull work"]);
        let closed_pr_head = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["checkout", "--quiet", "main"]);
        git(&work, &["merge", "--quiet", "--ff-only", &closed_pr_head]);
        git(&work, &["checkout", "--quiet", "feature/closed-pr"]);
        std::fs::write(work.join("later.txt"), "later\n").unwrap();
        git(&work, &["add", "."]);
        git(
            &work,
            &["commit", "--quiet", "-m", "later closed-branch work"],
        );
        let closed_late_sha = git(&work, &["rev-parse", "HEAD"]);

        git(&work, &["checkout", "--quiet", "main"]);
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git(
            &work,
            &["clone", "--quiet", "--bare", ".", bare.to_str().unwrap()],
        );
        git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        let core = ForgeCore::new();
        core.create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
        core.create_pull_request(
            "alice",
            "jeryu",
            "alice",
            CreatePullRequestRequest {
                title: "open feature".to_string(),
                head: "feature/open-pr".to_string(),
                base: "main".to_string(),
                head_sha: Some(open_sha),
                base_sha: Some(open_base),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
        let closed = core
            .create_pull_request(
                "alice",
                "jeryu",
                "alice",
                CreatePullRequestRequest {
                    title: "closed feature".to_string(),
                    head: "feature/closed-pr".to_string(),
                    base: "main".to_string(),
                    head_sha: Some(closed_pr_head.clone()),
                    base_sha: Some(base),
                    ..CreatePullRequestRequest::default()
                },
            )
            .unwrap();
        core.finalize_merge("alice", "jeryu", closed.number, closed_pr_head, None)
            .unwrap();
        let state = Arc::new(WebState::new_with_git_storage(
            core,
            storage.path().to_path_buf(),
        ));
        Self {
            _storage: storage,
            _work: work,
            state,
            dayshift_sha,
            closed_late_sha,
        }
    }
}

#[test]
fn sweep_reports_stranded_dayshift_and_closed_pr_branches_only() {
    let fixture = Fixture::new();
    let mut branches = all_at_round(&fixture.state, 0);
    branches.sort_by(|a, b| a.branch.cmp(&b.branch));

    let names: Vec<_> = branches
        .iter()
        .map(|branch| branch.branch.as_str())
        .collect();
    assert_eq!(
        names,
        ["dayshift/2026-10-11", "feature/closed-pr"],
        "cherry-picked work and open pull request branches are quiet"
    );

    let dayshift = branches
        .iter()
        .find(|branch| branch.branch == "dayshift/2026-10-11")
        .unwrap();
    assert_eq!(dayshift.repo, "alice/jeryu");
    assert_eq!(dayshift.commit_count, 1);
    assert_eq!(dayshift.newest_sha, fixture.dayshift_sha);
    assert_eq!(dayshift.newest_subject, "dayshift work");
    assert_eq!(dayshift.closed_pr, None);

    let closed = branches
        .iter()
        .find(|branch| branch.branch == "feature/closed-pr")
        .unwrap();
    assert_eq!(closed.commit_count, 1);
    assert_eq!(closed.newest_sha, fixture.closed_late_sha);
    assert_eq!(closed.newest_subject, "later closed-branch work");
    assert_eq!(closed.closed_pr, Some(2));
}

#[test]
fn large_repository_scans_are_bounded_and_rotate_to_later_branches() {
    let fixture = Fixture::new();
    let bare = fixture._storage.path().join("alice/jeryu.git");
    for n in 0..70 {
        git(
            &bare,
            &[
                "update-ref",
                &format!("refs/heads/extra-{n:03}"),
                &fixture.dayshift_sha,
            ],
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    for round in 0..8 {
        let rows = all_at_round(&fixture.state, round);
        assert!(rows.len() <= MAX_BRANCHES_PER_REPO);
        seen.extend(rows.into_iter().map(|row| row.branch));
    }
    for n in 0..70 {
        assert!(
            seen.contains(&format!("extra-{n:03}")),
            "branch {n} was starved"
        );
    }
    for n in 0..4 {
        let name = format!("extra-repo-{n}");
        fixture
            .state
            .core
            .create_repository(
                "alice",
                CreateRepositoryRequest {
                    name: name.clone(),
                    private: false,
                    description: None,
                    default_branch: Some("main".to_string()),
                },
            )
            .unwrap();
        let destination = fixture
            ._storage
            .path()
            .join("alice")
            .join(format!("{name}.git"));
        git(
            &fixture._work,
            &[
                "clone",
                "--quiet",
                "--bare",
                bare.to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
    }
    let rows = all_at_round(&fixture.state, 0);
    assert_eq!(
        rows.len(),
        MAX_SCAN_BRANCHES,
        "global budget applies across repositories"
    );
}

#[test]
fn inbox_scan_is_repeatable_within_a_time_window_and_rotates_after_it() {
    let fixture = Fixture::new();
    let bare = fixture._storage.path().join("alice/jeryu.git");
    for n in 0..32 {
        git(
            &bare,
            &[
                "update-ref",
                &format!("refs/heads/window-{n:03}"),
                &fixture.dayshift_sha,
            ],
        );
    }
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-13T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let first = all_at_time(&fixture.state, at);
    assert_eq!(first, all_at_time(&fixture.state, at));
    assert_eq!(
        first,
        all_at_time(&fixture.state, at + chrono::Duration::seconds(30))
    );
    let next = all_at_time(&fixture.state, at + chrono::Duration::seconds(60));
    assert_ne!(first, next);
    assert!(first.len() <= MAX_BRANCHES_PER_REPO && next.len() <= MAX_BRANCHES_PER_REPO);
}
