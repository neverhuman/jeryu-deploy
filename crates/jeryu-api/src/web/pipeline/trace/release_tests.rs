//! Deployment timestamps come from per-PR production evidence.

use super::*;
use jeryu_core::ForgeCore;
use serde_json::json;

#[test]
fn deployment_history_dates_success_not_start_or_a_later_deploy() {
    use super::super::types::NewEvent;
    let root = tempfile::tempdir().unwrap();
    let state = WebState::new_with_git_storage(ForgeCore::new(), root.path().to_path_buf());
    let now = Utc::now().timestamp_millis();
    let post = |repo: &str, pr, kind: &str, outcome: Option<&str>, offset| {
        state
            .events
            .insert(
                "operator",
                &NewEvent {
                    source: "fixture".into(),
                    kind: kind.into(),
                    summary: kind.into(),
                    repo: Some(repo.into()),
                    pr: Some(pr),
                    outcome: outcome.map(str::to_string),
                    detail: Some(json!({"environment": "production"})),
                    ..Default::default()
                },
                now + offset,
            )
            .unwrap();
    };
    let repo = "acme/widgets-web";
    for kind in ["deploy.created", "deploy.status"] {
        state
            .events
            .insert(
                "operator",
                &NewEvent {
                    source: "fixture".into(),
                    kind: kind.into(),
                    summary: kind.into(),
                    repo: Some(repo.into()),
                    pr: Some(8),
                    outcome: Some("success".into()),
                    detail: Some(json!({"environment": "staging"})),
                    ..Default::default()
                },
                now,
            )
            .unwrap();
    }
    assert_eq!(state.events.first_pr_deployment(repo, 8).unwrap(), None);
    post(repo, 7, "pr.merged", None, 0);
    post(repo, 7, "release.staged", None, 1000);
    post(repo, 7, "deploy.created", None, 2000);
    assert_eq!(state.events.first_pr_deployment(repo, 7).unwrap(), None);
    post(repo, 7, "deploy.status", Some("failure"), 3000);
    assert_eq!(state.events.first_pr_deployment(repo, 7).unwrap(), None);
    post(repo, 7, "deploy.created", None, 4000);
    post(repo, 7, "deploy.status", Some("success"), 5000);
    let expected = Some(crate::web::shift::rfc3339_ms(now + 5000));
    assert_eq!(state.events.first_pr_deployment(repo, 7).unwrap(), expected);
    post(repo, 7, "deploy.created", None, 6000);
    post(repo, 7, "deploy.status", Some("success"), 7000);
    assert_eq!(state.events.first_pr_deployment(repo, 7).unwrap(), expected);
    assert_eq!(state.events.first_pr_deployment(repo, 8).unwrap(), None);
    assert_eq!(
        state
            .events
            .first_pr_deployment("other/widgets-web", 7)
            .unwrap(),
        None
    );
}

/// Work a todo carries is projected onto the todo's lot, not `repo#pr`: its
/// pull request's deployment is still dated.
#[test]
fn a_todo_bound_pull_request_is_dated_by_its_own_deployment() {
    use super::super::types::NewEvent;
    let root = tempfile::tempdir().unwrap();
    let state = WebState::new_with_git_storage(ForgeCore::new(), root.path().to_path_buf());
    let now = Utc::now().timestamp_millis();
    let repo = "acme/widgets-web";
    let post = |kind: &str, todo: Option<&str>, pr: Option<i64>, outcome: Option<&str>, offset| {
        state
            .events
            .insert(
                "operator",
                &NewEvent {
                    source: "fixture".into(),
                    kind: kind.into(),
                    summary: kind.into(),
                    repo: pr.map(|_| repo.into()),
                    pr,
                    todo_id: todo.map(str::to_string),
                    outcome: outcome.map(str::to_string),
                    detail: Some(json!({"environment": "production"})),
                    ..Default::default()
                },
                now + offset,
            )
            .unwrap();
    };
    let todo = "20261021-000000-aaaaaa";
    post("todo.filed", Some(todo), None, None, 0);
    post("pr.opened", Some(todo), Some(4), None, 1000);
    post("pr.opened", Some(todo), Some(5), None, 1500);
    post("pr.merged", None, Some(4), None, 2000);
    post("deploy.created", None, Some(4), None, 3000);
    post("deploy.status", None, Some(4), Some("success"), 4000);
    let expected = Some(crate::web::shift::rfc3339_ms(now + 4000));
    assert_eq!(state.events.first_pr_deployment(repo, 4).unwrap(), expected);
    // The todo's other pull request has not shipped: it is not dated from
    // the deployment its sibling had.
    assert_eq!(state.events.first_pr_deployment(repo, 5).unwrap(), None);
    let all = state.events.first_pr_deployments().unwrap();
    assert_eq!(all.get(&(repo.to_string(), 4)), expected.as_ref());
    assert!(!all.contains_key(&(repo.to_string(), 5)));
}
