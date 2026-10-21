//! The green ref end to end: a green `<repo>/main-health` status posted through
//! the REST edge moves `refs/green/main` in the real bare repository, under
//! compare-and-swap, and nothing else does.
//!
//! The ref is what the release automation releases from, so each test here asks
//! git what the ref points at after the status, never the forge's own summary.
#![cfg(feature = "web")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use jeryu_api::GithubRouter;
use jeryu_core::ForgeCore;
use jeryu_gitd::refs::RefService;
use jeryu_gitd::{GitdConfig, RepoId, RepoManager};

mod common;

fn git_available() -> bool {
    common::git_command()
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn temp_dir(prefix: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "{}-{}",
        prefix,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).expect("create temp dir");
    base
}

fn run_git(dir: &Path, args: &[&str], label: &str) {
    let status = common::git_command()
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|err| panic!("{label} failed to start: {err}"));
    assert!(status.success(), "{label} failed with {status}");
}

fn rev_parse_head(work: &Path) -> String {
    let out = common::git_command()
        .args(["rev-parse", "HEAD"])
        .current_dir(work)
        .output()
        .expect("git rev-parse");
    assert!(out.status.success(), "rev-parse failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare `acme/demo.git` plus a work tree that pushes commits into it.
struct Fixture {
    root: PathBuf,
    work: PathBuf,
    manager: Arc<RepoManager>,
    router: GithubRouter,
}

impl Fixture {
    fn new(prefix: &str) -> Self {
        let root = temp_dir(&format!("{prefix}-root"));
        let work = temp_dir(&format!("{prefix}-work"));
        let manager = Arc::new(RepoManager::new(GitdConfig::new(&root)));
        let id = RepoId::new("acme", "demo").expect("repo id");
        manager.create_bare(&id).expect("create bare");

        run_git(&work, &["init", "-b", "main"], "git init");
        run_git(
            &work,
            &["config", "user.email", "test@example.invalid"],
            "config email",
        );
        run_git(&work, &["config", "user.name", "Test"], "config name");

        let router =
            GithubRouter::with_core(ForgeCore::new()).with_repo_manager(Arc::clone(&manager));
        let created = router.post(
            "/repos",
            r#"{"owner":"acme","name":"demo","private":false,"default_branch":"main"}"#,
        );
        assert_eq!(created.status, 201, "create repo: {}", created.body);

        let fixture = Fixture {
            root,
            work,
            manager,
            router,
        };
        fixture.commit("seed", "seed\n");
        fixture.push("main");
        fixture
    }

    fn cleanup(self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.work);
    }

    /// Commit `content` to `README.md` in the work tree and return its oid.
    fn commit(&self, message: &str, content: &str) -> String {
        std::fs::write(self.work.join("README.md"), content).expect("write readme");
        run_git(&self.work, &["add", "README.md"], "git add");
        run_git(&self.work, &["commit", "-m", message], "git commit");
        rev_parse_head(&self.work)
    }

    fn bare_path(&self) -> PathBuf {
        let id = RepoId::new("acme", "demo").expect("repo id");
        self.manager.resolve(&id).expect("resolve").path
    }

    /// Push the work tree's HEAD onto `branch` in the bare repository.
    fn push(&self, branch: &str) {
        let bare = self.bare_path();
        run_git(
            &self.work,
            &[
                "push",
                "--force",
                bare.to_str().expect("bare utf8"),
                &format!("HEAD:refs/heads/{branch}"),
            ],
            "push branch",
        );
    }

    /// Post one commit status through the REST edge.
    fn post_status(&self, sha: &str, context: &str, state: &str) {
        let posted = self.router.post(
            &format!("/repos/acme/demo/statuses/{sha}"),
            &format!(
                r#"{{"state":"{state}","context":"{context}","description":"gate; cause: none","actor":"gate-runner"}}"#
            ),
        );
        assert_eq!(posted.status, 201, "post status: {}", posted.body);
    }

    /// What `refs/green/main` names now, straight from git.
    fn green(&self) -> Option<String> {
        let id = RepoId::new("acme", "demo").expect("repo id");
        let repo = self.manager.resolve(&id).expect("resolve");
        RefService::new((*self.manager).clone())
            .resolve_commit(&repo, "refs/green/main")
            .expect("resolve green ref")
    }
}

#[test]
fn a_green_main_health_status_moves_the_green_ref_onto_mains_head() {
    if !git_available() {
        return;
    }
    let fixture = Fixture::new("jeryu-green-ref-first");
    let seed = rev_parse_head(&fixture.work);
    assert_eq!(
        fixture.green(),
        None,
        "no green ref before the first verdict"
    );

    fixture.post_status(&seed, "demo/main-health", "success");
    assert_eq!(fixture.green().as_deref(), Some(seed.as_str()));

    // A second tip, proved in turn, advances the ref.
    let second = fixture.commit("second", "second\n");
    fixture.push("main");
    fixture.post_status(&second, "demo/main-health", "success");
    assert_eq!(fixture.green().as_deref(), Some(second.as_str()));

    // A green verdict about the commit main has moved past arrives late; it
    // must not rewind what the release automation reads.
    fixture.post_status(&seed, "demo/main-health", "success");
    assert_eq!(
        fixture.green().as_deref(),
        Some(second.as_str()),
        "an older commit's verdict rewound the green ref"
    );
    fixture.cleanup();
}

#[test]
fn only_a_green_main_health_verdict_about_main_moves_the_ref() {
    if !git_available() {
        return;
    }
    let fixture = Fixture::new("jeryu-green-ref-refusals");
    let seed = rev_parse_head(&fixture.work);

    // A failing gate run on main's own head.
    fixture.post_status(&seed, "demo/main-health", "failure");
    assert_eq!(fixture.green(), None, "a red main moved the green ref");

    // The required gate's green verdict is about a pull-request head or a
    // queue commit, not about main, so it proves nothing here.
    fixture.post_status(&seed, "demo/required", "success");
    assert_eq!(fixture.green(), None, "another context moved the green ref");

    // A commit that is not on main, under the right context and state: it is
    // pushed to another branch only, so main still carries just the seed.
    let feature = fixture.commit("feature", "feature\n");
    fixture.push("feature");
    fixture.post_status(&feature, "demo/main-health", "success");
    assert_eq!(
        fixture.green(),
        None,
        "a commit main does not carry became the green ref"
    );

    // And the same commit once main has fast-forwarded onto it.
    fixture.push("main");
    fixture.post_status(&feature, "demo/main-health", "success");
    assert_eq!(fixture.green().as_deref(), Some(feature.as_str()));
    fixture.cleanup();
}

#[test]
fn a_rewritten_main_takes_the_green_ref_with_it() {
    if !git_available() {
        return;
    }
    let fixture = Fixture::new("jeryu-green-ref-rewrite");
    let seed = rev_parse_head(&fixture.work);
    let dropped = fixture.commit("dropped", "dropped\n");
    fixture.push("main");
    fixture.post_status(&dropped, "demo/main-health", "success");
    assert_eq!(fixture.green().as_deref(), Some(dropped.as_str()));

    // A human rewrites main: the green commit is no longer one main carries.
    // The next green verdict has to follow main, or the ref would be stuck on
    // an unreachable commit for good.
    run_git(&fixture.work, &["reset", "--hard", &seed], "reset main");
    let replacement = fixture.commit("replacement", "replacement\n");
    fixture.push("main");
    fixture.post_status(&replacement, "demo/main-health", "success");
    assert_eq!(fixture.green().as_deref(), Some(replacement.as_str()));
    fixture.cleanup();
}
