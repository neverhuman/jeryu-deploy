use std::collections::BTreeMap;
use std::path::Path;

use chrono::Utc;

use super::super::tests::run_git;
use super::super::todo_file::TodoFile;
use super::super::types::{ShiftTodo, TodoStatus};
use super::{Inner, RepoFacts, derive, scan_trailers};

const REPO: &str = "jeryu-deploy";

fn commit(dir: &Path, file: &str, message: &str) -> String {
    std::fs::write(dir.join(file), message).unwrap();
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-q", "-m", message]);
    run_git(dir, &["rev-parse", "HEAD"])
}

fn done_todo(id: &str, sha: &str) -> ShiftTodo {
    let mut file = TodoFile::new(id.to_string(), "jeryu".to_string(), "t".to_string());
    file.status = TodoStatus::Done;
    file.commits = vec![(REPO.to_string(), sha.to_string())];
    file.to_api(Utc::now())
}

fn facts(dir: &Path, base_head: &str, deployed: Option<&str>) -> BTreeMap<String, RepoFacts> {
    BTreeMap::from([(
        REPO.to_string(),
        RepoFacts {
            owner: "jeryu".to_string(),
            path: dir.to_path_buf(),
            base_head: Some(base_head.to_string()),
            deployed: deployed.map(str::to_string),
        },
    )])
}

#[test]
fn done_work_is_merged_by_ancestry_or_trailer_and_released_by_deployment() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    run_git(repo, &["init", "-q", "."]);
    let base = commit(repo, "base.txt", "base");
    run_git(repo, &["checkout", "-q", "-b", "nightshift/2026-09-20"]);
    let replayed = commit(repo, "a.txt", "a\n\nTodo: 20260920-000001-aaaaaa");
    let pending = commit(repo, "b.txt", "b\n\nTodo: 20260920-000002-bbbbbb");
    run_git(repo, &["checkout", "-q", "main"]);
    let on_main = commit(repo, "a.txt", "a again\n\nTodo: 20260920-000001-aaaaaa");
    let trailers = scan_trailers("git", repo, &on_main);
    assert_eq!(trailers["20260920-000001-aaaaaa"], on_main);
    assert!(!trailers.contains_key("20260920-000002-bbbbbb"));

    let mut inner = Inner::default();
    let facts_now = facts(repo, &on_main, Some(&base));

    // An ancestor of the base branch is merged under its own sha.
    let direct = derive(&mut inner, "git", &facts_now, &done_todo("direct", &base));
    assert!(direct.merged);
    assert_eq!(direct.landed[REPO], base);
    assert_eq!(direct.released, Some(true));

    // A rebased commit is merged through its `Todo:` trailer, not yet released.
    let rebased = derive(
        &mut inner,
        "git",
        &facts_now,
        &done_todo("20260920-000001-aaaaaa", &replayed),
    );
    assert!(rebased.merged);
    assert_eq!(rebased.landed[REPO], on_main);
    assert_eq!(rebased.released, Some(false));

    // Work whose PR has not merged is neither merged nor released.
    let open = derive(
        &mut inner,
        "git",
        &facts_now,
        &done_todo("20260920-000002-bbbbbb", &pending),
    );
    assert!(!open.merged);
    assert!(open.landed.is_empty());
    assert_eq!(open.released, Some(false));

    // No production deployment: release is unknown, not false.
    let unknown = derive(
        &mut Inner::default(),
        "git",
        &facts(repo, &on_main, None),
        &done_todo("direct", &base),
    );
    assert!(unknown.merged);
    assert_eq!(unknown.released, None);

    // Deploying main releases the rebased todo on the next look.
    let deployed = facts(repo, &on_main, Some(&on_main));
    let released = derive(
        &mut inner,
        "git",
        &deployed,
        &done_todo("20260920-000001-aaaaaa", &replayed),
    );
    assert_eq!(released.released, Some(true));

    // Merged and released stay that way even when the facts regress.
    let regressed = derive(
        &mut inner,
        "git",
        &facts(repo, &base, None),
        &done_todo("20260920-000001-aaaaaa", &replayed),
    );
    assert!(regressed.merged);
    assert_eq!(regressed.released, Some(true));
}
