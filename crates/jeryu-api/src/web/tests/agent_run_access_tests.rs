//! Viewer access to agent runs through the real auth gate: owner, repo writer,
//! non-owner stranger, and admin; forbidden routes; cross-origin upgrades.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Method, Request};
use jeryu_core::{CreateRepositoryRequest, ForgeCore, RepoAccessLevel, UserRole};
use tower::ServiceExt;

use axum::http::{HeaderMap, StatusCode, header};

use crate::web::WebState;
use crate::web::agent_runs::agent_run_viewer_id;
use crate::web::auth::websocket_origin_allowed;

struct Fixture {
    _root: tempfile::TempDir,
    app: axum::Router,
    tokens: BTreeMap<&'static str, String>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("fixture root");
        let core = ForgeCore::open_sqlite(root.path().join("forge.sqlite")).expect("core");
        let state = WebState::new_with_git_storage(core, root.path().join("git"))
            .with_auth(true, false, false);
        let mut tokens = BTreeMap::new();
        for login in ["admin", "owner", "writer", "stranger"] {
            let role = if login == "admin" {
                UserRole::Admin
            } else {
                UserRole::User
            };
            state
                .core
                .create_account(login, "agent-run-access-password", role)
                .expect("account");
            let token = state
                .core
                .create_personal_access_token(login, "agent run access", None)
                .expect("token")
                .secret;
            tokens.insert(login, token);
        }
        state
            .core
            .create_repository(
                "alice",
                CreateRepositoryRequest {
                    name: "demo".into(),
                    private: true,
                    default_branch: Some("main".into()),
                    ..Default::default()
                },
            )
            .expect("repo");
        state
            .core
            .grant_repo_access("admin", "writer", "alice", "demo", RepoAccessLevel::Write)
            .expect("grant");
        state
            .agent_runs
            .seed_test_session("ar-acl", "owner", "alice/demo", None);
        state
            .agent_runs
            .seed_test_session("ar-shell", "owner", "alice/demo", Some("shell"));
        let app = crate::web::app(state, &root.path().join("absent-spa"));
        Self {
            _root: root,
            app,
            tokens,
        }
    }

    async fn status(&self, method: Method, path: &str, login: &str) -> StatusCode {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.tokens[login]),
            )
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{\"kind\":\"interrupt\"}"))
            .expect("request");
        // A documentation-range peer, so local-dev trust never applies.
        request.extensions_mut().insert(ConnectInfo(
            "192.0.2.1:43210".parse::<SocketAddr>().expect("peer"),
        ));
        self.app
            .clone()
            .oneshot(request)
            .await
            .expect("response")
            .status()
    }
}

#[test]
fn viewer_routes_are_read_only_per_run_paths() {
    let get = Method::GET;
    assert_eq!(
        agent_run_viewer_id(&get, "/api/v1/agent-runs/ar-1"),
        Some("ar-1")
    );
    assert_eq!(
        agent_run_viewer_id(&get, "/api/v1/agent-runs/ar-1/events"),
        Some("ar-1")
    );
    assert_eq!(
        agent_run_viewer_id(&get, "/api/v1/agent-runs/ar-1/tty/stream"),
        Some("ar-1")
    );
    assert_eq!(
        agent_run_viewer_id(&get, "/api/v1/agent-runs/ar-1/attach"),
        Some("ar-1")
    );
    assert_eq!(agent_run_viewer_id(&get, "/api/v1/agent-runs"), None);
    assert_eq!(
        agent_run_viewer_id(&get, "/api/v1/agent-runs/ar-1/shell"),
        None
    );
    assert_eq!(
        agent_run_viewer_id(&get, "/api/v1/agent-runs/../admin"),
        None
    );
    assert_eq!(
        agent_run_viewer_id(&Method::POST, "/api/v1/agent-runs/ar-1/control"),
        None
    );
}

