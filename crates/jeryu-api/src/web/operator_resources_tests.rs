//! `GET /api/v1/{releases,mirrors,settings,audit}` through the full router:
//! rows follow repository read access like `/api/v1/repos`, the collections
//! page, and the audit trail is for global admins only.

use std::path::Path;

use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::{
    CheckRunStatus, CreateCheckRunRequest, CreateRepositoryRequest, ForgeCore, RepoAccessLevel,
    UserRole,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{WebState, app};

async fn get(router: &axum::Router, uri: &str, token: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::GET)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn names(body: &Value, rows: &str) -> Vec<String> {
    body[rows]
        .as_array()
        .unwrap_or_else(|| panic!("{rows} missing: {body}"))
        .iter()
        .map(|row| row["repo"]["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn operator_resources_follow_read_access_page_and_gate_audit() {
    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).unwrap();
    core.create_account("admin", "admin-password", UserRole::Admin)
        .unwrap();
    core.create_account("reader", "reader-password", UserRole::User)
        .unwrap();
    for (name, private) in [("open", false), ("vault", true), ("hidden", true)] {
        core.create_repository(
            "admin",
            CreateRepositoryRequest {
                name: name.to_string(),
                private,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }
    core.grant_repo_access("admin", "reader", "admin", "vault", RepoAccessLevel::Write)
        .unwrap();
    core.grant_repo_access("admin", "reader", "admin", "open", RepoAccessLevel::Read)
        .unwrap();
    core.create_check_run(
        "admin",
        "open",
        CreateCheckRunRequest {
            name: "jeryu/github-mirror".to_string(),
            head_sha: "a".repeat(40),
            status: Some(CheckRunStatus::Completed),
            conclusion: Some(jeryu_core::CheckConclusion::Success),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    core.append_audit_as(
        "admin",
        "repository.update",
        "admin/hidden",
        "completed",
        json!({ "field": "family" }),
    )
    .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, reader) = (token("admin"), token("reader"));
    let router = app(
        WebState::new(core).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );

    // Admins see every repository; a reader sees only what they were granted.
    for rows in ["releases", "mirrors", "settings"] {
        let (status, body) = get(&router, &format!("/api/v1/{rows}"), &admin).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["total"], 3, "{body}");
        let (status, body) = get(&router, &format!("/api/v1/{rows}"), &reader).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let mut seen = names(&body, rows);
        seen.sort();
        assert_eq!(seen, ["open", "vault"], "{rows}: {body}");
        // Shaped like /repos: `?repo=` narrows, paging cuts and reports.
        let (_, one) = get(
            &router,
            &format!("/api/v1/{rows}?repo=admin/vault"),
            &reader,
        )
        .await;
        assert_eq!(names(&one, rows), ["vault"], "{one}");
        let (_, hidden) = get(
            &router,
            &format!("/api/v1/{rows}?repo=admin/hidden"),
            &reader,
        )
        .await;
        assert_eq!(hidden["total"], 0, "{hidden}");
        let (_, paged) = get(&router, &format!("/api/v1/{rows}?limit=1"), &admin).await;
        assert_eq!(paged[rows].as_array().unwrap().len(), 1, "{paged}");
        assert_eq!(paged["page"]["has_more"], true, "{paged}");
        let (status, _) = get(&router, &format!("/api/v1/{rows}?limit=0"), &admin).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    let (_, mirrors) = get(&router, "/api/v1/mirrors?repo=admin/open", &admin).await;
    assert_eq!(
        mirrors["mirrors"][0]["mirror"]["configured"], true,
        "{mirrors}"
    );
    assert_eq!(mirrors["mirrors"][0]["mirror"]["last_attempt_ok"], true);
    let (_, unmirrored) = get(&router, "/api/v1/mirrors?repo=admin/vault", &admin).await;
    assert!(unmirrored["mirrors"][0]["mirror"].is_null(), "{unmirrored}");

    // Nothing was ever pushed or tagged: no release on the default branch.
    // (Tag resolution itself is covered by the release-tag route tests.)
    let (_, releases) = get(&router, "/api/v1/releases?repo=admin/open", &admin).await;
    assert_eq!(releases["releases"][0]["branch"], "main", "{releases}");
    assert!(releases["releases"][0]["tag"].is_null());

    let (_, settings) = get(&router, "/api/v1/settings?repo=admin/vault", &reader).await;
    assert_eq!(
        settings["settings"][0]["default_branch"], "main",
        "{settings}"
    );
    assert_eq!(settings["settings"][0]["visibility"], "private");
    assert_eq!(settings["settings"][0]["can_write"], true);
    assert!(settings["feature_flags"].is_object(), "{settings}");
    assert!(
        settings["permissions"]
            .as_array()
            .unwrap()
            .contains(&json!("settings.read"))
    );
    let (_, open) = get(&router, "/api/v1/settings?repo=admin/open", &reader).await;
    assert_eq!(open["settings"][0]["can_write"], false, "{open}");

    let (status, _) = get(&router, "/api/v1/audit", &reader).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, audit) = get(&router, "/api/v1/audit", &admin).await;
    assert_eq!(status, StatusCode::OK, "{audit}");
    assert_eq!(audit["total"], 1, "{audit}");
    assert_eq!(audit["entries"][0]["action"], "repository.update");
    assert_eq!(audit["entries"][0]["actor"], "admin");
    assert_eq!(audit["entries"][0]["subject"], "admin/hidden");
    let (_, other) = get(&router, "/api/v1/audit?repo=admin/open", &admin).await;
    assert_eq!(other["total"], 0, "{other}");
}
