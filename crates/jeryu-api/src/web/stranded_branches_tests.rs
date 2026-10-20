use std::path::{Path, PathBuf};
use std::sync::Arc;

use jeryu_core::{CreatePullRequestRequest, CreateRepositoryRequest, ForgeCore};

use super::*;
use std::collections::BTreeMap;

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
    assert_eq!(first.len(), MAX_BRANCHES_PER_REPO);
    // Rows from the earlier window stay until their branch is looked at again.
    assert!(
        first
            .iter()
            .all(|row| next.iter().any(|n| n.branch == row.branch))
    );
    assert!(next.len() > first.len());
    let stamps: std::collections::BTreeSet<_> = next.iter().map(|row| row.scanned_at).collect();
    assert_eq!(stamps.len(), 2, "each row carries its own pass's time");
}

// ---- Landed work and custody refs -------------------------------------

use jeryu_core::{PullRequestState, UpdatePullRequestRequest};

impl Fixture {
    fn bare(&self) -> PathBuf {
        self._storage.path().join("alice/jeryu.git")
    }

    /// Commit `files` on a new branch cut from main, push it, and return its tip.
    fn branch_with(&self, name: &str, files: &[(&str, &str)], message: &str, date: &str) -> String {
        git(&self._work, &["checkout", "--quiet", "main"]);
        git(&self._work, &["checkout", "--quiet", "-b", name]);
        let tip = self.commit_here(files, message, date);
        git(&self._work, &["checkout", "--quiet", "main"]);
        git(
            &self._work,
            &["push", "--quiet", self.bare().to_str().unwrap(), name],
        );
        tip
    }

    /// Commit on main and push it.
    fn land_on_main(&self, files: &[(&str, &str)], message: &str) {
        git(&self._work, &["checkout", "--quiet", "main"]);
        self.commit_here(files, message, "2026-10-01T00:00:00Z");
        git(
            &self._work,
            &["push", "--quiet", self.bare().to_str().unwrap(), "main"],
        );
    }

    fn commit_here(&self, files: &[(&str, &str)], message: &str, date: &str) -> String {
        for (path, body) in files {
            std::fs::write(self._work.join(path), body).unwrap();
        }
        git(&self._work, &["add", "."]);
        let output = crate::test_git::git_command()
            .args(["commit", "--quiet", "-m", message])
            .current_dir(&self._work)
            .env("GIT_AUTHOR_NAME", "jeryu-test")
            .env("GIT_AUTHOR_EMAIL", "jeryu-test@example.com")
            .env("GIT_COMMITTER_NAME", "jeryu-test")
            .env("GIT_COMMITTER_EMAIL", "jeryu-test@example.com")
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        git(&self._work, &["rev-parse", "HEAD"])
    }

    fn pull(&self, branch: &str, tip: &str) -> u64 {
        let base = git(&self._work, &["rev-parse", "main"]);
        self.state
            .core
            .create_pull_request(
                "alice",
                "jeryu",
                "alice",
                CreatePullRequestRequest {
                    title: format!("pull for {branch}"),
                    head: branch.to_string(),
                    base: "main".to_string(),
                    head_sha: Some(tip.to_string()),
                    base_sha: Some(base),
                    ..CreatePullRequestRequest::default()
                },
            )
            .unwrap()
            .number
    }

    fn close(&self, number: u64) {
        self.state
            .core
            .update_pull_request(
                "alice",
                "jeryu",
                number,
                UpdatePullRequestRequest {
                    state: Some(PullRequestState::Closed),
                    ..UpdatePullRequestRequest::default()
                },
            )
            .unwrap();
    }

    fn stranded_names(&self) -> Vec<String> {
        let repo = self.state.core.get_repository("alice", "jeryu").unwrap();
        for_repository(&self.state, &repo)
            .expect("branches readable")
            .into_iter()
            .map(|row| row.branch)
            .collect()
    }
}

const OLD: &str = "2020-01-01T00:00:00Z";

