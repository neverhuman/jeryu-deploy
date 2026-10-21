//! Line survival on real git repositories. Every name is invented (families
//! acme and globex, host node-a).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

use super::*;
use crate::web::pipeline::NewEvent;

const T0: i64 = 1_760_000_000;
const DAY: i64 = 86_400;

struct Repo {
    _dir: tempfile::TempDir,
    work: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("widget");
        std::fs::create_dir_all(&work).unwrap();
        let repo = Self { _dir: dir, work };
        repo.git(0, &["init", "-q", "-b", "main"]);
        repo
    }

    fn git(&self, at: i64, args: &[&str]) -> String {
        let date = format!("@{at} +0000");
        let out = Command::new("git")
            .current_dir(&self.work)
            .args(["-c", "user.name=Ann", "-c", "user.email=ann@example.test"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(&self, name: &str, body: &str) {
        let path = self.work.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// Commit everything at `at`, returning the commit id.
    fn commit(&self, at: i64, message: &str) -> String {
        self.git(at, &["add", "-A"]);
        self.git(at, &["commit", "-q", "-m", message]);
        self.git(at, &["rev-parse", "HEAD"])
    }

    fn git_dir(&self) -> PathBuf {
        self.work.join(".git")
    }

    fn measure(&self, sha: &str, merged: i64, now: i64) -> Measured {
        let leg = due(sha, merged * 1000, now * 1000);
        measure("git", &self.git_dir(), "main", &leg).unwrap()
    }
}

fn due(sha: &str, merged_ms: i64, now_ms: i64) -> DueLeg {
    DueLeg {
        lot_id: "20261001-000001-aaaaaa".into(),
        repo: "acme/widget".into(),
        sha: sha.into(),
        merged_ms,
        needs_14d: now_ms - merged_ms >= 14 * DAY_MS,
        needs_30d: now_ms - merged_ms >= 30 * DAY_MS,
    }
}

fn lines(prefix: &str, n: usize) -> String {
    (0..n).map(|i| format!("{prefix} {i}\n")).collect()
}

#[test]
fn a_rewrite_of_the_same_lines_stops_them_surviving() {
    let repo = Repo::new();
    repo.write("a.txt", &lines("keep", 6));
    let first = repo.commit(T0, "add");
    // 4 of the 6 lines are rewritten 5 days later, 2 more 20 days later.
    repo.write(
        "a.txt",
        &format!("{}{}", lines("new", 4), "keep 4\nkeep 5\n"),
    );
    repo.commit(T0 + 5 * DAY, "rewrite four");
    repo.write(
        "a.txt",
        &format!("{}{}", lines("new", 4), "again\nagain too\n"),
    );
    repo.commit(T0 + 20 * DAY, "rewrite two");
    let m = repo.measure(&first, T0, T0 + 40 * DAY);
    assert_eq!((m.added, m.deleted), (Some(6), Some(0)));
    assert_eq!(m.surviving_14d, Some(2));
    assert_eq!(m.surviving_30d, Some(0));
}

#[test]
fn a_file_rename_keeps_the_lines_surviving() {
    let repo = Repo::new();
    repo.write("src/a.txt", &lines("keep", 8));
    let first = repo.commit(T0, "add");
    repo.git(T0 + DAY, &["mv", "src/a.txt", "src/renamed.txt"]);
    repo.commit(T0 + DAY, "rename");
    let m = repo.measure(&first, T0, T0 + 31 * DAY);
    assert_eq!(m.surviving_14d, Some(8));
    assert_eq!(m.surviving_30d, Some(8));
}

#[test]
fn a_revert_leaves_nothing_surviving() {
    let repo = Repo::new();
    repo.write("a.txt", &lines("base", 3));
    repo.commit(T0 - DAY, "base");
    repo.write("b.txt", &lines("fresh", 10));
    let first = repo.commit(T0, "add b");
    repo.git(T0 + 2 * DAY, &["revert", "--no-edit", &first]);
    let m = repo.measure(&first, T0, T0 + 31 * DAY);
    assert_eq!(m.added, Some(10));
    assert_eq!(m.surviving_14d, Some(0));
    assert_eq!(m.surviving_30d, Some(0));
}

#[test]
fn generated_vendored_and_lock_files_are_not_counted() {
    let repo = Repo::new();
    repo.write("base.txt", "x\n");
    repo.commit(T0 - DAY, "base");
    repo.write(
        ".gitattributes",
        "gen/** linguist-generated\nthird/** linguist-vendored\nkeepme.lock -linguist-generated\n",
    );
    repo.write("real.txt", &lines("real", 5));
    repo.write("Cargo.lock", &lines("lock", 100));
    repo.write("gen/out.txt", &lines("gen", 40));
    repo.write("third/lib.txt", &lines("lib", 30));
    repo.write("web/node_modules/x/index.txt", &lines("dep", 20));
    let first = repo.commit(T0, "add");
    let m = repo.measure(&first, T0, T0 + 20 * DAY);
    // .gitattributes itself counts as one added line per rule: 3.
    assert_eq!(m.added, Some(5 + 3));
    assert_eq!(m.surviving_14d, Some(8));
    assert_eq!(m.surviving_30d, None);
}

#[test]
fn default_names_follow_the_list() {
    assert!(default_skipped("Cargo.lock"));
    assert!(default_skipped("a/b/yarn.lock"));
    assert!(default_skipped("static/app.min.js"));
    assert!(default_skipped("vendor/x/y.go"));
    assert!(!default_skipped("src/vendor.rs"));
    assert!(!default_skipped("src/lock.rs"));
}

#[test]
fn a_merge_commit_counts_the_lines_of_the_commits_it_brought_in() {
    let repo = Repo::new();
    repo.write("base.txt", "x\n");
    repo.commit(T0 - DAY, "base");
    repo.git(T0, &["checkout", "-q", "-b", "topic"]);
    repo.write("t.txt", &lines("topic", 7));
    repo.commit(T0, "topic work");
    repo.git(T0 + 60, &["checkout", "-q", "main"]);
    repo.git(
        T0 + 60,
        &["merge", "--no-ff", "-q", "-m", "merge topic", "topic"],
    );
    let merge = repo.git(T0 + 60, &["rev-parse", "HEAD"]);
    repo.write("later.txt", "later\n");
    repo.commit(T0 + 3 * DAY, "later");
    let m = repo.measure(&merge, T0, T0 + 20 * DAY);
    assert_eq!(m.added, Some(7));
    assert_eq!(m.surviving_14d, Some(7));
}

#[test]
fn a_late_measurement_counts_the_tip_of_its_own_window() {
    let repo = Repo::new();
    repo.write("a.txt", &lines("keep", 4));
    let first = repo.commit(T0, "add");
    repo.write("a.txt", "gone\n");
    repo.commit(T0 + 20 * DAY, "rewrite after the 14 day mark");
    // Measured 90 days late, the 14 day count still sees main as it was.
    let m = repo.measure(&first, T0, T0 + 90 * DAY);
    assert_eq!(m.surviving_14d, Some(4));
    assert_eq!(m.surviving_30d, Some(0));
}

#[test]
fn a_commit_that_is_not_on_main_is_an_error_not_a_zero() {
    let repo = Repo::new();
    repo.write("a.txt", "x\n");
    repo.commit(T0, "base");
    repo.git(T0, &["checkout", "-q", "-b", "side"]);
    repo.write("s.txt", "s\n");
    let side = repo.commit(T0 + DAY, "side only");
    repo.git(T0, &["checkout", "-q", "main"]);
    let leg = due(&side, T0 * 1000, (T0 + 20 * DAY) * 1000);
    assert!(measure("git", &repo.git_dir(), "main", &leg).is_err());
    let bad = DueLeg {
        sha: "--output=x".into(),
        ..leg
    };
    assert!(measure("git", &repo.git_dir(), "main", &bad).is_err());
}

#[test]
fn parsers_read_renames_deletes_and_binary_files() {
    let numstat = b"3\t1\tsrc/a.rs\0-\t-\timg.png\x000\t0\t\0old.rs\0new.rs\0";
    let files = parse_numstat(numstat);
    assert_eq!(files.len(), 2);
    assert_eq!(files[1].path, "new.rs");
    let moved = parse_name_status(b"R090\0old.rs\0new.rs\0D\0gone.rs\0M\0kept.rs\0");
    assert_eq!(moved["old.rs"].as_deref(), Some("new.rs"));
    assert_eq!(moved["gone.rs"], None);
    assert!(!moved.contains_key("kept.rs"));
    let sha = "a".repeat(40);
    let blame = format!(
        "{sha} 1 1 2\nauthor x\n\tone\n{sha} 2 2\n\ttwo\n{} 3 3 1\n\tthree\n",
        "b".repeat(40)
    );
    let authors: BTreeSet<String> = BTreeSet::from([sha]);
    assert_eq!(blamed_lines(blame.as_bytes(), &authors), 2);
}

fn merged_event(todo: &str, repo: &str, pr: i64, sha: &str) -> NewEvent {
    NewEvent {
        source: "todoq".into(),
        kind: "pr.merged".into(),
        summary: "merged".into(),
        family: Some("acme".into()),
        todo_id: Some(todo.into()),
        repo: Some(repo.into()),
        pr: Some(pr),
        sha: Some(sha.into()),
        ..NewEvent::default()
    }
}

fn store() -> (tempfile::TempDir, EventStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = EventStore::open(&dir.path().join("shift.sqlite")).unwrap();
    (dir, store)
}

fn leg_of(store: &EventStore, lot: &str) -> crate::web::outcomes::project::Leg {
    store
        .outcome_changes(None, None)
        .unwrap()
        .into_iter()
        .find(|c| c.lot_id == lot)
        .unwrap()
        .legs
        .remove(0)
}

#[test]
fn a_pass_stores_each_window_once_it_is_due_and_catches_up() {
    let repo = Repo::new();
    repo.write("a.txt", &lines("keep", 6));
    let sha = repo.commit(T0, "add");
    repo.write(
        "a.txt",
        &format!(
            "{}{}",
            lines("new", 2),
            lines("keep", 6)
                .lines()
                .skip(2)
                .map(|l| format!("{l}\n"))
                .collect::<String>()
        ),
    );
    repo.commit(T0 + 20 * DAY, "rewrite two");
    let (_dir, store) = store();
    let lot = "20261001-000001-aaaaaa";
    store
        .insert(
            "node-a",
            &merged_event(lot, "acme/widget", 7, &sha),
            T0 * 1000,
        )
        .unwrap();
    let git_dir = repo.git_dir();
    let resolve =
        |name: &str| (name == "acme/widget").then(|| (git_dir.clone(), "main".to_string()));
    let mut attempts = Attempts::new();

    // Not yet 14 days: nothing is due and nothing is stored as zero.
    assert_eq!(
        run_pass(
            &store,
            "git",
            &resolve,
            (T0 + 10 * DAY) * 1000,
            &mut attempts
        ),
        0
    );
    assert_eq!(leg_of(&store, lot).lines, Default::default());

    // At 16 days only the 14 day mark is stored; 30 days stays pending.
    assert_eq!(
        run_pass(
            &store,
            "git",
            &resolve,
            (T0 + 16 * DAY) * 1000,
            &mut attempts
        ),
        1
    );
    let lines = leg_of(&store, lot).lines;
    assert_eq!(
        (lines.added, lines.deleted, lines.surviving_14d),
        (Some(6), Some(0), Some(6))
    );
    assert_eq!(lines.surviving_30d, None);

    // After downtime the 30 day mark is caught up, and nothing is re-measured.
    attempts.clear();
    assert_eq!(
        run_pass(
            &store,
            "git",
            &resolve,
            (T0 + 80 * DAY) * 1000,
            &mut attempts
        ),
        1
    );
    assert_eq!(leg_of(&store, lot).lines.surviving_30d, Some(4));
    attempts.clear();
    assert_eq!(
        run_pass(
            &store,
            "git",
            &resolve,
            (T0 + 81 * DAY) * 1000,
            &mut attempts
        ),
        0
    );
}

#[test]
fn an_unresolvable_repository_leaves_the_leg_due_and_is_not_retried_at_once() {
    let (_dir, store) = store();
    let lot = "20261001-000002-bbbbbb";
    store
        .insert(
            "node-a",
            &merged_event(lot, "globex/gadget", 3, "abc1234def"),
            T0 * 1000,
        )
        .unwrap();
    let resolve = |_: &str| None::<(PathBuf, String)>;
    let mut attempts = Attempts::new();
    let now = (T0 + 40 * DAY) * 1000;
    assert_eq!(run_pass(&store, "git", &resolve, now, &mut attempts), 0);
    assert_eq!(attempts.len(), 1);
    assert_eq!(store.line_survival_due(now, 10).unwrap().len(), 1);
    assert_eq!(leg_of(&store, lot).lines, Default::default());
}
