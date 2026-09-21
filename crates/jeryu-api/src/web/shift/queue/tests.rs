use super::super::tests::run_git;
use super::super::todo_file::TodoFile;
use super::super::types::TodoStatus;
use super::{build_commit, parse_family_toml, read_todos, resolve};

#[test]
fn family_toml_defaults_fill_gaps_and_repos_sort_by_order_then_name() {
    let config = parse_family_toml(
        "[[repo]]\nname = \"b\"\norder = 1\n[[repo]]\nname = \"a\"\norder = 1\n[[repo]]\nname = \"z\"\n",
        "fallback",
    )
    .expect("parse");
    assert_eq!(config.name, "fallback");
    assert_eq!(config.base_branch, "main");
    assert_eq!(config.landing, "batch");
    assert_eq!(config.shift_tz, "America/Los_Angeles");
    assert_eq!(config.bulletshift_prefix, "bulletshift");
    assert_eq!(config.nightshift_prefix, "nightshift");
    let names: Vec<&str> = config.repos.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["z", "a", "b"]);
    assert!(parse_family_toml("[family\n", "x").is_err());
}

fn todo(id: &str, status: TodoStatus) -> TodoFile {
    let mut todo = TodoFile::new(id.to_string(), "jeryu".to_string(), format!("Todo {id}"));
    todo.status = status;
    todo
}

/// A bare repo whose `queue` branch holds one todo per status, a file with an
/// unknown status, and a non-markdown file.
fn queue_repo(root: &std::path::Path) -> (std::path::PathBuf, String) {
    let work = root.join("work");
    std::fs::create_dir_all(work.join("todos")).unwrap();
    run_git(&work, &["init", "-q", "-b", "queue"]);
    for (index, status) in TodoStatus::ALL.into_iter().enumerate() {
        let todo = todo(&format!("20260921-00000{index}-aaaaaa"), status);
        std::fs::write(work.join("todos").join(todo.filename()), todo.dump()).unwrap();
    }
    std::fs::write(
        work.join("todos/bad.md"),
        "+++\nid = \"bad\"\nstatus = \"merged\"\n+++\n",
    )
    .unwrap();
    std::fs::write(work.join("todos/README.txt"), "not a todo").unwrap();
    run_git(&work, &["add", "."]);
    run_git(&work, &["commit", "-q", "-m", "seed"]);
    let bare = root.join("jeryu-todo.git");
    run_git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let head = resolve("git", &bare, "refs/heads/queue").expect("queue head");
    (bare, head)
}

#[test]
fn read_todos_keeps_every_status_and_skips_files_that_are_not_todos() {
    let dir = tempfile::tempdir().unwrap();
    let (bare, head) = queue_repo(dir.path());
    let todos = read_todos("git", &bare, &head).expect("read");
    let statuses: Vec<TodoStatus> = todos.iter().map(|q| q.todo.status).collect();
    assert_eq!(statuses, TodoStatus::ALL);
    assert!(todos.iter().all(|q| q.path.starts_with("todos/")));
    assert!(resolve("git", &bare, "refs/heads/missing").is_none());
}

#[test]
fn build_commit_writes_and_deletes_on_top_of_the_parent_only() {
    let dir = tempfile::tempdir().unwrap();
    let (bare, head) = queue_repo(dir.path());
    let before = read_todos("git", &bare, &head).unwrap();
    let mut done = before[0].todo.clone();
    done.status = TodoStatus::Done;
    let changes = vec![
        (before[0].path.clone(), Some(done.dump())),
        (before[1].path.clone(), None),
    ];
    let commit =
        build_commit("git", &bare, &head, "alton", "move todos", &changes).expect("build commit");
    // The commit exists but no ref moved: moving it is the caller's CAS.
    assert_eq!(
        resolve("git", &bare, "refs/heads/queue").as_deref(),
        Some(head.as_str())
    );
    assert_eq!(run_git(&bare, &["rev-parse", &format!("{commit}^")]), head);
    assert_eq!(
        run_git(&bare, &["log", "-1", "--format=%an <%ae>|%s", &commit]),
        "alton <alton@jeryu>|move todos"
    );
    let after = read_todos("git", &bare, &commit).unwrap();
    assert_eq!(after.len(), before.len() - 1);
    assert_eq!(after[0].todo.status, TodoStatus::Done);
    assert!(after.iter().all(|q| q.path != before[1].path));
}
