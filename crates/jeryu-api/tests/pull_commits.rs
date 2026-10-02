//! `GET /repos/{owner}/{repo}/pulls/{number}/commits`: the commits of a pull
//! request, oldest first, read from the real bare repository.
//!
//! A shift PR carries one commit per todo, so this list (and each commit's full
//! message with its `Todo:`/`Worked-by:`/`Shift:` trailers) is how a reviewer
//! reads what landed. The specs below pin the order, the messages, the page
//! cuts, and the 404 for a pull request the caller cannot read.
#![cfg(feature = "web")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use jeryu_api::GithubRouter;
use jeryu_core::ForgeCore;
use jeryu_gitd::{GitdConfig, RepoId, RepoManager};
use serde_json::Value;

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

/// The identity every seeded commit carries, pinned through the environment so
/// an ambient `GIT_AUTHOR_*` (the shift worker's own) never renames it.
const WHO: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Shift Worker"),
    ("GIT_AUTHOR_EMAIL", "worker@example.invalid"),
    ("GIT_COMMITTER_NAME", "Shift Worker"),
    ("GIT_COMMITTER_EMAIL", "worker@example.invalid"),
];

fn run_git(dir: &Path, args: &[&str], label: &str) {
    let status = common::git_command()
        .args(args)
        .envs(WHO)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|err| panic!("{label} failed to start: {err}"));
    assert!(status.success(), "{label} failed with {status}");
}

