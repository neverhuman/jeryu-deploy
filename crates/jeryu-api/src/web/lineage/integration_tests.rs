use super::*;
use jeryu_core::{CreateRepositoryRequest, ForgeCore};
use jeryu_gitd::{GitdConfig, RepoId, RepoManager};
#[tokio::test]
async fn reads_real_forks_merges_and_only_recent_uncontained_branches() {
    let storage = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let manager = RepoManager::new(GitdConfig::new(storage.path().to_path_buf()));
    let bare = manager
        .create_bare(&RepoId::new("acme", "app").unwrap())
        .unwrap();
    let run = |args: &[&str]| {
        let output = crate::test_git::git_command()
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(work.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    run(&["init", "-q"]);
    run(&["commit", "--allow-empty", "-qm", "base"]);
    let root = run(&["rev-parse", "HEAD"]);
    run(&["checkout", "-qb", "feature"]);
    run(&["commit", "--allow-empty", "-qm", "feature change"]);
    let feature = run(&["rev-parse", "HEAD"]);
    run(&["checkout", "main"]);
    run(&["commit", "--allow-empty", "-qm", "main change"]);
    let main_before_merge = run(&["rev-parse", "HEAD"]);
    run(&["merge", "--no-ff", "feature", "-m", "merge feature"]);
    let merge = run(&["rev-parse", "HEAD"]);
    run(&["checkout", "-qb", "active", &root]);
    run(&["commit", "--allow-empty", "-qm", "active change"]);
    run(&[
        "push",
        "-q",
        &bare.path.to_string_lossy(),
        "main",
        "feature",
        "active",
    ]);
    let core = ForgeCore::new();
    core.create_repository(
        "acme",
        CreateRepositoryRequest {
            name: "app".into(),
            private: false,
            description: None,
            default_branch: Some("main".into()),
        },
    )
    .unwrap();
    let state = WebState::with_repo_manager(
        core,
        Arc::new(manager),
        work.path().join("no-spa"),
        data.path().into(),
        super::super::catalog::SplitCatalog::builtin(),
    );
    let graph = read(&state, "acme/app").await.unwrap();
    assert_eq!(graph.default_sha, merge);
    assert_eq!(
        graph
            .branches
            .iter()
            .map(|b| b.name.as_str())
            .collect::<Vec<_>>(),
        ["active"]
    );
    let merged = graph.commits.iter().find(|c| c.sha == merge).unwrap();
    assert_eq!(merged.parents, [main_before_merge, feature]);
    assert!(
        graph
            .commits
            .iter()
            .any(|c| c.sha == root && c.parents.is_empty())
    );
    assert!(graph.missing_parents.is_empty());
    assert!(!graph.shallow);
}
