use super::*;

#[tokio::test]
async fn signup_issues_session_cookie_for_followup_api_calls() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = app(
        WebState::new(ForgeCore::new()).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/signup")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "login": "newuser",
                        "password": "correct horse"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let signup_cookie_header = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .expect("signup sets a session cookie")
        .to_string();
    assert!(
        !signup_cookie_header.contains("Max-Age="),
        "signup keeps the browser-session cookie behavior"
    );
    let cookie = signup_cookie_header
        .split(';')
        .next()
        .expect("cookie name and value")
        .to_string();
    let body = response_json(response).await;
    assert_eq!(body["login"], "newuser");
    assert_eq!(body["role"], "user");

    let me = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(me.status(), StatusCode::OK);
    let body = response_json(me).await;
    assert_eq!(body["login"], "newuser");
    assert_eq!(body["mustChangePassword"], false);
    assert!(body["csrfToken"].as_str().is_some());
}

#[tokio::test]
async fn login_remember_me_controls_session_cookie_max_age_and_logout_expiry() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_account("remembered", "correct horse battery", UserRole::User)
        .unwrap();
    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );

    let normal_login = app
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "login": "remembered",
                        "password": "correct horse battery"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(normal_login.status(), StatusCode::OK);
    let normal_cookie = normal_login
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .expect("normal login sets a session cookie")
        .to_string();
    assert!(normal_cookie.contains("jeryu-session="));
    assert!(normal_cookie.contains("HttpOnly"));
    assert!(normal_cookie.contains("SameSite=Lax"));
    assert!(
        !normal_cookie.contains("Max-Age="),
        "normal logins stay browser-session scoped"
    );
    let normal_body = response_json(normal_login).await;
    assert!(normal_body["csrfToken"].as_str().is_some());

    let remembered_login = app
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "login": "remembered",
                        "password": "correct horse battery",
                        "rememberMe": true
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(remembered_login.status(), StatusCode::OK);
    let remembered_cookie_header = remembered_login
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .expect("remembered login sets a persistent session cookie")
        .to_string();
    assert!(remembered_cookie_header.contains("jeryu-session="));
    assert!(remembered_cookie_header.contains("Max-Age=2592000"));
    assert!(remembered_cookie_header.contains("HttpOnly"));
    assert!(remembered_cookie_header.contains("SameSite=Lax"));
    let remembered_cookie = remembered_cookie_header
        .split(';')
        .next()
        .expect("cookie name and value")
        .to_string();
    let remembered_body = response_json(remembered_login).await;
    let remembered_csrf = remembered_body["csrfToken"]
        .as_str()
        .expect("remembered login includes csrf token")
        .to_string();

    let logout = app
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/logout")
                .header(header::COOKIE, remembered_cookie)
                .header("x-jeryu-csrf", remembered_csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::OK);
    let expired_cookie = logout
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .expect("logout expires the session cookie");
    assert!(expired_cookie.contains("jeryu-session="));
    assert!(expired_cookie.contains("Max-Age=0"));
}

#[test]
fn bootstrap_admin_password_creates_and_resets_admin_without_receipt_secret() {
    let state = WebState::new(ForgeCore::new());
    let data_dir = tempdir().expect("bootstrap data dir");

    bootstrap_public_accounts_with_admin_password(
        &state,
        data_dir.path(),
        Some("operator-admin-password-123"),
    )
    .expect("bootstrap admin with operator password");
    let admin = state
        .core
        .authenticate_password("jeryu-admin", "operator-admin-password-123")
        .expect("operator admin password works");
    assert_eq!(admin.role, UserRole::Admin);
    assert!(!admin.must_change_password);

    let receipt: Value = serde_json::from_slice(
        &std::fs::read(data_dir.path().join("bootstrap-credentials.json"))
            .expect("bootstrap receipt exists for non-admin users"),
    )
    .expect("bootstrap receipt is json");
    let logins: Vec<_> = receipt["credentials"]
        .as_array()
        .expect("credentials array")
        .iter()
        .filter_map(|credential| credential["login"].as_str())
        .collect();
    assert_eq!(logins, vec!["jordanh", "jepsont"]);

    bootstrap_public_accounts_with_admin_password(
        &state,
        data_dir.path(),
        Some("operator-admin-password-456"),
    )
    .expect("bootstrap admin reset with operator password");
    assert!(
        state
            .core
            .authenticate_password("jeryu-admin", "operator-admin-password-123")
            .is_err(),
        "reset must revoke the prior bootstrap admin password"
    );
    let reset = state
        .core
        .authenticate_password("jeryu-admin", "operator-admin-password-456")
        .expect("reset operator admin password works");
    assert_eq!(reset.role, UserRole::Admin);
    assert!(!reset.must_change_password);
}