#[tokio::test]
async fn owners_and_repo_writers_may_watch_but_only_admins_drive() {
    let fixture = Fixture::new();
    let run = "/api/v1/agent-runs/ar-acl";
    for login in ["admin", "owner", "writer"] {
        assert_eq!(
            fixture.status(Method::GET, run, login).await,
            StatusCode::OK,
            "{login}"
        );
        let events = format!("{run}/events");
        assert_eq!(
            fixture.status(Method::GET, &events, login).await,
            StatusCode::OK,
            "{login}"
        );
    }
    assert_eq!(
        fixture.status(Method::GET, run, "stranger").await,
        StatusCode::FORBIDDEN
    );
    let missing = "/api/v1/agent-runs/ar-missing";
    assert_eq!(
        fixture.status(Method::GET, missing, "writer").await,
        StatusCode::FORBIDDEN
    );
    let control = format!("{run}/control");
    for login in ["owner", "writer"] {
        let status = fixture.status(Method::POST, &control, login).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{login} cannot drive a host PTY"
        );
    }
    let list = "/api/v1/agent-runs";
    assert_eq!(
        fixture.status(Method::GET, list, "owner").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture.status(Method::GET, list, "admin").await,
        StatusCode::OK
    );
}

#[test]
fn production_origin_pins_websocket_upgrades() {
    let headers = |origin: &str| {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("10.0.0.5:8787"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_str(origin).expect("origin"),
        );
        headers
    };
    let pinned = Some("https://forge.example/");
    assert!(websocket_origin_allowed(
        &headers("https://forge.example"),
        pinned
    ));
    assert!(
        !websocket_origin_allowed(&headers("http://10.0.0.5:8787"), pinned),
        "a matching Host is not enough once a public origin is configured"
    );
    assert!(websocket_origin_allowed(
        &headers("http://10.0.0.5:8787"),
        None
    ));
    assert!(!websocket_origin_allowed(
        &headers("https://evil.example"),
        Some("")
    ));
}

#[tokio::test]
async fn companion_shells_stay_admin_only_to_watch() {
    let fixture = Fixture::new();
    let shell = "/api/v1/agent-runs/ar-shell";
    for login in ["owner", "writer"] {
        let status = fixture.status(Method::GET, shell, login).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{login} cannot watch a host shell"
        );
    }
    assert_eq!(
        fixture.status(Method::GET, shell, "admin").await,
        StatusCode::OK
    );
}

/// Local-dev trust is ambient authority for a loopback peer: a foreign page
/// must not be able to open an attach socket with it.
#[tokio::test]
async fn local_dev_trust_still_refuses_cross_origin_websockets() {
    let root = tempfile::tempdir().expect("root");
    let core = ForgeCore::open_sqlite(root.path().join("forge.sqlite")).expect("core");
    let state =
        WebState::new_with_git_storage(core, root.path().join("git")).with_auth(true, true, false);
    state.agent_runs.seed_test_run("ar-local", 8);
    let app = crate::web::app(state, &root.path().join("absent-spa"));
    let upgrade = |origin: Option<&str>| {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("/api/v1/agent-runs/ar-local/attach?control=true")
            .header(header::HOST, "127.0.0.1:8787")
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        let mut request = request.body(Body::empty()).expect("request");
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:50000".parse::<SocketAddr>().expect("peer"),
        ));
        request
    };
    let evil = app
        .clone()
        .oneshot(upgrade(Some("https://evil.example")))
        .await
        .expect("response");
    assert_eq!(evil.status(), StatusCode::FORBIDDEN);
    let null = app
        .clone()
        .oneshot(upgrade(Some("null")))
        .await
        .expect("response");
    assert_eq!(null.status(), StatusCode::FORBIDDEN);
    for origin in [Some("http://127.0.0.1:8787"), None] {
        let response = app
            .clone()
            .oneshot(upgrade(origin))
            .await
            .expect("response");
        assert_ne!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{origin:?} is not refused"
        );
    }
}
