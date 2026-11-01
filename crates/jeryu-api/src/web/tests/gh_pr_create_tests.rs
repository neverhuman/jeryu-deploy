use super::*;
use axum::{body::Body, http::Request};
use jeryu_core::{BotKind, BotReach, BotRepoRef};
use std::{
    io::Write,
    process::{Command, Stdio},
};
use tower::ServiceExt;

struct Fixture {
    root: tempfile::TempDir,
    core: ForgeCore,
    router: AxumRouter,
    events: crate::web::pipeline::EventStore,
    review_wanted: crate::web::review_wanted::ReviewWantedStore,
    writer: String,
    reader: String,
    agent: String,
    head: String,
}
impl Fixture {
    async fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("gh-create-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let core = ForgeCore::new();
        for (login, role) in [
            ("ada", UserRole::Admin),
            ("writer", UserRole::User),
            ("reader", UserRole::User),
        ] {
            core.create_account(login, "fixture-password", role)
                .unwrap();
        }
        for repo in ["widgets", "vault"] {
            core.create_repository(
                "ada",
                CreateRepositoryRequest {
                    name: repo.into(),
                    private: true,
                    default_branch: Some("main".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let (_, head) = build_bare_repo_with_main_and_feature(
            root.path(),
            "ada",
            "widgets",
            "feature.txt",
            "new content",
        );
        for (login, access) in [
            ("writer", RepoAccessLevel::Write),
            ("reader", RepoAccessLevel::Read),
        ] {
            core.grant_repo_access("ada", login, "ada", "widgets", access)
                .unwrap();
        }
        let token = |login| {
            core.create_personal_access_token(login, "fixture", None)
                .unwrap()
                .secret
        };
        let admin = token("ada");
        let writer = token("writer");
        let reader = token("reader");
        core.grant_repo_access("ada", "ada", "ada", "widgets", RepoAccessLevel::Write)
            .unwrap();
        let bot = core
            .enroll_bot(
                "ada",
                "creator",
                "Creator",
                BotKind::GROK,
                BotReach::Repositories {
                    refs: None,
                    repos: vec![BotRepoRef {
                        owner: "ada".into(),
                        name: "widgets".into(),
                    }],
                },
            )
            .unwrap();
        let state = WebState::new_with_git_storage(core.clone(), root.path().to_path_buf())
            .with_auth(true, false, false);
        let events = state.events.clone();
        let review_wanted = state.review_wanted.clone();
        let router = app(state, root.path());
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/bots/{}/tokens", bot.bot.id))
                    .header(header::AUTHORIZATION, format!("Bearer {admin}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"label":"fixture","ttl_days":7}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let issued = response_json(response).await;
        let agent = issued["token"].as_str().unwrap().to_string();
        Self {
            root,
            core,
            router,
            events,
            review_wanted,
            writer,
            reader,
            agent,
            head,
        }
    }
}
fn mutation(repo: &str, head: &str) -> Value {
    json!({"query":super::super::gh_pr_create::CREATE,"variables":{"input":{
        "repositoryId":format!("jeryu-repository:ada/{repo}"),"headRefName":head,"baseRefName":"main",
        "title":"A title — preserved","body":"Line one\n\nLine two.\n","draft":true,"maintainerCanModify":true
    }}})
}
async fn send(f: &Fixture, token: &str, body: Value) -> (StatusCode, Value) {
    let response = f
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/graphql")
                .header(header::AUTHORIZATION, format!("token {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    (response.status(), response_json(response).await)
}
#[tokio::test]
async fn gh_draft_create_authorizes_validates_and_preserves_rest_domain_behavior() {
    let f = Fixture::new().await;
    for (token, repo, head, expected) in [
        (&f.reader, "widgets", "feature", StatusCode::FORBIDDEN),
        (&f.agent, "vault", "feature", StatusCode::FORBIDDEN),
        (&f.writer, "widgets", "missing", StatusCode::OK),
    ] {
        let (status, result) = send(&f, token, mutation(repo, head)).await;
        assert_eq!(status, expected, "{result}");
        assert!(
            result.get("errors").is_some() || result.get("message").is_some(),
            "{result}"
        );
        assert!(
            f.core
                .list_pull_requests("ada", "widgets", None)
                .unwrap()
                .is_empty()
        );
    }
    for field in ["draft", "baseRefName", "headRefName", "repositoryId"] {
        let mut body = mutation("widgets", "feature");
        body["variables"]["input"][field] = match field {
            "draft" => json!(false),
            "baseRefName" => json!("other"),
            "headRefName" => json!("feature~1"),
            _ => json!("jeryu-repository:ada/../widgets"),
        };
        let (_, denied) = send(&f, &f.writer, body).await;
        assert!(denied.get("errors").is_some(), "{denied}");
        assert!(
            f.core
                .list_pull_requests("ada", "widgets", None)
                .unwrap()
                .is_empty()
        );
    }
    let mut modified = mutation("widgets", "feature");
    modified["query"] = json!(super::super::gh_pr_create::CREATE.replace("url", "title"));
    assert_eq!(
        send(&f, &f.writer, modified).await.0,
        StatusCode::NOT_IMPLEMENTED
    );
    let manifest = f
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/capabilities")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let manifest = response_json(manifest).await;
    let guidance = &manifest["gh_command_map"]["gh pr create --draft --repo <forge>/<owner>/<repo> --base main --head <pushed-branch> --title <title> --body-file <file>"];
    assert!(
        guidance
            .as_str()
            .is_some_and(|s| s.contains("2.45.0") && s.contains("gh pr merge are not supported")),
        "{guidance}"
    );
    assert!(
        f.events
            .query(&crate::web::pipeline::EventsQuery::default())
            .unwrap()
            .is_empty()
    );
    assert!(f.review_wanted.since(0).1.is_empty());
    let (_, result) = send(&f, &f.writer, mutation("widgets", "feature")).await;
    assert!(
        result["data"]["createPullRequest"]["pullRequest"]["url"].is_string(),
        "{result}"
    );
    let (_, duplicate) = send(&f, &f.writer, mutation("widgets", "feature")).await;
    assert!(
        duplicate["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("already exists")
    );
    let pulls = f.core.list_pull_requests("ada", "widgets", None).unwrap();
    assert_eq!(pulls.len(), 1);
    assert!(pulls[0].draft);
    assert_eq!(pulls[0].author, "writer");
    assert_eq!(pulls[0].head.sha, f.head);
    assert_eq!(pulls[0].title, "A title — preserved");
    assert_eq!(pulls[0].body.as_deref(), Some("Line one\n\nLine two.\n"));
    let events = f
        .events
        .query(&crate::web::pipeline::EventsQuery {
            kind: Some("pr.opened".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        events.len(),
        1,
        "successful creation publishes once; duplicates publish nothing"
    );
    assert_eq!(events[0].repo.as_deref(), Some("ada/widgets"));
    assert_eq!(events[0].pr, Some(pulls[0].number as i64));
    assert_eq!(events[0].actor.as_deref(), Some("writer"));
    let (_, signals) = f.review_wanted.since(0);
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].repo, "ada/widgets");
    assert_eq!(signals[0].number, pulls[0].number);
    assert_eq!(signals[0].head_sha, f.head);
    assert!(signals[0].draft);
    assert!(
        f.core
            .list_pull_requests("ada", "vault", None)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires gh 2.45.0, python3 and openssl; run with --ignored"]
async fn actual_gh_245_pr_create_draft() {
    let f = Fixture::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = f.router.clone();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let input = json!({"token":f.writer,"reader":f.reader,"agent":f.agent,"url":crate::links::absolute(&crate::links::native_pull_app_path("ada","widgets",1))});
    let root = f.root.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        let mut child = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/gh-pr-create/run.py"
            ))
            .arg(address.to_string())
            .arg(root)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let pulls = f.core.list_pull_requests("ada", "widgets", None).unwrap();
    assert_eq!(pulls.len(), 1);
    assert!(pulls[0].draft);
    assert_eq!(pulls[0].title, "A title — preserved");
    assert_eq!(pulls[0].body.as_deref(), Some("Line one\n\nLine two.\n"));
    let events = f
        .events
        .query(&crate::web::pipeline::EventsQuery {
            kind: Some("pr.opened".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        events.len(),
        1,
        "successful creation publishes once; duplicates publish nothing"
    );
    assert_eq!(events[0].repo.as_deref(), Some("ada/widgets"));
    assert_eq!(events[0].pr, Some(pulls[0].number as i64));
    assert_eq!(events[0].actor.as_deref(), Some("writer"));
    let (_, signals) = f.review_wanted.since(0);
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].repo, "ada/widgets");
    assert_eq!(signals[0].number, pulls[0].number);
    assert_eq!(signals[0].head_sha, f.head);
    assert!(signals[0].draft);
    assert_eq!(pulls[0].head.sha, f.head);
    assert!(
        f.core
            .list_pull_requests("ada", "vault", None)
            .unwrap()
            .is_empty()
    );
}