#[tokio::test]
async fn admin_sees_all_repositories_but_fresh_signup_sees_none() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    for name in ["jeryu-core", "jeryu-web"] {
        core.create_repository(
            "jeryu",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: true,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }
    core.create_account("jeryu-admin", "admin-password-123", UserRole::Admin)
        .unwrap();
    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );

    let admin_login = app
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "login": "jeryu-admin",
                        "password": "admin-password-123"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(admin_login.status(), StatusCode::OK);
    let admin_cookie = admin_login
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .expect("admin login sets a session cookie")
        .split(';')
        .next()
        .expect("cookie name and value")
        .to_string();
    let admin_repos = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/repos")
                .header(header::COOKIE, admin_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(admin_repos.status(), StatusCode::OK);
    let admin_repos_body = response_json(admin_repos).await;
    assert_eq!(admin_repos_body["total"], 2);
    let admin_names: Vec<_> = admin_repos_body["repositories"]
        .as_array()
        .expect("repositories array")
        .iter()
        .filter_map(|repo| repo["id"]["name"].as_str())
        .collect();
    assert!(admin_names.contains(&"jeryu-core"));
    assert!(admin_names.contains(&"jeryu-web"));

    let signup = app
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/signup")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "login": "freshuser",
                        "password": "fresh-password-123"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(signup.status(), StatusCode::OK);
    let fresh_cookie = signup
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .expect("signup sets a session cookie")
        .split(';')
        .next()
        .expect("cookie name and value")
        .to_string();
    let fresh_repos = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/repos")
                .header(header::COOKIE, fresh_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fresh_repos.status(), StatusCode::OK);
    let fresh_repos_body = response_json(fresh_repos).await;
    assert_eq!(fresh_repos_body["total"], 0);
    assert_eq!(
        fresh_repos_body["repositories"]
            .as_array()
            .expect("repositories array")
            .len(),
        0
    );
}

