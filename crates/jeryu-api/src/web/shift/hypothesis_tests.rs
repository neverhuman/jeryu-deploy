//! Authenticated queue writes retain the front-matter contract on reread.
use super::tests::{body_json, request, run_git};
use crate::web::{WebState, app};
use axum::http::{Method, StatusCode};
use jeryu_core::{CreateRepositoryRequest, ForgeCore, RepoAccessLevel, UserRole};
use serde_json::{Value, json};
use std::path::Path;
use tower::ServiceExt;

#[tokio::test]
async fn hypothesis_action_persists_and_refuses_a_non_owner() {
    let dir = tempfile::tempdir().unwrap();
    let owner = dir.path().join("acme");
    let work = dir.path().join("queue-source");
    std::fs::create_dir_all(&owner).unwrap();
    std::fs::create_dir_all(work.join("todos")).unwrap();
    std::fs::write(work.join("family.toml"), "[family]\nname = \"acme\"\nbase_branch = \"main\"\nlanding = \"shifts\"\nshift_tz = \"UTC\"\n[[repo]]\nname = \"widget\"\norder = 1\n").unwrap();
    std::fs::write(work.join("todos/20261001-000000-aaaaaa-checkout.md"), "+++\nid = \"20261001-000000-aaaaaa\"\nfamily = \"acme\"\ntitle = \"Checkout\"\nrepos = [\"widget\"]\nstatus = \"done\"\nrequested_by = \"owner\"\nhypothesis = \"Users finish checkout faster.\"\ncheck_after = 7\n+++\nMeasure checkout time.\n").unwrap();
    run_git(&work, &["init", "-q"]);
    run_git(&work, &["checkout", "-q", "-b", "queue"]);
    run_git(&work, &["add", "."]);
    run_git(&work, &["commit", "-q", "-m", "seed"]);
    run_git(&owner, &["init", "-q", "--bare", "acme-todo.git"]);
    run_git(
        &work,
        &[
            "push",
            "-q",
            owner.join("acme-todo.git").to_str().unwrap(),
            "queue",
        ],
    );
    let core = ForgeCore::new();
    for (login, role) in [
        ("admin", UserRole::Admin),
        ("owner", UserRole::User),
        ("other", UserRole::User),
    ] {
        core.create_account(login, "test-password", role).unwrap();
    }
    core.create_repository(
        "acme",
        CreateRepositoryRequest {
            name: "widget".into(),
            private: true,
            description: None,
            default_branch: Some("main".into()),
        },
    )
    .unwrap();
    for login in ["owner", "other"] {
        core.grant_repo_access("admin", login, "acme", "widget", RepoAccessLevel::Write)
            .unwrap();
    }
    let token = |login| {
        core.create_personal_access_token(login, "test", None)
            .unwrap()
            .secret
    };
    let (admin, owner_token, other) = (token("admin"), token("owner"), token("other"));
    let state = WebState::new_with_git_storage(core, dir.path().to_path_buf())
        .with_auth(true, false, false);
    let now = chrono::Utc::now().timestamp_millis();
    for kind in ["pr.merged", "deploy.status"] {
        let event = crate::web::pipeline::NewEvent {
            family: Some("acme".into()),
            todo_id: Some("20261001-000000-aaaaaa".into()),
            repo: Some("acme/widget".into()),
            pr: Some(1),
            outcome: (kind == "deploy.status").then(|| "success".into()),
            ..crate::web::pipeline::NewEvent::forge(kind, kind)
        };
        state.events.insert("node-a", &event, now).unwrap();
    }
    let router = app(state, Path::new("/nonexistent-spa"));
    let uri = "/api/v1/shift/todos/acme/20261001-000000-aaaaaa/action";
    let value = json!({"action":"outcome_check", "value":{"outcome":"met", "outcome_note":"Median checkout time fell.", "check_after":"2026-10-15"}});
    let refused = router
        .clone()
        .oneshot(request(Method::POST, uri, &other, Some(value.clone())))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let saved = router
        .clone()
        .oneshot(request(Method::POST, uri, &owner_token, Some(value)))
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::OK);
    let saved = body_json(saved).await;
    assert_eq!(saved["outcome"], "met");
    assert_eq!(saved["hypothesis"], "Users finish checkout faster.");
    let list = router
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v1/shift/todos?family=acme",
            &owner_token,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let list = body_json(list).await;
    assert_eq!(
        list["todos"][0]["outcome_note"],
        "Median checkout time fell."
    );
    assert_eq!(list["todos"][0]["check_after"], "2026-10-15");
    let reset = router.clone().oneshot(request(Method::POST, uri, &owner_token,
        Some(json!({"action":"outcome_check", "value":{"outcome":"not_checked", "check_after":0}})))).await.unwrap();
    assert_eq!(reset.status(), StatusCode::OK);
    let attention = router
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v1/attention?family=acme",
            &admin,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(attention.status(), StatusCode::OK);
    let attention = body_json(attention).await;
    let reminder = attention["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "todo_check_outcome")
        .expect("deployed todo's reminder");
    assert_eq!(reminder["assignee"], "owner");
    let check = router
        .clone()
        .oneshot(request(
            Method::POST,
            uri,
            &owner_token,
            Some(json!({"action":"outcome_check", "value":{"outcome":"met"}})),
        ))
        .await
        .unwrap();
    assert_eq!(check.status(), StatusCode::OK);
    let scorecard = router
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v1/scorecard?family=acme",
            &admin,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(scorecard.status(), StatusCode::OK);
    let scorecard = body_json(scorecard).await;
    let metric = &scorecard["metrics"]["hypotheses_checked_and_met"]["value"];
    assert_eq!(metric["shipped"], 1);
    assert_eq!(metric["with_hypothesis"], 1);
    assert_eq!(metric["checked"], 1);
    assert_eq!(metric["met_share_pct"], 100.0);
    let attention = router
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v1/attention?family=acme",
            &admin,
            None,
        ))
        .await
        .unwrap();
    assert!(
        !body_json(attention).await["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "todo_check_outcome")
    );
    let invalid = router
        .oneshot(request(
            Method::POST,
            uri,
            &admin,
            Some(json!({"action":"outcome_check", "value":{"outcome":"yes"}})),
        ))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let _: Value = body_json(invalid).await;
}
