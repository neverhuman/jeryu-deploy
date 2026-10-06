//! Who may record and read deployments through the authenticated web edge.
//!
//! A deployment record is a claim about what an environment runs, so writing
//! one needs global admin AND a `JERYU_DEPLOYERS` identity (default
//! `operator,rel_bot`): the automation admins (ci_bot, review_bot) must not, since
//! ci_bot's token is readable by the code it gates. Reading follows ordinary
//! repository read access. The
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
    /// `rel_bot`: admin and a default deployer.
    admin: String,
    /// `ci_bot`: an admin, as in production, but not a deployer.
    writer: String,
    /// `jeryu-admin`: an admin outside the deployer list.
    unlisted_admin: String,
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
    for (login, role, level) in [
        ("rel-bot", UserRole::Admin, RepoAccessLevel::Write),
        ("ci-bot", UserRole::Admin, RepoAccessLevel::Write),
        ("reader", UserRole::User, RepoAccessLevel::Read),
    ] {
        core.create_account(login, "user-password", role).unwrap();
        core.grant_repo_access("jeryu-admin", login, "alice", "jeryu", level)
            .unwrap();
    }
    let token = |login: &str| {
        core.create_personal_access_token(login, "test", None)
            .unwrap()
            .secret
    };
    let (admin, writer, unlisted_admin, reader) = (
        token("rel-bot"),
        token("ci-bot"),
        token("jeryu-admin"),
        token("reader"),
    );
    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    Forge {
        app,
        admin,
        writer,
        unlisted_admin,
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
async fn only_listed_deployers_record_deployments_and_the_creator_is_the_caller() {
    let forge = forge();
    for prefix in ["", "/api/v3"] {
        let path = format!("{prefix}/repos/alice/jeryu/deployments");
        let deployment = serde_json::json!({"sha": SHA, "environment": "production"});

        for token in [&forge.writer, &forge.unlisted_admin, &forge.reader] {
            let (status, _) =
                call(&forge, token, HttpMethod::POST, &path, deployment.clone()).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "only a listed deployer may record a deployment through {prefix:?}"
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
        assert_eq!(created["creator"]["login"], "rel-bot");
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
            "an admin automation identity must not append a status"
        );
        let (status, appended) =
            call(&forge, &forge.admin, HttpMethod::POST, &statuses, success).await;
        assert_eq!(status, StatusCode::CREATED, "{appended}");
        assert_eq!(appended["creator"]["login"], "rel-bot");
    }

    // Each accepted write became a pipeline event; the refused ones did not.
    let (status, page) = call(
        &forge,
        &forge.admin,
        HttpMethod::GET,
        "/api/v1/events?after_seq=0&kind=deploy.",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let events = page["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "deploy.created",
            "deploy.status",
            "deploy.created",
            "deploy.status"
        ]
    );
    assert_eq!(events[1]["outcome"], "success");
    assert_eq!(events[1]["sha"], SHA);
    assert_eq!(events[1]["repo"], "alice/jeryu");
    assert_eq!(events[1]["actor"], "rel-bot");
    assert_eq!(events[1]["reporter"], "forge");
    assert_eq!(events[1]["needs_human"], false);
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

#[tokio::test]
async fn a_failure_status_carries_its_log_into_the_event_and_its_line_into_environments() {
    let forge = forge();
    let (_, created) = call(
        &forge,
        &forge.admin,
        HttpMethod::POST,
        "/repos/alice/jeryu/deployments",
        serde_json::json!({"sha": SHA, "environment": "production", "payload": {"release": "r1"}}),
    )
    .await;
    let id = created["id"].as_u64().unwrap();
    let why = "switch.sh exited 3: error: health check timed out";
    let log_path = "/home/op/.local/state/jeryu-release/logs/r1-20260920T140800Z.log";
    let tail: Vec<String> = (1..=25).map(|n| format!("line {n}")).collect();
    let (status, appended) = call(
        &forge,
        &forge.admin,
        HttpMethod::POST,
        &format!("/repos/alice/jeryu/deployments/{id}/statuses"),
        serde_json::json!({
            "state": "failure",
            "description": why,
            "log_path": log_path,
            "log_tail": tail.join("\n"),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{appended}");
    assert!(
        appended.get("log_tail").is_none(),
        "the status keeps GitHub's shape"
    );

    let (_, page) = call(
        &forge,
        &forge.admin,
        HttpMethod::GET,
        "/api/v1/events?after_seq=0&kind=deploy.status",
        serde_json::Value::Null,
    )
    .await;
    let event = &page["events"][0];
    assert_eq!(event["reason"], why, "{page}");
    assert_eq!(event["needs_human"], true);
    assert_eq!(event["detail"]["log_path"], log_path);
    let kept = event["detail"]["log_tail"].as_str().unwrap();
    assert_eq!(kept.lines().count(), 20, "{kept}");
    assert!(
        kept.starts_with("line 6\n") && kept.ends_with("line 25"),
        "{kept}"
    );

    let (_, environments) = call(
        &forge,
        &forge.reader,
        HttpMethod::GET,
        "/repos/alice/jeryu/environments",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        environments["environments"][0]["latest"]["status"]["description"], why,
        "{environments}"
    );
}
