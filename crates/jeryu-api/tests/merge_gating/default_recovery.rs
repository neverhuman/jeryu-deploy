//! Review and gate checks for restoring an absent default branch.
use super::*;

#[test]
fn a_deleted_default_branch_can_be_recreated_through_review() {
    if !git_available() {
        return;
    }
    let fixture = seed_fixture("jeryu-recreate-default");
    let id = RepoId::new("acme", "demo").unwrap();
    let resolved = fixture.manager.resolve(&id).unwrap();
    run_git(
        &resolved.path,
        &["update-ref", "-d", "refs/heads/main"],
        "delete default before create",
    );
    let router = router_over(&fixture);
    let number = open_pr_by_branch(&router, "feature", "main");
    approve(&router, number);
    let merged = router.put(&format!("/repos/acme/demo/pulls/{number}/merge"), "{}");
    assert_eq!(merged.status, 200, "{}", merged.body);
    assert_eq!(fixture.main_ref(), fixture.head_oid);
    fixture.cleanup();
}