#[test]
fn custody_refs_are_not_stranded_work() {
    let fixture = Fixture::new();
    for (n, name) in [
        "preserve/jeryu-v1",
        "preserve/hosted-cargo/v1.2.3",
        "backup/nightly",
        "mirror/upstream",
        "todoq-kept/20261020-aaaa",
        "todoq-rescue/20261020-bbbb",
    ]
    .into_iter()
    .enumerate()
    {
        fixture.branch_with(name, &[(&format!("c{n}.txt"), "kept\n")], "kept work", OLD);
    }
    for (n, subject) in [
        "GOV-005 custody commit for release 4",
        "custody: hold the old tree",
        "preserve(hosted-cargo): v1.2.3",
    ]
    .into_iter()
    .enumerate()
    {
        fixture.branch_with(
            &format!("held-{n}"),
            &[(&format!("h{n}.txt"), "held\n")],
            subject,
            OLD,
        );
    }
    fixture.branch_with("plain-work", &[("plain.txt", "p\n")], "plain work", OLD);

    let names: Vec<_> = all_at_round(&fixture.state, 0)
        .into_iter()
        .map(|row| row.branch)
        .collect();
    assert!(names.contains(&"plain-work".to_string()), "{names:?}");
    assert!(
        names.iter().all(|name| !name.starts_with("preserve/")
            && !name.starts_with("backup/")
            && !name.starts_with("mirror/")
            && !name.starts_with("todoq-")
            && !name.starts_with("held-")),
        "{names:?}"
    );
}

#[test]
fn custody_prefixes_come_from_the_site_setting() {
    let fixture = Fixture::new();
    fixture.branch_with("preserve/x", &[("p.txt", "p\n")], "work", OLD);
    fixture.branch_with("keep/y", &[("k.txt", "k\n")], "work", OLD);
    assert_eq!(custody_prefixes(&fixture.state)[0], "preserve/");
    fixture
        .state
        .site_settings
        .set(CUSTODY_PREFIXES_KEY, Some("keep/\n"), "admin", 1)
        .unwrap();
    assert_eq!(custody_prefixes(&fixture.state), ["keep/"]);
    let names = fixture.stranded_names();
    assert!(names.contains(&"preserve/x".to_string()), "{names:?}");
    assert!(!names.contains(&"keep/y".to_string()), "{names:?}");
}

#[test]
fn a_squash_merge_counts_as_landed_but_other_content_does_not() {
    let fixture = Fixture::new();
    fixture.branch_with(
        "squashed",
        &[("s1.txt", "one\n"), ("s2.txt", "two\n")],
        "squashed work",
        OLD,
    );
    // A second commit on the same branch leaves two patches, so patch identity
    // alone cannot match the single squash commit below.
    git(&fixture._work, &["checkout", "--quiet", "squashed"]);
    fixture.commit_here(&[("s2.txt", "two!\n")], "tweak", OLD);
    git(
        &fixture._work,
        &[
            "push",
            "--quiet",
            fixture.bare().to_str().unwrap(),
            "squashed",
        ],
    );
    fixture.land_on_main(&[("s1.txt", "one\n"), ("s2.txt", "two!\n")], "Squash (#9)");
    fixture.branch_with("diverged", &[("d.txt", "other\n")], "diverged work", OLD);
    fixture.land_on_main(&[("d.txt", "different\n")], "Other change");

    let names = fixture.stranded_names();
    assert!(!names.contains(&"squashed".to_string()), "{names:?}");
    assert!(names.contains(&"diverged".to_string()), "{names:?}");
}

