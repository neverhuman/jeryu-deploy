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
