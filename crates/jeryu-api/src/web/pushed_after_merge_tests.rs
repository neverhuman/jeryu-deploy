//! A branch after its pull request closed, over a real bare repository: what
//! the push says back, and when it says nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Duration, Utc};
use jeryu_core::{
    CreatePullRequestRequest, CreateRepositoryRequest, ForgeCore, PullRequestState,
    UpdatePullRequestRequest,
};

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

/// `alice/jeryu` with `main`, a `feature` branch one commit ahead of it, an
/// unrelated `spike` branch, and an open pull request for `feature`.
struct Fixture {
    _storage: tempfile::TempDir,
    bare: PathBuf,
    work: PathBuf,
    state: Arc<WebState>,
    number: u64,
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
        git(&work, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(work.join("feature.txt"), "feature\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "feature"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        // A branch nobody opened a pull request for, so the rule has something
        // it must stay quiet about.
        git(&work, &["checkout", "--quiet", "-b", "spike", &base]);
        std::fs::write(work.join("spike.txt"), "spike\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "spike"]);
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
        let number = Self::open_pull(&core, &head, &base);
        let state = Arc::new(WebState::new_with_git_storage(
            core,
            storage.path().to_path_buf(),
        ));
        Self {
            _storage: storage,
            bare,
            work,
            state,
            number,
        }
    }

    fn open_pull(core: &ForgeCore, head: &str, base: &str) -> u64 {
        core.create_pull_request(
            "alice",
            "jeryu",
            "alice",
            CreatePullRequestRequest {
                title: "the feature".to_string(),
                head: "feature".to_string(),
                base: "main".to_string(),
                head_sha: Some(head.to_string()),
                base_sha: Some(base.to_string()),
                ..CreatePullRequestRequest::default()
            },
        )
        .expect("open pull request")
        .number
    }

    /// Merge the pull request the way the forge does: the record is merged and
    /// `main` carries its head.
    fn merge(&self) {
        let head = git(&self.bare, &["rev-parse", "refs/heads/feature"]);
        git(&self.bare, &["update-ref", "refs/heads/main", &head]);
        self.state
            .core
            .finalize_merge("alice", "jeryu", self.number, head, None)
            .expect("merge the pull request");
    }

    /// Close the pull request without merging it.
    fn close(&self) {
        self.state
            .core
            .update_pull_request(
                "alice",
                "jeryu",
                self.number,
                UpdatePullRequestRequest {
                    state: Some(PullRequestState::Closed),
                    ..UpdatePullRequestRequest::default()
                },
            )
            .expect("close the pull request");
    }

    /// One more commit on `feature`, as a push to the merged branch leaves it.
    fn push_to_feature(&self, body: &str) -> String {
        git(&self.work, &["checkout", "--quiet", "feature"]);
        std::fs::write(self.work.join("late.txt"), body).unwrap();
        git(&self.work, &["add", "."]);
        git(&self.work, &["commit", "--quiet", "-m", "late work"]);
        let head = git(&self.work, &["rev-parse", "HEAD"]);
        git(
            &self.bare,
            &[
                "fetch",
                "--quiet",
                self.work.to_str().unwrap(),
                "+refs/heads/feature:refs/heads/feature",
            ],
        );
        head
    }

    fn stranded(&self, branch: &str) -> Option<StrandedPush> {
        stranded(&self.state, "alice", "jeryu", branch, Utc::now())
    }
}

#[test]
fn a_commit_pushed_after_the_merge_names_the_pull_request_it_missed() {
    let fixture = Fixture::new();
    fixture.merge();
    let blocked = merged_pull_request_branch(&fixture.state, "alice", "jeryu", "feature")
        .expect("the merged pull request branch is blocked");
    assert_eq!(blocked.branch, "feature");
    assert_eq!(blocked.number, fixture.number);
    assert_eq!(
        receive_refusal_message(&blocked),
        format!(
            "this branch's pull request #{} was already merged; push to a new branch and open a new pull request",
            fixture.number
        )
    );
    // The branch is on its base: the merge took everything there was.
    assert_eq!(fixture.stranded("feature"), None);

    let late = fixture.push_to_feature("late\n");
    let stranded = fixture.stranded("feature").expect("the push is stranded");
    assert_eq!(stranded.repo, "alice/jeryu");
    assert_eq!(stranded.branch, "feature");
    assert_eq!(stranded.number, fixture.number);
    assert_eq!(stranded.base_ref, "main");
    assert!(stranded.merged);
    assert_eq!(stranded.commit_count, 1);
    assert_eq!(stranded.commits.len(), 1);
    assert!(
        late.starts_with(&stranded.commits[0]),
        "the stranded commit is the one just pushed: {stranded:?} vs {late}"
    );

    let message = remote_message(&stranded).join("\n");
    assert!(
        message.contains(&format!("alice/jeryu #{}", fixture.number)),
        "{message}"
    );
    assert!(message.contains("already merged"), "{message}");
    assert!(message.contains("not on main"), "{message}");
    assert!(
        message.contains("open a new pull request from feature"),
        "{message}"
    );
}

#[test]
fn a_branch_whose_pull_request_was_closed_unmerged_says_so() {
    let fixture = Fixture::new();
    fixture.close();
    assert_eq!(
        merged_pull_request_branch(&fixture.state, "alice", "jeryu", "feature"),
        None,
        "only a merged pull request blocks receive-pack"
    );
    let stranded = fixture
        .stranded("feature")
        .expect("a closed pull request strands its branch too");
    assert!(!stranded.merged);
    let message = remote_message(&stranded).join("\n");
    assert!(message.contains("closed without merging"), "{message}");
}

#[test]
fn a_branch_no_pull_request_ever_had_says_nothing() {
    let fixture = Fixture::new();
    fixture.merge();
    fixture.push_to_feature("late\n");
    assert_eq!(fixture.stranded("spike"), None);
    assert_eq!(fixture.stranded("main"), None);
    // Only the merged branch's own push is waiting on anybody.
    let all = all(&fixture.state, Utc::now());
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].branch, "feature");
}