#[test]
fn commits_whose_todo_trailer_is_on_main_are_landed() {
    let fixture = Fixture::new();
    fixture.branch_with(
        "todo-landed",
        &[("t1.txt", "a\n")],
        "work\n\nTodo: 20261001-abc.def",
        OLD,
    );
    fixture.branch_with(
        "todo-other-id",
        &[("t2.txt", "b\n")],
        "work\n\nTodo: 20261001-abc",
        OLD,
    );
    fixture.branch_with(
        "todo-absent",
        &[("t3.txt", "c\n")],
        "work\n\nTodo: nowhere",
        OLD,
    );
    fixture.land_on_main(
        &[("elsewhere.txt", "x\n")],
        "Landed differently\n\nTodo: 20261001-abc.def",
    );
    fixture.land_on_main(
        &[("elsewhere2.txt", "y\n")],
        "Longer id\n\nTodo: 20261001-abc-2",
    );

    let names = fixture.stranded_names();
    assert!(!names.contains(&"todo-landed".to_string()), "{names:?}");
    assert!(names.contains(&"todo-other-id".to_string()), "{names:?}");
    assert!(names.contains(&"todo-absent".to_string()), "{names:?}");
}

#[test]
fn a_closed_pull_whose_replacement_merged_is_landed() {
    let fixture = Fixture::new();
    let old_tip = fixture.branch_with("replaced", &[("r.txt", "r\n")], "first try", OLD);
    let unreplaced_tip = fixture.branch_with("unreplaced", &[("u.txt", "u\n")], "lone try", OLD);
    let replaced = fixture.pull("replaced", &old_tip);
    let unreplaced = fixture.pull("unreplaced", &unreplaced_tip);
    let new_tip = fixture.branch_with("replacement", &[("r2.txt", "r2\n")], "second try", OLD);
    let replacement = fixture.pull("replacement", &new_tip);
    fixture.close(replaced);
    fixture.close(unreplaced);
    // Both were pushed to after they closed, so only the replacement can
    // clear one of them.
    for (n, name) in ["replaced", "unreplaced"].into_iter().enumerate() {
        git(&fixture._work, &["checkout", "--quiet", name]);
        fixture.commit_here(
            &[(&format!("later{n}.txt"), "later\n")],
            "pushed later",
            "2099-01-01T00:00:00Z",
        );
        git(
            &fixture._work,
            &["push", "--quiet", fixture.bare().to_str().unwrap(), name],
        );
    }
    git(&fixture._work, &["checkout", "--quiet", "main"]);
    fixture
        .state
        .github
        .pull_replacements()
        .record("alice", "jeryu", replaced, replacement, "alice", 1)
        .unwrap();
    // Replacement still open: its branch is carried, the replaced one is not
    // yet landed.
    assert!(fixture.stranded_names().contains(&"replaced".to_string()));
    fixture
        .state
        .core
        .finalize_merge("alice", "jeryu", replacement, new_tip, None)
        .unwrap();
    let names = fixture.stranded_names();
    assert!(!names.contains(&"replaced".to_string()), "{names:?}");
    assert!(names.contains(&"unreplaced".to_string()), "{names:?}");
}

#[test]
fn a_closed_pull_branch_is_flagged_only_when_pushed_after_the_close() {
    let fixture = Fixture::new();
    let before = fixture.branch_with("closed-before", &[("b.txt", "b\n")], "old try", OLD);
    let after_base = fixture.branch_with("closed-after", &[("a.txt", "a\n")], "old try", OLD);
    let before_pr = fixture.pull("closed-before", &before);
    let after_pr = fixture.pull("closed-after", &after_base);
    fixture.close(before_pr);
    fixture.close(after_pr);
    git(&fixture._work, &["checkout", "--quiet", "closed-after"]);
    fixture.commit_here(
        &[("a2.txt", "a2\n")],
        "pushed later",
        "2099-01-01T00:00:00Z",
    );
    git(
        &fixture._work,
        &[
            "push",
            "--quiet",
            fixture.bare().to_str().unwrap(),
            "closed-after",
        ],
    );
    git(&fixture._work, &["checkout", "--quiet", "main"]);

    let names = fixture.stranded_names();
    assert!(!names.contains(&"closed-before".to_string()), "{names:?}");
    assert!(names.contains(&"closed-after".to_string()), "{names:?}");
}