#[tokio::test]
async fn forced_password_change_blocks_other_authenticated_routes_until_changed() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    let temporary_password = ["temporary", "pass", "123"].join("-");
    core.create_temporary_account("resetuser", &temporary_password, UserRole::User)
        .expect("create temporary account");
    let session = core.create_session("resetuser").expect("create session");
    let cookie = format!("jeryu-session={}", session.token);
    let csrf = session.session.csrf_token.clone();
    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );

    let me = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(me.status(), StatusCode::OK);
    let me_body = response_json(me).await;
    assert_eq!(me_body["login"], "resetuser");
    assert_eq!(me_body["mustChangePassword"], true);

    let repos = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/repos")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(repos.status(), StatusCode::FORBIDDEN);
    let repos_body = response_json(repos).await;
    assert_eq!(repos_body["code"], "password_change_required");

    let token_denied = app
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/tokens")
                .header(header::COOKIE, &cookie)
                .header("x-jeryu-csrf", &csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({ "name": "cli" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(token_denied.status(), StatusCode::FORBIDDEN);
    let token_denied_body = response_json(token_denied).await;
    assert_eq!(token_denied_body["code"], "password_change_required");

    let changed = app
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/password")
                .header(header::COOKIE, &cookie)
                .header("x-jeryu-csrf", &csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "currentPassword": temporary_password,
                        "newPassword": "new-password-12345"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::OK);
    let rotated_cookie = changed
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .expect("password change rotates the session cookie")
        .split(';')
        .next()
        .expect("rotated cookie name and value")
        .to_string();
    let changed_body = response_json(changed).await;
    assert_eq!(changed_body["mustChangePassword"], false);
    let rotated_csrf = changed_body["csrfToken"]
        .as_str()
        .expect("password change returns the rotated CSRF token")
        .to_string();

    let old_session = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(old_session.status(), StatusCode::UNAUTHORIZED);

    let token_created = app
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/tokens")
                .header(header::COOKIE, &rotated_cookie)
                .header("x-jeryu-csrf", &rotated_csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({ "name": "cli" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(token_created.status(), StatusCode::OK);
}

#[tokio::test]
async fn cookie_auth_mutations_require_csrf_and_pat_lifecycle_is_user_scoped() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = app(
        WebState::new(ForgeCore::new()).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let signup = app
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/signup")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "login": "tokenuser",
                        "password": "correct horse"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = signup
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .expect("signup sets cookie")
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let signup_body = response_json(signup).await;
    let csrf = signup_body["csrfToken"].as_str().expect("csrf token");

    let denied = app
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/tokens")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({ "name": "cli" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/tokens")
                .header(header::COOKIE, &cookie)
                .header("x-jeryu-csrf", csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({ "name": "cli" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    let created_body = response_json(created).await;
    let token_id = created_body["id"].as_str().expect("token id").to_string();
    assert!(
        created_body["token"]
            .as_str()
            .unwrap_or("")
            .starts_with("jpat_")
    );
    assert!(created_body["expiresAt"].as_str().is_some());

    let list = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/tokens")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let list_body = response_json(list).await;
    assert_eq!(list_body.as_array().unwrap().len(), 1);
    assert!(list_body[0].get("token").is_none(), "list omits PAT secret");

    let deleted = app
        .oneshot(
            Request::builder()
                .method(HttpMethod::DELETE)
                .uri(format!("/api/v1/auth/tokens/{token_id}"))
                .header(header::COOKIE, &cookie)
                .header("x-jeryu-csrf", csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn auth_rate_limit_returns_429_for_repeated_login_failures() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = app(
        WebState::new(ForgeCore::new()).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let mut last_status = StatusCode::OK;
    for _ in 0..=10 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(HttpMethod::POST)
                    .uri("/api/v1/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "login": "missing",
                            "password": "incorrect password"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        last_status = response.status();
    }
    assert_eq!(last_status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn anonymous_git_read_follows_repository_visibility() {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use tower::ServiceExt;

    let storage = tempdir().unwrap();
    let core = ForgeCore::new();
    for (name, private) in [("open", false), ("closed", true)] {
        core.create_repository(
            "jeryu",
            CreateRepositoryRequest {
                name: name.to_string(),
                private,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }
    let app = app(
        WebState::new_with_git_storage(core, storage.path().to_path_buf())
            .with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let anonymous = |uri: &str| {
        let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(std::net::SocketAddr::from((
                [203, 0, 113, 7],
                40000,
            ))));
        app.clone().oneshot(request)
    };

    let public_read = anonymous("/git/jeryu/open.git/info/refs?service=git-upload-pack")
        .await
        .unwrap();
    assert_ne!(public_read.status(), StatusCode::UNAUTHORIZED);
    assert!(public_read.headers().get("www-authenticate").is_none());

    for uri in [
        "/git/jeryu/open.git/info/refs?service=git-receive-pack",
        "/git/jeryu/closed.git/info/refs?service=git-upload-pack",
        "/git/jeryu/missing.git/info/refs?service=git-upload-pack",
    ] {
        let response = anonymous(uri).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
}

#[tokio::test]
async fn trust_local_dev_requires_loopback_peer() {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = app(
        WebState::new(ForgeCore::new()).with_auth(true, true, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let no_peer = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(no_peer.status(), StatusCode::UNAUTHORIZED);

    let mut request = Request::builder()
        .uri("/api/v1/auth/me")
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            9988,
        ))));
    let loopback = app.oneshot(request).await.unwrap();
    assert_eq!(loopback.status(), StatusCode::OK);
}

#[tokio::test]
async fn repeated_bad_tokens_answer_429_with_retry_after() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let app = app(
        WebState::new(ForgeCore::new()).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let send = |app: axum::Router| async move {
        app.oneshot(
            Request::builder()
                .uri("/api/v1/work")
                .header(header::AUTHORIZATION, "Bearer not-a-real-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    };
    for _ in 0..20 {
        assert_eq!(send(app.clone()).await.status(), StatusCode::UNAUTHORIZED);
    }
    let limited = send(app.clone()).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers()[header::RETRY_AFTER], "60");
    assert_eq!(limited.headers()["x-ratelimit-remaining"], "0");
    assert!(limited.headers().contains_key("x-ratelimit-reset"));
}

/// A metered read says what is left of its window, so a polling agent can slow
/// down before it is refused instead of discovering the cap as a 429.
#[tokio::test]
async fn a_metered_read_carries_the_rate_limit_budget() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::User)
        .unwrap();
    let token = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let read = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::OK);
    let headers = read.headers();
    assert_eq!(headers["x-ratelimit-limit"], "600");
    assert_eq!(headers["x-ratelimit-remaining"], "599");
    assert!(headers.contains_key("x-ratelimit-reset"));
    // The budget is for the limited reads, not an advisory on every answer.
    assert!(!headers.contains_key(header::RETRY_AFTER));
}
