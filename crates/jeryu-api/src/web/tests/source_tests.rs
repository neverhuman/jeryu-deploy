use super::*;

#[tokio::test]
async fn repo_refs_use_the_repository_default_branch_for_protection() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "trunk-repo".to_string(),
                private: false,
                description: None,
                default_branch: Some("trunk".to_string()),
            },
        )
        .unwrap();
    let state = Arc::new(WebState::new(core));

    let response = repo_refs(State(state), AxumPath(repo.id.to_string())).await;
    let refs = response_json(response).await;
    assert_eq!(refs.as_array().expect("refs array")[0]["name"], "trunk");
    assert_eq!(refs[0]["protected"], true);
}

#[tokio::test]
async fn readme_update_round_trips_through_the_local_api() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: Some("forge".to_string()),
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    let state = Arc::new(WebState::new(core));
    let markdown = "# Managed README\n\n- score: 92\n".to_string();
    let payload = serde_json::json!({ "markdown": markdown.clone() });
    let updated = response_json(
        repo_readme_update(
            State(state.clone()),
            AxumPath(repo.id.to_string()),
            axum::body::Bytes::from(serde_json::to_vec(&payload).unwrap()),
        )
        .await,
    )
    .await;
    assert_eq!(updated["markdown"], markdown);
    assert!(updated["html"].as_str().unwrap().contains("Managed README"));

    let readme = response_json(
        repo_readme(
            State(state.clone()),
            AxumPath(repo.id.to_string()),
            Query(crate::web::repositories::SourceQuery::default()),
        )
        .await,
    )
    .await;
    assert_eq!(readme["markdown"], markdown);
    assert!(readme["html"].as_str().unwrap().contains("Managed README"));

    // The README is read through /readme; a blob read without a path is an
    // input error rather than a silent README fallback.
    let blob = repo_blob(
        State(state.clone()),
        AxumPath(repo.id.to_string()),
        Query(crate::web::repositories::SourceQuery::default()),
    )
    .await;
    assert_eq!(blob.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let raw = repo_raw(
        State(state),
        AxumPath(repo.id.to_string()),
        Query(crate::web::repositories::SourceQuery::default()),
    )
    .await;
    let raw_bytes = axum::body::to_bytes(raw.into_body(), usize::MAX)
        .await
        .expect("raw response bytes");
    assert!(
        std::str::from_utf8(&raw_bytes)
            .unwrap()
            .contains("Managed README")
    );
}

#[test]
fn markdown_renderer_escapes_html_and_builds_toc() {
    let rendered = render_markdown("# Hello <world>\n\nbody");
    assert!(rendered.html.contains("&lt;world&gt;"));
    assert_eq!(rendered.toc[0].id, "hello-world");
}

