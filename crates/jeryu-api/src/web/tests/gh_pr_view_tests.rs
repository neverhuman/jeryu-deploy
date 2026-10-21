use super::*;
use axum::body::Body;
use axum::http::Request;
use jeryu_core::{
    BotKind, BotReach, BotRepoRef, BotTaskKind, MergePullRequestRequest, PullRequestState,
    UpdatePullRequestRequest,
};
use std::io::Write;
use std::process::{Command, Stdio};
use tower::ServiceExt;

fn request(repo: &str, number: u64) -> String {
    let mut captured: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/gh-pr-view/request.json"
    ))
    .unwrap();
    captured["body"]["variables"]["repo"] = json!(repo);
    captured["body"]["variables"]["pr_number"] = json!(number);
    captured["body"].to_string()
}

fn seed() -> (ForgeCore, Vec<Value>, String) {
    let core = ForgeCore::new();
    core.create_account("ada", "fixture-password", UserRole::Admin)
        .unwrap();
    for repo in ["widgets", "vault"] {
        core.create_repository(
            "ada",
            CreateRepositoryRequest {
                name: repo.into(),
                private: true,
                default_branch: Some("main".into()),
                description: None,
            },
        )
        .unwrap();
    }
    let mut expected = Vec::new();
    for (index, state) in ["draft", "ready", "closed", "merged"].iter().enumerate() {
        let sha = format!("{:040x}", index + 1);
        let pr = core
            .create_pull_request(
                "ada",
                "widgets",
                "ada",
                CreatePullRequestRequest {
                    title: format!("{state} PR"),
                    head: format!("feature-{index}"),
                    base: "main".into(),
                    head_sha: Some(sha.clone()),
                    draft: *state == "draft",
                    ..Default::default()
                },
            )
            .unwrap();
        if *state == "closed" {
            core.update_pull_request(
                "ada",
                "widgets",
                pr.number,
                UpdatePullRequestRequest {
                    state: Some(PullRequestState::Closed),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        if *state == "merged" {
            core.merge_pull_request(
                "ada",
                "widgets",
                pr.number,
                MergePullRequestRequest::default(),
            )
            .unwrap();
        }
        expected.push(json!({"number": pr.number, "title": pr.title, "state": if *state == "merged" {"MERGED"} else if *state == "closed" {"CLOSED"} else {"OPEN"}, "isDraft": pr.draft, "headRefOid": sha, "url": crate::links::absolute(&crate::links::native_pull_app_path("ada", "widgets", pr.number))}));
    }
    let token = core
        .create_personal_access_token("ada", "fixture", None)
        .unwrap()
        .secret;
    (core, expected, token)
}

async fn send(router: &AxumRouter, path: &str, token: &str, body: String) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::HOST, "127.0.0.1")
                .header(
                    header::AUTHORIZATION,
                    format!(
                        "{} {token}",
                        if path.starts_with("/api/v1/") {
                            "Bearer"
                        } else {
                            "token"
                        }
                    ),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (status, response_json(response).await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_gh_245_pr_view_reads_four_lifecycle_fixtures() {
    let (core, pulls, token) = seed();
    let directory = tempdir().unwrap();
    let router = app(
        WebState::new(core).with_auth(true, false, false),
        directory.path(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let result = tokio::task::spawn_blocking(move || {
        let mut child = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/gh-pr-view/run.py"
            ))
            .arg(address.to_string())
            .arg(directory.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("python3, openssl and gh 2.45.0 are required for the compatibility fixture");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"pulls": pulls, "token": token})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    server.abort();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[tokio::test]
async fn pr_view_checks_principal_agent_reach_and_rejects_modified_queries() {
    let (core, expected, admin) = seed();
    core.create_account("bob", "fixture-password", UserRole::User)
        .unwrap();
    let bob = core
        .create_personal_access_token("bob", "fixture", None)
        .unwrap()
        .secret;
    core.grant_repo_access("ada", "ada", "ada", "widgets", RepoAccessLevel::Read)
        .unwrap();
    let bot = core
        .enroll_bot(
            "ada",
            "atlas",
            "Atlas",
            BotKind::GROK,
            BotReach::Repositories {
                repos: vec![BotRepoRef {
                    owner: "ada".into(),
                    name: "widgets".into(),
                }],
            },
        )
        .unwrap();
    let directory = tempdir().unwrap();
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        directory.path(),
    );
    let (status, issued) = send(
        &router,
        &format!("/api/v1/bots/{}/tokens", bot.bot.id),
        &admin,
        json!({"label":"fixture","ttl_days":7}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    let agent = issued["token"].as_str().unwrap();
    for path in ["/api/graphql", "/graphql", "/api/v3/graphql"] {
        let (status, denied) = send(&router, path, &bob, request("widgets", 1)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(denied["data"]["repository"], Value::Null);
        let (status, allowed) = send(&router, path, agent, request("widgets", 1)).await;
        assert_eq!(status, StatusCode::OK, "{allowed}");
        assert_eq!(
            allowed["data"]["repository"]["pullRequest"]["title"],
            expected[0]["title"]
        );
        let (status, denied) = send(&router, path, agent, request("vault", 1)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
        for body in [
            request("widgets", 1).replace("headRefOid", "unknownField"),
            request("widgets", 1).replace("{number,title", "{defaultBranchRef,number,title"),
            request("widgets", 1)
                .replace("query PullRequestByNumber", "mutation PullRequestByNumber"),
        ] {
            let (status, error) = send(&router, path, &admin, body).await;
            assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{error}");
            assert!(
                error["jeryu_repair_hint"]["repair_hint"]
                    .as_str()
                    .is_some_and(|hint| hint.contains("REST")),
                "{error}"
            );
            assert!(
                error["documentation_url"]
                    .as_str()
                    .is_some_and(|url| url.contains("graphql")),
                "{error}"
            );
        }
        let (status, missing) = send(&router, path, &admin, request("widgets", 99)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(missing["data"]["repository"]["pullRequest"], Value::Null);
    }
    let manifest = router
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
    let mapping = &manifest["gh_command_map"]["gh pr view <number> --repo <forge>/<owner>/<repo> --json number,title,state,isDraft,headRefOid,url"];
    assert!(
        mapping
            .as_str()
            .is_some_and(|value| value.contains("2.45.0") && value.contains("/api/graphql")),
        "{mapping}"
    );
    let task_bot = core
        .enroll_bot(
            "ada",
            "task-reader",
            "Task reader",
            BotKind::GROK,
            BotReach::Task {
                repo: BotRepoRef {
                    owner: "ada".into(),
                    name: "widgets".into(),
                },
                task_kind: BotTaskKind::Pull,
                task_id: "1".into(),
            },
        )
        .unwrap();
    let (status, issued) = send(
        &router,
        &format!("/api/v1/bots/{}/tokens", task_bot.bot.id),
        &admin,
        json!({"label":"fixture-task","ttl_days":7}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    let task_token = issued["token"].as_str().unwrap();
    let (status, allowed) = send(&router, "/api/graphql", task_token, request("widgets", 1)).await;
    assert_eq!(status, StatusCode::OK, "{allowed}");
    let (status, denied) = send(&router, "/api/graphql", task_token, request("vault", 2)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    core.grant_repo_access("ada", "bob", "ada", "widgets", RepoAccessLevel::Read)
        .unwrap();
    let (_, allowed) = send(&router, "/api/graphql", &bob, request("widgets", 1)).await;
    assert_eq!(allowed["data"]["repository"]["pullRequest"]["number"], 1);
}
