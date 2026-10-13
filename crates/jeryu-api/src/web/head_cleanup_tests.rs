//! `delete_branch_on_merge` over real bare repositories: a merged pull
//! request's head branch is deleted, unless something still needs it.

use super::merge_queue_tests::{Fixture, git};
use super::*;
use crate::github::Method;
use jeryu_core::{CommitStatusState, CreatePullRequestRequest, SetBranchProtectionRequest};

const FEATURE: &str = "refs/heads/feature";
const PINNED: &str = "refs/pull/1/head";

/// The commit `refs/pull/1/head` keeps once the branch is gone.
fn pinned(fx: &Fixture) -> String {
    git(&fx.bare, &["rev-parse", PINNED])
}

fn merge(fx: &Fixture) -> Value {
    let response = fx.state.github.handle(
        Method::Put,
        &format!("/repos/alice/jeryu/pulls/{}/merge", fx.number),
        "{}",
    );
    assert_eq!(response.status, 200, "{}", response.body);
    serde_json::from_str(&response.body).unwrap()
}

fn patch_repo(fx: &Fixture, body: &str) -> (u16, Value) {
    let response = fx
        .state
        .github
        .handle(Method::Patch, "/repos/alice/jeryu", body);
    (
        response.status,
        serde_json::from_str(&response.body).unwrap_or(Value::Null),
    )
}

/// A second open pull request in the same repository, from `head` into `base`.
fn open_pull(fx: &Fixture, head: &str, base: &str) -> u64 {
    fx.state
        .core
        .create_pull_request(
            "alice",
            "jeryu",
            "alice",
            CreatePullRequestRequest {
                title: "stacked".to_string(),
                head: head.to_string(),
                base: base.to_string(),
                head_sha: Some(fx.head.clone()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap()
        .number
}

#[test]
fn a_direct_merge_deletes_the_head_branch_and_says_so() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let body = merge(&fx);
    assert_eq!(body["merged"], true, "{body}");
    assert_eq!(
        body["head_branch"],
        json!({ "name": "feature", "deleted": true })
    );
    assert!(!fx.has_ref(FEATURE));
    assert_eq!(pinned(&fx), fx.head, "the merged head stays reachable");
    assert_eq!(fx.main(), fx.head, "the merge itself still landed");
}

#[test]
fn a_deleted_head_has_a_durable_audit_receipt() {
    let fx = Fixture::with_audit_store("feature.txt", "feature\n");
    merge(&fx);
    let rows = fx.state.core.list_audit("alice/jeryu").unwrap();
    let rows: Vec<_> = rows
        .iter()
        .filter(|row| row.action == "pull_request.delete_head_branch")
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].phase, "requested");
    assert_eq!(rows[1].phase, "completed");
    assert_eq!(rows[1].actor, "system:pr-merge");
    assert_eq!(rows[1].detail["old_sha"], fx.head);
    assert_eq!(rows[1].detail["ref"], FEATURE);
    assert_eq!(rows[1].detail["pull_request"], fx.number);
    assert_eq!(rows[1].detail["request_audit_id"], rows[0].id);
}

#[test]
fn an_archived_repository_rejects_merge_setting_writes() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let (status, _) = patch_repo(&fx, r#"{"archived": true, "actor": "alice"}"#);
    assert_eq!(status, 200);
    let (status, body) = patch_repo(&fx, r#"{"delete_branch_on_merge": false}"#);
    assert_eq!(status, 422, "{body}");
    assert!(body.to_string().contains("read-only"));
}

#[test]
fn an_unknown_or_fork_source_keeps_the_destination_branch() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let resolved = fx
        .state
        .repo_manager
        .resolve_parts("alice", "jeryu")
        .unwrap();
    for source in ["", "invalid", "bob/jeryu"] {
        let mut pr = fx
            .state
            .core
            .get_pull_request("alice", "jeryu", fx.number)
            .unwrap();
        pr.source_repository = source.to_string();
        let result =
            fx.state
                .github
                .cleanup_pull_head_for_test(&fx.state.repo_manager, &resolved, &pr);
        assert_eq!(result["deleted"], false);
        assert!(fx.has_ref(FEATURE));
    }
}

#[tokio::test]
async fn a_merge_queue_landing_deletes_the_head_branch() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(merge_queue::tick(&fx.state), 1);
    assert_eq!(fx.entry().state, merge_queue::QueueState::Landed);
    assert!(!fx.has_ref(FEATURE));
}