#[tokio::test]
async fn browser_repo_routes_serve_the_spa_shell() {
    use axum::body::Body;
    use axum::http::Request;
    use tempfile::tempdir;
    use tower::ServiceExt;

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
    .unwrap();
    let spa_dir = tempdir().expect("temp SPA dir");
    std::fs::write(
        spa_dir.path().join("index.html"),
        r#"<!doctype html><html><body><div id="root"></div></body></html>"#,
    )
    .expect("write SPA stub");
    let app = app(WebState::new(core), spa_dir.path());

    let api = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/repos")
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(api.status(), StatusCode::OK);
    assert_eq!(
        api.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let api_body = response_json(api).await;
    assert!(
        api_body.to_string().contains("alice"),
        "JSON clients must still reach the REST edge"
    );

    for path in [
        "/repos",
        "/repos/family/jeryu-split",
        "/repos/alice/jeryu",
        "/repos/alice/jeryu/pulls/99",
        "/repos/alice/jeryu/settings/merge",
        "/repos/jeryu/alice/jeryu",
        "/repos/jeryu/alice/jeryu/code",
        "/repos/jeryu/alice/jeryu/blob/main/src/lib.rs",
        "/repos/jeryu/alice/jeryu/settings/general",
        "/repos/jeryu/alice/jeryu/agents/run-1",
        "/tools",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(
                        header::ACCEPT,
                        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                    )
                    .header(header::USER_AGENT, "Mozilla/5.0 (browser)")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "path {path}");
        assert!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/html")),
            "path {path} must serve the SPA shell"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("browser shell body");
        let body = std::str::from_utf8(&bytes).expect("browser shell is utf-8");
        assert!(
            body.contains(r#"<div id="root"></div>"#),
            "path {path} must serve the SPA shell"
        );
    }
}

#[tokio::test]
async fn source_browser_rejects_unsafe_paths_before_storage_lookup() {
    let core = ForgeCore::new();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "jeryu-core".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new(core));
    for path in ["/etc/passwd", "../Cargo.toml", "src/\0secret"] {
        let response = crate::web::repositories::repo_tree(
            State(state.clone()),
            AxumPath("jeryu/jeryu-core".to_string()),
            Query(crate::web::repositories::SourceQuery {
                path: Some(path.to_string()),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsafe path should be rejected before git storage lookup: {path:?}"
        );
    }
}

/// A link to a file that is on disk but was never committed (a generated
/// `.jankurai/repo-score.md`, live on 2026-09-19) answered 500, because git's
/// "Needed a single revision" was reported as a server fault. A path that is
/// not at the ref is a 404, for the blob, the raw and the tree routes; a
/// directory asked for as a file says so.
#[tokio::test]
async fn source_browser_answers_404_for_a_path_that_is_not_at_the_ref() {
    use crate::web::shift::tests::run_git;
    let dir = tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join("docs")).unwrap();
    run_git(&work, &["init", "-q"]);
    std::fs::write(work.join("docs/guide.md"), "# Guide\n").unwrap();
    run_git(&work, &["add", "."]);
    run_git(&work, &["commit", "-q", "-m", "base"]);
    let owner = dir.path().join("jeryu");
    std::fs::create_dir_all(&owner).unwrap();
    run_git(&owner, &["init", "-q", "--bare", "jeryu-core.git"]);
    run_git(
        &work,
        &[
            "push",
            "-q",
            owner.join("jeryu-core.git").to_str().unwrap(),
            "main",
        ],
    );
    let core = ForgeCore::new();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "jeryu-core".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new_with_git_storage(
        core,
        dir.path().to_path_buf(),
    ));
    let query = |path: &str, render: Option<&str>| {
        Query(crate::web::repositories::SourceQuery {
            path: Some(path.to_string()),
            render: render.map(str::to_string),
            ref_name: Some("main".to_string()),
        })
    };
    let id = || AxumPath("jeryu/jeryu-core".to_string());

    let found = repo_blob(
        State(state.clone()),
        id(),
        query("docs/guide.md", Some("html")),
    )
    .await;
    assert_eq!(found.status(), StatusCode::OK);

    for render in [None, Some("html")] {
        let missing = repo_blob(
            State(state.clone()),
            id(),
            query(".jankurai/repo-score.md", render),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND, "render={render:?}");
        assert_eq!(response_json(missing).await["code"], "not_found");
    }
    let raw = repo_raw(State(state.clone()), id(), query("no/such/file.txt", None)).await;
    assert_eq!(raw.status(), StatusCode::NOT_FOUND);
    let tree =
        crate::web::repositories::repo_tree(State(state.clone()), id(), query("no/such/dir", None))
            .await;
    assert_eq!(tree.status(), StatusCode::NOT_FOUND);

    let directory = repo_blob(State(state.clone()), id(), query("docs", None)).await;
    assert_eq!(directory.status(), StatusCode::NOT_FOUND);
    assert_eq!(response_json(directory).await["code"], "not_a_file");
    let listed = crate::web::repositories::repo_tree(State(state), id(), query("docs", None)).await;
    assert_eq!(listed.status(), StatusCode::OK);
}

/// A blob read with a missing `path` or `ref` answered 200 with an empty
/// file and `"sha":"unknown"`, so a typoed parameter looked like a real,
/// empty file. Both are required and a miss names the missing one.
#[tokio::test]
async fn repo_blob_rejects_a_missing_path_or_ref() {
    let core = ForgeCore::new();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "jeryu-core".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new(core));
    let cases = [
        (None, Some("main"), "path is required"),
        (Some("  "), Some("main"), "path is required"),
        (Some("src/lib.rs"), None, "ref is required"),
        (Some("src/lib.rs"), Some(""), "ref is required"),
        (None, None, "path is required"),
    ];
    for (path, ref_name, message) in cases {
        let response = repo_blob(
            State(state.clone()),
            AxumPath("jeryu/jeryu-core".to_string()),
            Query(crate::web::repositories::SourceQuery {
                path: path.map(str::to_string),
                ref_name: ref_name.map(str::to_string),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "path={path:?} ref={ref_name:?}"
        );
        let body = response_json(response).await;
        assert_eq!(body["code"], "invalid_input");
        assert_eq!(body["message"], message);
    }
}
