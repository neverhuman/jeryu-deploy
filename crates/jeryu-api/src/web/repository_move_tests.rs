//! Renaming and transferring a repository through the authenticated web edge.
//!
//! Both are admin-only on the GitHub-compatible edge (`PATCH` with `name`,
//! `POST .../transfer`) and on the typed `PATCH /api/v1/repos/:id`: a
//! repository writer is refused before anything moves. Each accepted move is a
//! `repo.renamed` or `repo.transferred` pipeline event carrying `{from, to}`.

use super::*;
use axum::body::Body;
use axum::http::Request;
use jeryu_core::{CreateOrganizationRequest, CreateRepositoryRequest, RepoAccessLevel};
use tower::ServiceExt;

struct Forge {
    app: axum::Router,
    admin: String,
    writer: String,
}

fn forge() -> Forge {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.create_organization(CreateOrganizationRequest {
        login: "veox".to_string(),
        display_name: None,
    })
    .unwrap();
    core.create_account("jeryu-admin", "admin-password", UserRole::Admin)
        .unwrap();
    core.create_account("writer", "user-password", UserRole::User)
        .unwrap();
    core.grant_repo_access(
        "jeryu-admin",
        "writer",
        "alice",
        "jeryu",
        RepoAccessLevel::Write,
    )
    .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "test", None)
            .unwrap()
            .secret
    };
    let (admin, writer) = (token("jeryu-admin"), token("writer"));
    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    Forge { app, admin, writer }
}

async fn call(
    forge: &Forge,
    token: &str,
    method: HttpMethod,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = forge.app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let parsed = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, parsed)
}

async fn repo_events(forge: &Forge) -> Vec<serde_json::Value> {
    let (status, page) = call(
        forge,
        &forge.admin,
        HttpMethod::GET,
        "/api/v1/events?after_seq=0&kind=repo.",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["events"].as_array().unwrap().clone()
}

#[tokio::test]
async fn a_repository_writer_may_not_rename_or_transfer() {
    let forge = forge();
    for (method, path, body) in [
        (
            HttpMethod::PATCH,
            "/repos/alice/jeryu",
            serde_json::json!({"name": "renamed"}),
        ),
        (
            HttpMethod::POST,
            "/repos/alice/jeryu/transfer",
            serde_json::json!({"new_owner": "veox"}),
        ),
        (
            HttpMethod::PATCH,
            "/api/v1/repos/alice/jeryu",
            serde_json::json!({"name": "renamed"}),
        ),
    ] {
        let (status, body) = call(&forge, &forge.writer, method.clone(), path, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
    }
    let (status, repo) = call(
        &forge,
        &forge.admin,
        HttpMethod::GET,
        "/repos/alice/jeryu",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(repo["full_name"], "alice/jeryu", "nothing moved");
    assert!(
        repo_events(&forge).await.is_empty(),
        "no refused move emits"
    );
}

#[tokio::test]
async fn an_admin_renames_and_transfers_and_each_move_is_an_event() {
    let forge = forge();

    let (status, renamed) = call(
        &forge,
        &forge.admin,
        HttpMethod::PATCH,
        "/repos/alice/jeryu",
        serde_json::json!({"name": "forge"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["full_name"], "alice/forge");

    let (status, transferred) = call(
        &forge,
        &forge.admin,
        HttpMethod::POST,
        "/api/v3/repos/alice/forge/transfer",
        serde_json::json!({"new_owner": "veox"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{transferred}");
    assert_eq!(transferred["full_name"], "veox/forge");

    let (status, typed) = call(
        &forge,
        &forge.admin,
        HttpMethod::PATCH,
        "/api/v1/repos/veox/forge",
        serde_json::json!({"name": "jeryu"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{typed}");

    let (status, both) = call(
        &forge,
        &forge.admin,
        HttpMethod::PATCH,
        "/api/v1/repos/veox/jeryu",
        serde_json::json!({"name": "other", "archived": true}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{both}");

    // The very first slug still resolves to the moved repository.
    let (status, old) = call(
        &forge,
        &forge.admin,
        HttpMethod::GET,
        "/repos/alice/jeryu",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{old}");
    assert_eq!(old["full_name"], "veox/jeryu");

    let events = repo_events(&forge).await;
    let moves: Vec<(&str, &str, &str)> = events
        .iter()
        .map(|event| {
            (
                event["kind"].as_str().unwrap(),
                event["detail"]["from"].as_str().unwrap(),
                event["detail"]["to"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        moves,
        [
            ("repo.renamed", "alice/jeryu", "alice/forge"),
            ("repo.transferred", "alice/forge", "veox/forge"),
            ("repo.renamed", "veox/forge", "veox/jeryu"),
        ]
    );
    assert!(events.iter().all(|event| event["actor"] == "jeryu-admin"));
}