#[test]
fn an_open_pull_request_from_the_branch_clears_it() {
    let fixture = Fixture::new();
    fixture.merge();
    let late = fixture.push_to_feature("late\n");
    assert!(fixture.stranded("feature").is_some());

    let base = git(&fixture.bare, &["rev-parse", "refs/heads/main"]);
    Fixture::open_pull(&fixture.state.core, &late, &base);
    assert_eq!(
        merged_pull_request_branch(&fixture.state, "alice", "jeryu", "feature"),
        None,
        "an open pull request from the branch accepts pushes again"
    );
    assert_eq!(
        fixture.stranded("feature"),
        None,
        "a new pull request carries the commits"
    );
    assert!(all(&fixture.state, Utc::now()).is_empty());
}

#[test]
fn a_pull_request_that_closed_long_ago_is_the_branchs_own_business() {
    let fixture = Fixture::new();
    fixture.merge();
    fixture.push_to_feature("late\n");
    let later = Utc::now() + Duration::days(LOOKBACK_DAYS + 1);
    assert_eq!(
        stranded(&fixture.state, "alice", "jeryu", "feature", later),
        None
    );
    assert_eq!(
        merged_pull_request_branch_at(&fixture.state, "alice", "jeryu", "feature", later),
        None
    );
}

#[test]
fn fork_pull_requests_do_not_own_upstream_branch_names() {
    let fixture = Fixture::new();
    let base = git(&fixture.bare, &["rev-parse", "refs/heads/main"]);
    let head = git(&fixture.bare, &["rev-parse", "refs/heads/feature"]);
    let fork_pull = fixture
        .state
        .core
        .create_pull_request(
            "alice",
            "jeryu",
            "fork-user",
            CreatePullRequestRequest {
                title: "fork feature".into(),
                head: "feature".into(),
                base: "main".into(),
                source_repository: Some("fork-user/jeryu".into()),
                head_sha: Some(head.clone()),
                base_sha: Some(base),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    fixture.close();
    fixture
        .state
        .core
        .finalize_merge("alice", "jeryu", fork_pull.number, head, None)
        .unwrap();
    assert!(merged_pull_request_branch(&fixture.state, "alice", "jeryu", "feature").is_none());
    // A fork PR cannot suppress a same-repository closed-branch notice either.
    assert_eq!(fixture.stranded("feature").unwrap().number, fixture.number);
}