fn head_oid(work: &Path) -> String {
    let out = common::git_command()
        .args(["rev-parse", "HEAD"])
        .current_dir(work)
        .output()
        .expect("git rev-parse");
    assert!(out.status.success(), "rev-parse failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn body(resp: &jeryu_api::Response) -> Value {
    serde_json::from_str(&resp.body)
        .unwrap_or_else(|err| panic!("bad json body: {err}: {}", resp.body))
}

fn link(resp: &jeryu_api::Response) -> String {
    resp.headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("Link"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

/// A bare `acme/demo.git` with `main` at a seed commit and `feature` three
/// commits ahead of it, each message shaped like a shift commit.
struct Fixture {
    root: PathBuf,
    work: PathBuf,
    router: GithubRouter,
    number: u64,
    head: String,
}

impl Fixture {
    fn cleanup(self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.work);
    }
}

const SUBJECTS: [&str; 3] = ["first todo", "second todo", "third todo"];

fn seed(prefix: &str) -> Fixture {
    let root = temp_dir(&format!("{prefix}-root"));
    let work = temp_dir(&format!("{prefix}-work"));
    let manager = Arc::new(RepoManager::new(GitdConfig::new(&root)));
    let id = RepoId::new("acme", "demo").unwrap();
    let bare = manager.create_bare(&id).expect("create bare");
    let bare_path = bare.path.to_str().unwrap().to_owned();

    run_git(&work, &["init"], "git init");
    std::fs::write(work.join("README.md"), "hello\n").expect("write");
    run_git(&work, &["add", "README.md"], "git add");
    run_git(&work, &["commit", "-m", "seed"], "git commit seed");
    run_git(
        &work,
        &["push", &bare_path, "HEAD:refs/heads/main"],
        "push main",
    );
    let base = head_oid(&work);

    for (index, subject) in SUBJECTS.iter().enumerate() {
        std::fs::write(work.join(format!("file-{index}.txt")), "x\n").expect("write");
        run_git(&work, &["add", "."], "git add");
        run_git(
            &work,
            &[
                "commit",
                "-m",
                subject,
                "-m",
                &format!("Todo: 2026092{index}\nWorked-by: shift worker\nShift: dayshift"),
            ],
            "git commit",
        );
    }
    run_git(
        &work,
        &["push", &bare_path, "HEAD:refs/heads/feature"],
        "push feature",
    );
    let head = head_oid(&work);

    let router = GithubRouter::with_core(ForgeCore::new()).with_repo_manager(manager);
    let created = router.post(
        "/repos",
        r#"{"owner":"acme","name":"demo","private":false,"default_branch":"main"}"#,
    );
    assert_eq!(created.status, 201, "create repo: {}", created.body);
    let opened = router.post(
        "/repos/acme/demo/pulls",
        &format!(
            r#"{{"title":"three todos","head":"feature","base":"main","head_sha":"{head}","base_sha":"{base}","actor":"alice"}}"#
        ),
    );
    assert_eq!(opened.status, 201, "open pr: {}", opened.body);
    let number = body(&opened)["number"].as_u64().expect("pr number");

    Fixture {
        root,
        work,
        router,
        number,
        head,
    }
}

#[test]
fn a_pull_request_lists_its_three_commits_oldest_first_with_messages() {
    if !git_available() {
        return;
    }
    let fixture = seed("jeryu-pull-commits-order");
    let path = format!("/repos/acme/demo/pulls/{}/commits", fixture.number);
    let resp = fixture.router.get(&path);
    assert_eq!(resp.status, 200, "list commits: {}", resp.body);
    let commits = body(&resp);
    let commits = commits.as_array().expect("array body");
    assert_eq!(commits.len(), 3, "three commits: {commits:?}");

    for (commit, subject) in commits.iter().zip(SUBJECTS) {
        let message = commit["commit"]["message"].as_str().expect("message");
        assert!(
            message.starts_with(subject),
            "expected {subject} first in {message}"
        );
        assert!(
            message.contains("Worked-by: shift worker"),
            "trailers kept in {message}"
        );
        assert_eq!(commit["commit"]["author"]["name"], WHO[0].1);
        assert_eq!(commit["commit"]["committer"]["email"], WHO[3].1);
        assert_eq!(commit["parents"].as_array().expect("parents").len(), 1);
        let sha = commit["sha"].as_str().expect("sha");
        assert_eq!(sha.len(), 40, "full sha: {sha}");
        assert!(
            commit["html_url"]
                .as_str()
                .expect("html_url")
                .ends_with(&format!("/repos/jeryu/acme/demo/commit/{sha}")),
            "html_url points at the commit: {commit:?}"
        );
    }
    // The newest commit of the range is the PR head; the base commit is not in it.
    assert_eq!(commits[2]["sha"], fixture.head.as_str());

    // The single-PR read reports the same count.
    let pull = fixture
        .router
        .get(&format!("/repos/acme/demo/pulls/{}", fixture.number));
    assert_eq!(
        body(&pull)["commits"],
        3,
        "pull commits count: {}",
        pull.body
    );

    fixture.cleanup();
}

#[test]
fn pull_commits_paginate_oldest_first_and_keep_the_query_in_link() {
    if !git_available() {
        return;
    }
    let fixture = seed("jeryu-pull-commits-pages");
    let path = format!("/repos/acme/demo/pulls/{}/commits", fixture.number);

    let first = fixture.router.get(&format!("{path}?per_page=2&page=1"));
    assert_eq!(first.status, 200, "page 1: {}", first.body);
    let page1 = body(&first);
    let page1 = page1.as_array().expect("array body");
    assert_eq!(page1.len(), 2);
    assert!(
        page1[0]["commit"]["message"]
            .as_str()
            .expect("message")
            .starts_with(SUBJECTS[0])
    );
    assert!(
        page1[1]["commit"]["message"]
            .as_str()
            .expect("message")
            .starts_with(SUBJECTS[1])
    );
    let first_link = link(&first);
    assert!(
        first_link.contains(&format!("{path}?per_page=2&page=2")) && first_link.contains("next"),
        "next link: {first_link}"
    );

    let second = fixture.router.get(&format!("{path}?per_page=2&page=2"));
    let page2 = body(&second);
    let page2 = page2.as_array().expect("array body");
    assert_eq!(page2.len(), 1, "last page holds the remainder");
    assert!(
        page2[0]["commit"]["message"]
            .as_str()
            .expect("message")
            .starts_with(SUBJECTS[2])
    );
    assert!(
        link(&second).contains("prev"),
        "prev link: {}",
        link(&second)
    );

    // Past the end: an empty page, never an error.
    let third = fixture.router.get(&format!("{path}?per_page=2&page=9"));
    assert_eq!(third.status, 200, "page 9: {}", third.body);
    assert!(body(&third).as_array().expect("array body").is_empty());

    fixture.cleanup();
}

#[test]
fn commits_of_a_pull_request_the_caller_cannot_read_are_a_404() {
    if !git_available() {
        return;
    }
    let fixture = seed("jeryu-pull-commits-404");

    // A repository this router does not serve, and a PR number it never issued:
    // both answer exactly as `GET /pulls/{number}` does.
    let other_repo = fixture.router.get("/repos/acme/secret/pulls/1/commits");
    assert_eq!(
        other_repo.status, 404,
        "unreadable repo: {}",
        other_repo.body
    );
    let missing_pr = fixture.router.get(&format!(
        "/repos/acme/demo/pulls/{}/commits",
        fixture.number + 7
    ));
    assert_eq!(missing_pr.status, 404, "unknown pr: {}", missing_pr.body);

    fixture.cleanup();
}
