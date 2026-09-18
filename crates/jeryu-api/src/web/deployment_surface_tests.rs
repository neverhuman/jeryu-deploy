//! Who may record and read deployments through the authenticated web edge.
//!
//! A deployment record is a claim about what an environment runs, so writing
//! one needs global admin; reading follows ordinary repository read access. The
//! whole `web` module is `#[cfg(feature = "web")]`-gated, so this compiles to
//! nothing without `--features web`.

use super::*;
use axum::body::Body;
use axum::http::Request;
use jeryu_core::{CreateRepositoryRequest, RepoAccessLevel};
use tower::ServiceExt;

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Forge {
    app: axum::Router,
    admin: String,
    writer: String,
    reader: String,
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
    core.create_account("jeryu-admin", "admin-password", UserRole::Admin)
        .unwrap();
    for (login, level) in [
        ("gatebot", RepoAccessLevel::Write),
        ("reader", RepoAccessLevel::Read),
    ] {
        core.create_account(login, "user-password", UserRole::User)
            .unwrap();
        core.grant_repo_access("jeryu-admin", login, "alice", "jeryu", level)
            .unwrap();
    }
    let token = |login: &str| {
        core.create_personal_access_token(login, "test", None)
            .unwrap()
            .secret
    };
    let (admin, writer, reader) = (token("jeryu-admin"), token("gatebot"), token("reader"));
    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    Forge {
        app,
        admin,
        writer,
        reader,
    }
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

#[tokio::test]
async fn only_admins_record_deployments_and_the_creator_is_the_caller() {
    let forge = forge();
    for prefix in ["", "/api/v3"] {
        let path = format!("{prefix}/repos/alice/jeryu/deployments");
        let deployment = serde_json::json!({"sha": SHA, "environment": "production"});

        for token in [&forge.writer, &forge.reader] {
            let (status, _) =
                call(&forge, token, HttpMethod::POST, &path, deployment.clone()).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "a non-admin must not record a deployment through {prefix:?}"
            );
        }

        // A spoofed actor in the body is replaced by the authenticated principal.
        let (status, created) = call(
            &forge,
            &forge.admin,
            HttpMethod::POST,
            &path,
            serde_json::json!({"sha": SHA, "environment": "production", "actor": "mallory"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["creator"]["login"], "jeryu-admin");
        let id = created["id"].as_u64().unwrap();

        let statuses = format!("{path}/{id}/statuses");
        let success = serde_json::json!({"state": "success"});
        let (status, _) = call(
            &forge,
            &forge.writer,
            HttpMethod::POST,
            &statuses,
            success.clone(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a non-admin must not append a status"
        );
        let (status, appended) =
            call(&forge, &forge.admin, HttpMethod::POST, &statuses, success).await;
        assert_eq!(status, StatusCode::CREATED, "{appended}");
        assert_eq!(appended["creator"]["login"], "jeryu-admin");
    }
}

#[tokio::test]
async fn readers_see_deployments_and_environments() {
    let forge = forge();
    let (_, created) = call(
        &forge,
        &forge.admin,
        HttpMethod::POST,
        "/repos/alice/jeryu/deployments",
        serde_json::json!({"sha": SHA, "environment": "production"}),
    )
    .await;
    let id = created["id"].as_u64().unwrap();
    call(
        &forge,
        &forge.admin,
        HttpMethod::POST,
        &format!("/repos/alice/jeryu/deployments/{id}/statuses"),
        serde_json::json!({"state": "success"}),
    )
    .await;

    for path in [
        "/repos/alice/jeryu/deployments".to_string(),
        format!("/repos/alice/jeryu/deployments/{id}"),
        format!("/repos/alice/jeryu/deployments/{id}/statuses"),
        "/repos/alice/jeryu/environments".to_string(),
    ] {
        let (status, body) = call(
            &forge,
            &forge.reader,
            HttpMethod::GET,
            &path,
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    let (_, environments) = call(
        &forge,
        &forge.reader,
        HttpMethod::GET,
        "/repos/alice/jeryu/environments",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        environments["environments"][0]["current"]["deployment"]["sha"],
        SHA
    );
}