/// A landing onto a moved base lands a REPLAY, so the gated head is not in
/// main's history. Deleting the branch must not leave that commit (the one the
/// checks ran on, and the pull request's diff) unreachable.
#[tokio::test]
async fn a_replayed_landing_keeps_the_gated_head_reachable() {
    let fx = Fixture::new("feature.txt", "feature\n");
    fx.advance_main("other.txt", "other\n");
    let (status, body) = fx.enqueue().await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let queue_sha = body["queue_sha"].as_str().unwrap().to_string();
    assert_ne!(queue_sha, fx.head, "the queue built a replay");
    fx.status(&queue_sha, CommitStatusState::Success);
    assert_eq!(merge_queue::tick(&fx.state), 1);
    assert_eq!(fx.entry().state, merge_queue::QueueState::Landed);

    assert!(!fx.has_ref(FEATURE));
    assert_eq!(pinned(&fx), fx.head);
    let ancestor = crate::test_git::git_command()
        .args(["merge-base", "--is-ancestor", &fx.head, "refs/heads/main"])
        .current_dir(&fx.bare)
        .status()
        .unwrap();
    assert!(
        !ancestor.success(),
        "the gated head is reachable only through the pin"
    );
}

#[test]
fn a_repository_with_the_setting_off_keeps_its_branches() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let (status, repo) = patch_repo(&fx, r#"{"delete_branch_on_merge": false, "actor": "ada"}"#);
    assert_eq!(status, 200, "{repo}");
    assert_eq!(repo["delete_branch_on_merge"], false);

    let body = merge(&fx);
    assert_eq!(body["head_branch"]["deleted"], false, "{body}");
    assert_eq!(
        body["head_branch"]["reason"],
        "delete_branch_on_merge is off for this repository"
    );
    assert!(fx.has_ref(FEATURE));
}

/// A branch some pull request merges into is a long-lived line (a stack's
/// base, `release/5.0`, `develop`): a back-merge out of it keeps it.
#[test]
fn a_branch_a_pull_request_merges_into_is_kept() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let stacked = open_pull(&fx, "feature-2", "feature");
    let body = merge(&fx);
    assert_eq!(body["head_branch"]["deleted"], false, "{body}");
    assert_eq!(
        body["head_branch"]["reason"],
        format!("pull request #{stacked} merges into it, so it is a long-lived branch")
    );
    assert!(fx.has_ref(FEATURE));
}

#[test]
fn a_branch_another_open_pull_request_merges_from_is_kept() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let other = open_pull(&fx, "feature", "release");
    let body = merge(&fx);
    assert_eq!(
        body["head_branch"]["reason"],
        format!("open pull request #{other} uses it"),
        "{body}"
    );
    assert!(fx.has_ref(FEATURE));
}

#[test]
fn a_protected_head_branch_is_kept() {
    let fx = Fixture::new("feature.txt", "feature\n");
    fx.state
        .core
        .set_branch_protection(
            "alice",
            "jeryu",
            "feature",
            SetBranchProtectionRequest::default(),
        )
        .unwrap();
    let body = merge(&fx);
    assert_eq!(
        body["head_branch"]["reason"], "it has branch protection",
        "{body}"
    );
    assert!(fx.has_ref(FEATURE));
}

#[test]
fn a_server_pattern_rule_keeps_the_head_branch() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let mut config = fx.state.repo_manager.config().clone();
    config
        .protected_refs
        .push(jeryu_gitd::protection::ProtectedRefRule {
            pattern: "refs/heads/feat*".to_string(),
            deny_delete: true,
            deny_force: true,
            bypass_actors: Vec::new(),
        });
    let manager = Arc::new(jeryu_gitd::RepoManager::new(config));
    let resolved = manager.resolve_parts("alice", "jeryu").unwrap();
    let pr = fx
        .state
        .core
        .get_pull_request("alice", "jeryu", fx.number)
        .unwrap();
    let result = fx
        .state
        .github
        .cleanup_pull_head_for_test(&manager, &resolved, &pr);
    assert_eq!(result["deleted"], false);
    assert!(result["reason"].as_str().unwrap().contains("cannot delete"));
    assert!(fx.has_ref(FEATURE));
    assert_eq!(pinned(&fx), fx.head);
}

#[test]
fn the_setting_reads_and_writes_the_way_github_spells_it() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let repo = fx.state.github.get("/repos/alice/jeryu");
    let repo: Value = serde_json::from_str(&repo.body).unwrap();
    assert_eq!(repo["delete_branch_on_merge"], true, "on unless turned off");

    let (status, body) = patch_repo(&fx, r#"{"delete_branch_on_merge": "no"}"#);
    assert_eq!(status, 422, "{body}");
    let (status, body) = patch_repo(
        &fx,
        r#"{"delete_branch_on_merge": false, "archived": true}"#,
    );
    assert_eq!(status, 422, "one decision per request: {body}");
    let (status, body) = patch_repo(&fx, r#"{"archived": true, "name": "renamed"}"#);
    assert_eq!(status, 422, "{body}");
    assert!(
        body.to_string()
            .contains("archive and rename are separate requests"),
        "the archive+rename case keeps its own message: {body}"
    );

    let (status, body) = patch_repo(&fx, r#"{"delete_branch_on_merge": false}"#);
    assert_eq!(status, 200, "{body}");
    let (status, body) = patch_repo(&fx, r#"{"delete_branch_on_merge": true}"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["delete_branch_on_merge"], true);
}