fn account(login: &str) -> jeryu_core::AccountSummary {
    jeryu_core::AccountSummary {
        login: login.to_string(),
        display_name: login.to_string(),
        role: jeryu_core::UserRole::User,
        status: jeryu_core::AccountStatus::Active,
        auth_epoch: 0,
        must_change_password: false,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

async fn list_status(fixture: &Fixture, login: &str, id: &str) -> (u16, serde_json::Value) {
    use axum::body::to_bytes;
    use axum::extract::{Extension, Path, State};
    use axum::response::IntoResponse;
    let response = list(
        State(fixture.state.clone()),
        Extension(account(login)),
        Path(id.to_string()),
    )
    .await
    .into_response();
    let status = response.status().as_u16();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

#[tokio::test]
async fn the_branch_list_route_answers_readers_and_hides_private_repositories() {
    let fixture = Fixture::new();
    fixture.branch_with("listed", &[("l.txt", "l\n")], "listed work", OLD);
    fixture
        .state
        .core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "secret".to_string(),
                private: true,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();

    let (status, body) = list_status(&fixture, "mallory", "alice/jeryu").await;
    assert_eq!(status, 200, "{body}");
    let branches: Vec<_> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["branch"].as_str().unwrap().to_string())
        .collect();
    assert!(branches.contains(&"listed".to_string()), "{body}");
    assert_eq!(body["total"], branches.len());

    let (private, _) = list_status(&fixture, "mallory", "alice/secret").await;
    let (missing, _) = list_status(&fixture, "mallory", "alice/nothing").await;
    assert_eq!(
        private, 404,
        "a non-member learns nothing of a private repo"
    );
    assert_eq!(private, missing);
}

fn seeded_row(repo: &str, branch: &str) -> StrandedBranch {
    StrandedBranch {
        repo: repo.to_string(),
        branch: branch.to_string(),
        default_branch: "main".to_string(),
        commit_count: 1,
        newest_sha: "abc1234".to_string(),
        newest_subject: "kept work".to_string(),
        newest_at: None,
        closed_pr: None,
        scanned_at: Some(
            chrono::DateTime::parse_from_rfc3339("2026-10-13T17:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        ),
    }
}

#[test]
fn a_repository_outside_this_pass_keeps_its_rows_with_their_scan_time() {
    let fixture = Fixture::new();
    let state = &fixture.state;
    state
        .core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "unreached".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    // This repository has no storage, so the pass cannot reach it, as when a
    // pass runs out of budget before getting to a repository.
    let kept = seeded_row("alice/unreached", "kept/branch");
    state.stranded_scans.lock().unwrap().insert(
        "alice/unreached".to_string(),
        RepoScan {
            rows: BTreeMap::from([("kept/branch".to_string(), kept.clone())]),
        },
    );
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-13T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    for step in 0..3 {
        let rows = all_at_time(state, at + chrono::Duration::seconds(60 * step));
        assert!(rows.contains(&kept), "pass {step}: {rows:?}");
        assert!(rows.iter().any(|row| row.repo == "alice/jeryu"));
    }
}

#[test]
fn a_rescanned_repository_replaces_only_what_it_assessed() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-13T18:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut cache = ScanCache::new();
    cache.insert(
        "a/r".to_string(),
        RepoScan {
            rows: BTreeMap::from([
                ("one".to_string(), seeded_row("a/r", "one")),
                ("two".to_string(), seeded_row("a/r", "two")),
                ("gone".to_string(), seeded_row("a/r", "gone")),
            ]),
        },
    );
    merge_passes(
        &mut cache,
        vec![RepoPass {
            full_name: "a/r".to_string(),
            candidates: ["one", "two"].map(String::from).into(),
            assessed: vec![("one".to_string(), None)],
        }],
        now,
    );
    let rows = &cache["a/r"].rows;
    assert!(!rows.contains_key("one"), "landed branch clears");
    assert!(rows.contains_key("two"), "unassessed branch keeps its row");
    assert!(!rows.contains_key("gone"), "deleted branch clears");
}
