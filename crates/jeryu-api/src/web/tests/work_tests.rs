use super::*;

#[tokio::test]
async fn work_repo_create_persists_item_and_linked_issue() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .expect("create repo");
    let state = Arc::new(WebState::new(core));
    let created = response_json(
        crate::web::work::repo_create(
            State(state.clone()),
            authenticated_admin_account("alice"),
            AxumPath(repo.id.to_string()),
            Json(jeryu_jira::CreateWorkItemRequest {
                title: "Fix flaky CI".to_string(),
                body: Some("The required lane flickers.".to_string()),
                kind: Some(jeryu_jira::WorkItemKind::Ci),
                labels: vec!["ci".to_string()],
                ..jeryu_jira::CreateWorkItemRequest::default()
            }),
        )
        .await,
    )
    .await;
    assert_eq!(created["key"], "JRY-1");
    assert_eq!(created["repo"]["id"], repo.id.to_string());
    assert_eq!(created["issue"]["number"], 1);

    let issues = state
        .core
        .list_issues("alice", "jeryu", None)
        .expect("list issues");
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].title, "Fix flaky CI");

    let listed = response_json(
        crate::web::work::repo_list(
            State(state),
            authenticated_admin_account("alice"),
            AxumPath(repo.id.to_string()),
            Query(crate::web::work::WorkListQuery::default()),
        )
        .await,
    )
    .await;
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["items"][0]["issue"]["number"], 1);
}

#[test]
fn github_issue_create_is_mirrored_into_work() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .expect("create repo");
    let state = WebState::new(core);
    let created = state.github.handle(
        Method::Post,
        "/repos/alice/jeryu/issues",
        r#"{"title":"bug report","body":"it broke","labels":["bug"],"assignees":["alice"]}"#,
    );
    assert_eq!(created.status, 201, "create issue: {}", created.body);

    let work = state
        .work
        .list(jeryu_jira::WorkFilter::default())
        .expect("list work");
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].title, "bug report");
    assert_eq!(work[0].kind, jeryu_jira::WorkItemKind::Bug);
    let issue = work[0].issue.as_ref().expect("issue link");
    assert_eq!(issue.number, 1);
    assert_eq!(
        issue.url.as_deref(),
        Some("/repos/jeryu/alice/jeryu/issues#1")
    );

    let comment = state.github.handle(
        Method::Post,
        "/repos/alice/jeryu/issues/1/comments",
        r#"{"body":"confirmed reproduction","actor":"bob"}"#,
    );
    assert_eq!(comment.status, 201, "create comment: {}", comment.body);
    let detail = state.work.detail("JRY-1").expect("work detail");
    assert_eq!(detail.comments.len(), 1);
    assert_eq!(detail.comments[0].body, "confirmed reproduction");
    assert_eq!(detail.comments[0].author.id, "bob");

    let updated = state.github.handle(
        Method::Patch,
        "/repos/alice/jeryu/issues/1",
        r#"{"title":"fixed bug report","state":"closed","labels":["bug","p1"]}"#,
    );
    assert_eq!(updated.status, 200, "update issue: {}", updated.body);
    let synced = state.work.get("JRY-1").expect("synced work");
    assert_eq!(synced.title, "fixed bug report");
    assert_eq!(synced.status, jeryu_jira::WorkStatus::Done);
    assert_eq!(synced.labels, vec!["bug", "p1"]);
}

#[test]
fn pull_request_marker_issues_do_not_appear_as_work() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .expect("create repo");
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "change".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            ..CreatePullRequestRequest::default()
        },
    )
    .expect("create pr");
    assert_eq!(
        core.list_issues("alice", "jeryu", None)
            .expect("list marker issues")
            .len(),
        1
    );
    let state = WebState::new(core);
    let work = state
        .work
        .list(jeryu_jira::WorkFilter::default())
        .expect("list work");
    assert!(work.is_empty());
}
