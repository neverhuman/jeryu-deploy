//! `delete_branch_on_merge` over real bare repositories: a merged pull
//! request's head branch is deleted, unless something still needs it.

use super::merge_queue_tests::Fixture;
use super::*;
use crate::github::Method;
use jeryu_core::{CreatePullRequestRequest, SetBranchProtectionRequest};

const FEATURE: &str = "refs/heads/feature";

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
    assert_eq!(fx.main(), fx.head, "the merge itself still landed");
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

#[test]
fn a_branch_another_open_pull_request_stacks_on_is_kept() {
    let fx = Fixture::new("feature.txt", "feature\n");
    let stacked = open_pull(&fx, "feature-2", "feature");
    let body = merge(&fx);
    assert_eq!(body["head_branch"]["deleted"], false, "{body}");
    assert_eq!(
        body["head_branch"]["reason"],
        format!("open pull request alice/jeryu#{stacked} uses it")
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

    let (status, body) = patch_repo(&fx, r#"{"delete_branch_on_merge": false}"#);
    assert_eq!(status, 200, "{body}");
    let (status, body) = patch_repo(&fx, r#"{"delete_branch_on_merge": true}"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["delete_branch_on_merge"], true);
}
