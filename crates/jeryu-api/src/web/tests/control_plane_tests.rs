use super::*;

#[tokio::test]
async fn control_plane_status_priorities_and_absence_states_are_live() {
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
    core.create_pull_request(
        "alice",
        "jeryu",
        "bob",
        CreatePullRequestRequest {
            title: "feature".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("head-without-checks".to_string()),
            draft: true,
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "ci/fast".to_string(),
            head_sha: "other-head".to_string(),
            status: Some(jeryu_core::CheckRunStatus::Completed),
            conclusion: Some(CheckConclusion::Failure),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new(core));

    let status = crate::web::control_plane::status(State(state.clone()), Query(Default::default()))
        .await
        .unwrap()
        .0
        .snapshot;
    assert_eq!(status.schema_version, "jeryu.control_plane/v1");
    assert_eq!(status.summary.repo_count, 1);
    assert_eq!(status.summary.draft_pr_count, 1);
    assert_eq!(status.pull_requests.len(), 1);
    assert_eq!(status.pull_requests[0].author, "bob");
    assert_eq!(
        serde_json::to_value(&status.pull_requests[0]).unwrap()["author"],
        "bob"
    );
    // The only failure is on a commit no open PR points at: history, not work.
    assert_eq!(status.summary.failing_check_count, 0);
    assert_eq!(
        serde_json::to_value(&status.summary).unwrap()["mirrorState"],
        "missing"
    );
    assert!(
        status
            .priorities
            .iter()
            .any(|priority| priority.id.contains("checks-missing"))
    );
    assert!(!status.artifacts.absence_is_success);

    let priorities = crate::web::control_plane::priorities(
        State(state.clone()),
        Query(crate::web::control_plane::PriorityQuery { limit: Some(1) }),
    )
    .await
    .0;
    assert_eq!(priorities.len(), 1);
    assert_eq!(priorities[0].rules_version, "rules-v1");

    let artifacts = crate::web::control_plane::artifacts_latest(State(state.clone()))
        .await
        .0;
    assert_eq!(
        artifacts.state,
        crate::web::control_plane::EvidenceState::Missing
    );
    assert!(!artifacts.absence_is_success);

    let runners = crate::web::control_plane::runners(State(state.clone()))
        .await
        .0;
    assert_eq!(
        runners.local.state,
        crate::web::control_plane::EvidenceState::Unknown
    );
    assert_eq!(
        runners.mirror.state,
        crate::web::control_plane::EvidenceState::Missing
    );

    let graph = crate::web::control_plane::repo_graph(
        State(state),
        Query(crate::web::control_plane::RepoGraphQuery {
            repo: None,
            include: None,
            cluster_kind: Some("ci_blocker".to_string()),
            query: None,
            limit: None,
        }),
    )
    .await
    .0;
    assert_eq!(graph.schema_version, "jeryu.repo_graph/v2");
    assert!(
        graph
            .clusters
            .iter()
            .all(|cluster| cluster.kind == "ci_blocker")
    );
}

#[tokio::test]
async fn control_plane_agent_runs_list_route_starts_empty() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let runs = crate::web::agent_runs::list(State(state)).await.0;
    assert!(runs.is_empty());
}

/// The live `/api/v1/ci/runs/{id}/evidence` route returns derived evidence
/// for a real run and a structured 404 for an unknown run id.
#[tokio::test]
async fn ci_run_evidence_route_serves_evidence_and_404() {
    use axum::body::Body;
    use axum::http::Request;
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
    let run = core
        .create_check_run(
            "alice",
            "jeryu",
            CreateCheckRunRequest {
                name: "ci".to_string(),
                head_sha: "deadbeef".to_string(),
                status: Some(jeryu_core::CheckRunStatus::Completed),
                conclusion: Some(CheckConclusion::Success),
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap();
    let router = || {
        app(
            WebState::new(core.clone()),
            std::path::Path::new("/tmp/jeryu-no-spa"),
        )
    };

    let ok = router()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/ci/runs/{}/evidence", run.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let parsed = response_json(ok).await;
    let items = parsed.as_array().expect("evidence array");
    assert!(!items.is_empty(), "a completed run yields evidence");
    for item in items {
        assert!(
            item["uri"]
                .as_str()
                .unwrap()
                .starts_with(&format!("jeryu://ci/run/{}/", run.id))
        );
        assert!(item["digest"].as_str().unwrap().starts_with("sha256:"));
        assert!(item.get("capturedAt").is_some());
    }

    // An unknown run id is a structured 404, not a silent empty list.
    let missing = router()
        .oneshot(
            Request::builder()
                .uri("/api/v1/ci/runs/00000000-0000-0000-0000-000000000000/evidence")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let err = response_json(missing).await;
    assert_eq!(err["code"], "not_found");
    assert_eq!(
        err["purpose"], "retrieve evidence for one live CI run",
        "repairable failures must carry typed guidance"
    );
    for key in ["reason", "common_fixes", "docs_url", "repair_hint"] {
        assert!(err.get(key).is_some(), "missing repair field: {key}");
    }
}

/// A CI run UUID is not authority on its own: the evidence route must only
/// serve runs from repositories the authenticated account can read, and must
/// stop serving them the moment that grant is revoked.
#[tokio::test]
async fn ci_run_evidence_enforces_repo_grants_and_revocation() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    let [admin_token, reader_token, outsider_token] = [
        ("evidence-admin", UserRole::Admin),
        ("evidence-reader", UserRole::User),
        ("evidence-outsider", UserRole::User),
    ]
    .map(|(login, role)| {
        core.create_account(login, "ci-evidence-test-password", role)
            .unwrap();
        core.create_personal_access_token(login, "ci evidence route test", None)
            .unwrap()
            .secret
    });
    let runs = ["private-a", "private-b"].map(|repo| {
        core.create_repository(
            "alice",
            CreateRepositoryRequest {
                name: repo.to_string(),
                private: true,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
        core.create_check_run(
            "alice",
            repo,
            CreateCheckRunRequest {
                name: format!("restricted-check-{repo}"),
                head_sha: if repo == "private-a" {
                    "a".repeat(40)
                } else {
                    "b".repeat(40)
                },
                status: Some(jeryu_core::CheckRunStatus::Completed),
                conclusion: Some(CheckConclusion::Failure),
                output: Some(jeryu_core::CheckRunOutput {
                    title: format!("restricted-title-{repo}"),
                    summary: format!("restricted-summary-{repo}"),
                    text: None,
                }),
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap()
    });
    core.grant_repo_access(
        "evidence-admin",
        "evidence-reader",
        "alice",
        "private-a",
        RepoAccessLevel::Read,
    )
    .unwrap();
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let request = |id: &str, token: Option<&str>| {
        let mut builder = Request::builder().uri(format!("/api/v1/ci/runs/{id}/evidence"));
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(Body::empty()).unwrap()
    };
    let run_ids = runs.each_ref().map(|run| run.id.to_string());
    let absent_id = uuid::Uuid::nil().to_string();

    // The normal auth gate is exercised, with no development bypass.
    for id in &run_ids {
        let response = router.clone().oneshot(request(id, None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response_json(response).await["code"], "unauthorized");
    }

    // A read grant is sufficient, and the administrator needs no explicit grant.
    for (run, token) in [
        (&runs[0], reader_token.as_str()),
        (&runs[0], admin_token.as_str()),
        (&runs[1], admin_token.as_str()),
    ] {
        let response = router
            .clone()
            .oneshot(request(&run.id.to_string(), Some(token)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        let facets = body.as_array().expect("authorized evidence array");
        assert_eq!(facets.len(), 4);
        assert_eq!(facets[0]["payload"]["name"], run.name);
        assert_eq!(facets[0]["payload"]["repo"], format!("alice/{}", run.repo));
        assert_eq!(facets[1]["payload"]["headSha"], run.head_sha);
        assert_eq!(
            facets[3]["payload"]["summary"],
            run.output.as_ref().unwrap().summary
        );
    }

    let missing = router
        .clone()
        .oneshot(request(&absent_id, Some(&reader_token)))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let missing_body = response_json(missing).await;
    assert_eq!(missing_body["code"], "not_found");
    for key in [
        "purpose",
        "reason",
        "common_fixes",
        "docs_url",
        "repair_hint",
    ] {
        assert!(missing_body.get(key).is_some(), "missing guidance: {key}");
    }

    // An unrelated account cannot use a known UUID. A grant to A does not grant B.
    // Missing, malformed, and inaccessible runs have identical response bodies.
    for (id, token) in [
        (run_ids[1].as_str(), reader_token.as_str()),
        (run_ids[0].as_str(), outsider_token.as_str()),
        (run_ids[1].as_str(), outsider_token.as_str()),
        ("not-a-uuid", reader_token.as_str()),
        (absent_id.as_str(), admin_token.as_str()),
    ] {
        let response = router
            .clone()
            .oneshot(request(id, Some(token)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response_json(response).await;
        assert_eq!(body, missing_body);
        let text = body.to_string();
        for run in &runs {
            for hidden in [
                run.id.to_string(),
                run.name.clone(),
                run.repo.clone(),
                run.head_sha.clone(),
                run.output.as_ref().unwrap().title.clone(),
                run.output.as_ref().unwrap().summary.clone(),
            ] {
                assert!(!text.contains(&hidden), "denial leaked a run field");
            }
        }
    }

    // The same router and still-valid token must observe a grant revocation.
    assert!(
        core.revoke_repo_access_checked("evidence-admin", "evidence-reader", "alice", "private-a")
            .unwrap()
    );
    let revoked = router
        .oneshot(request(&run_ids[0], Some(&reader_token)))
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::NOT_FOUND);
    assert_eq!(response_json(revoked).await, missing_body);
}

#[tokio::test]
async fn runner_heartbeats_are_reporter_only_and_reach_the_fleet() {
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("gatebot", "gatebot-password", UserRole::User)
        .unwrap();
    core.create_account("mallory", "mallory-password", UserRole::User)
        .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "test", None)
            .unwrap()
            .secret
    };
    let (admin, gatebot, mallory) = (token("alice"), token("gatebot"), token("mallory"));
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let heartbeat = serde_json::json!({
        "runnerId": "xbabe2/slot0",
        "host": "xbabe2",
        "slot": 0,
        "current": {
            "repo": "veox/jain-web", "pr": 13,
            "sha": "abc30d78ca5eadc15694dd1434d9f8f99c44a0d3",
            "recipe": "just required", "startedAt": "2026-09-17T03:04:00Z"
        }
    });
    let post = |token: &str| {
        Request::builder()
            .method(HttpMethod::POST)
            .uri("/api/v1/runners/heartbeat")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(heartbeat.to_string()))
            .unwrap()
    };

    let refused = router.clone().oneshot(post(&mallory)).await.unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    let accepted = router.clone().oneshot(post(&gatebot)).await.unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    assert_eq!(response_json(accepted).await["runnerId"], "xbabe2/slot0");

    let fleet = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/control-plane/runners")
                .header(header::AUTHORIZATION, format!("Bearer {admin}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fleet.status(), StatusCode::OK);
    let body = response_json(fleet).await;
    assert_eq!(body["local"]["state"], "fresh");
    assert_eq!(body["local"]["nodeDetails"][0]["runnerId"], "xbabe2/slot0");
    assert_eq!(
        body["local"]["nodeDetails"][0]["activeTasks"][0]["repo"],
        "veox/jain-web"
    );
}

/// A runner that says what code it runs gets that code on its node; one that
/// says nothing gets no `code` key; and the response names the forge's own
/// build next to them.
#[tokio::test]
async fn runner_code_and_forge_build_reach_the_fleet() {
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    let admin = core
        .create_personal_access_token("alice", "test", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let post = |beat: serde_json::Value| {
        Request::builder()
            .method(HttpMethod::POST)
            .uri("/api/v1/runners/heartbeat")
            .header(header::AUTHORIZATION, format!("Bearer {admin}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(beat.to_string()))
            .unwrap()
    };
    let with_code = serde_json::json!({
        "runnerId": "gate-a/slot0", "host": "gate-a", "slot": 0,
        "code": {
            "repo": "acme/gate-scripts",
            "commit": "0123456789abcdef0123456789abcdef01234567",
            "version": "gate-scripts-v1.2.0",
            "installedAt": "2026-09-30T12:00:00Z"
        }
    });
    let accepted = router.clone().oneshot(post(with_code)).await.unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let without = serde_json::json!({"runnerId": "gate-a/slot1", "host": "gate-a", "slot": 1});
    let accepted = router.clone().oneshot(post(without)).await.unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let bad = serde_json::json!({
        "runnerId": "gate-a/slot2", "host": "gate-a", "slot": 2,
        "code": {"repo": "acme/gate-scripts", "commit": "NOT-HEX"}
    });
    let refused = router.clone().oneshot(post(bad)).await.unwrap();
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let refusal = response_json(refused).await;
    assert!(
        refusal.to_string().contains("code.commit"),
        "the refusal names the field: {refusal}"
    );

    let fleet = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/control-plane/runners")
                .header(header::AUTHORIZATION, format!("Bearer {admin}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fleet.status(), StatusCode::OK);
    let body = response_json(fleet).await;
    let nodes = body["local"]["nodeDetails"].as_array().unwrap();
    assert_eq!(nodes.len(), 2, "the refused beat painted nothing");
    assert_eq!(
        nodes[0]["code"],
        serde_json::json!({
            "repo": "acme/gate-scripts",
            "commit": "0123456789abcdef0123456789abcdef01234567",
            "version": "gate-scripts-v1.2.0",
            "installedAt": "2026-09-30T12:00:00+00:00"
        })
    );
    assert!(
        nodes[1].get("code").is_none(),
        "no code key for a runner that sent none"
    );
    assert_eq!(body["forge"]["version"], crate::JERYU_API_VERSION);
    assert_eq!(body["forge"]["commit"].as_str(), crate::JERYU_BUILD_COMMIT);
    assert_eq!(body["forge"]["webCommit"].as_str(), crate::JERYU_WEB_COMMIT);
    assert!(body["forge"].as_object().unwrap().contains_key("commit"));
    assert!(body["forge"].as_object().unwrap().contains_key("webCommit"));
}

#[tokio::test]
async fn redteam_heartbeats_from_pragent_reach_the_fleet_as_a_reviewer() {
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("pragent", "pragent-password", UserRole::User)
        .unwrap();
    core.create_account("alton", "alton-password", UserRole::User)
        .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "test", None)
            .unwrap()
            .secret
    };
    let (admin, pragent, alton) = (token("alice"), token("pragent"), token("alton"));
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let heartbeat = serde_json::json!({
        "runnerId": "xbabe0/redteam",
        "host": "xbabe0",
        "slot": 0,
        "labels": ["redteam"],
        "current": {
            "repo": "jeryu/jeryu-web", "pr": 44,
            "sha": "abc30d78ca5eadc15694dd1434d9f8f99c44a0d3",
            "recipe": "redteam-review", "startedAt": "2026-09-19T05:20:00Z"
        },
        "last": {
            "repo": "jeryu/jeryu-deploy", "pr": 43,
            "sha": "55ee4dd0efe046dc716f77fa73536d35b760fe4e",
            "recipe": "redteam-review", "conclusion": "approve",
            "seconds": 22, "finishedAt": "2026-09-19T05:21:43Z"
        }
    });
    let post = |token: &str| {
        Request::builder()
            .method(HttpMethod::POST)
            .uri("/api/v1/runners/heartbeat")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(heartbeat.to_string()))
            .unwrap()
    };

    let refused = router.clone().oneshot(post(&alton)).await.unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(response_json(refused).await["code"], "permission_denied");

    let accepted = router.clone().oneshot(post(&pragent)).await.unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    assert_eq!(response_json(accepted).await["runnerId"], "xbabe0/redteam");

    let fleet = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/control-plane/runners")
                .header(header::AUTHORIZATION, format!("Bearer {admin}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fleet.status(), StatusCode::OK);
    let body = response_json(fleet).await;
    let node = &body["local"]["nodeDetails"][0];
    assert_eq!(node["runnerId"], "xbabe0/redteam");
    assert_eq!(node["source"], "pr-redteam");
    assert_eq!(node["classes"][0], "reviewer");
    assert_eq!(node["capacity"], 0);
    assert_eq!(node["activeTasks"][0]["label"], "jeryu/jeryu-web#44");
    assert_eq!(node["lastActivity"]["conclusion"], "approve");
    assert_eq!(body["local"]["totalSlots"], 0);
}

#[tokio::test]
async fn automation_heartbeats_from_an_admin_reach_the_fleet_without_a_slot() {
    use tower::ServiceExt;

    let core = ForgeCore::new();
    // alton2 is an admin and is not named in JERYU_RUNNER_REPORTERS.
    core.create_account("alton2", "alton2-password", UserRole::Admin)
        .unwrap();
    core.create_account("mallory", "mallory-password", UserRole::User)
        .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "test", None)
            .unwrap()
            .secret
    };
    let (admin, mallory) = (token("alton2"), token("mallory"));
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let beat = |conclusion: &str, interval: u64| {
        serde_json::json!({
            "runnerId": "xbabe0/auto-stage",
            "host": "xbabe0",
            "slot": 0,
            "labels": ["automation"],
            "intervalSeconds": interval,
            "last": {
                "repo": "jeryu/jeryu-deploy",
                "sha": "77dc3310aa5eadc15694dd1434d9f8f99c44a0d3",
                "recipe": "auto-stage", "conclusion": conclusion,
                "seconds": 0, "finishedAt": "2026-09-20T04:10:00Z"
            }
        })
    };
    let post = |token: &str, body: serde_json::Value| {
        Request::builder()
            .method(HttpMethod::POST)
            .uri("/api/v1/runners/heartbeat")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    };

    let refused = router
        .clone()
        .oneshot(post(&mallory, beat("staged", 300)))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(response_json(refused).await["code"], "permission_denied");

    for (body, message) in [
        (
            beat("success", 300),
            "last.conclusion: expected opened, staged, waiting, failed",
        ),
        (beat("staged", 29), "intervalSeconds: expected 30 to 86400"),
        (
            beat("staged", 86_401),
            "intervalSeconds: expected 30 to 86400",
        ),
    ] {
        let invalid = router.clone().oneshot(post(&admin, body)).await.unwrap();
        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = response_json(invalid).await;
        assert_eq!(body["code"], "invalid_input");
        assert_eq!(body["message"], message);
    }

    let accepted = router
        .clone()
        .oneshot(post(&admin, beat("staged", 300)))
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let accepted = response_json(accepted).await;
    assert_eq!(accepted["runnerId"], "xbabe0/auto-stage");
    assert_eq!(accepted["offlineAfterSeconds"], 900);

    let fleet = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/control-plane/runners")
                .header(header::AUTHORIZATION, format!("Bearer {admin}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response_json(fleet).await;
    let node = &body["local"]["nodeDetails"][0];
    assert_eq!(node["runnerId"], "xbabe0/auto-stage");
    assert_eq!(node["source"], "automation");
    assert_eq!(node["classes"][0], "automation");
    assert_eq!(node["state"], "active");
    assert_eq!(node["capacity"], 0);
    assert_eq!(node["offlineAfterSeconds"], 900);
    assert_eq!(node["lastActivity"]["conclusion"], "staged");
    assert!(node["lastActivity"]["pr"].is_null());
    // A timer is not gate capacity: no slot, and the fabric is still unknown.
    assert_eq!(body["local"]["totalSlots"], 0);
    assert_eq!(body["local"]["onlineRunners"], 0);
    assert_eq!(body["local"]["state"], "unknown");
}

#[tokio::test]
async fn gate_identity_publishes_statuses_without_admin_but_cannot_protect() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

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
    // JERYU_CI_PUBLISHERS is unset in tests, so the default gate identity applies.
    core.create_account("gatebot", "gatebot-password", UserRole::User)
        .unwrap();
    core.grant_repo_access(
        "jeryu-admin",
        "gatebot",
        "alice",
        "jeryu",
        RepoAccessLevel::Write,
    )
    .unwrap();
    let gatebot_token = core
        .create_personal_access_token("gatebot", "test", None)
        .unwrap()
        .secret;
    let app = app(
        WebState::new(core).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let request = |method: HttpMethod, path: String, body: String| {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {gatebot_token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    };

    for prefix in ["", "/api/v3"] {
        let status = app
            .clone()
            .oneshot(request(
                HttpMethod::POST,
                format!("{prefix}/repos/alice/jeryu/statuses/deadbeef"),
                serde_json::json!({"state": "pending", "context": "jeryu/required"}).to_string(),
            ))
            .await
            .unwrap();
        assert_ne!(
            status.status(),
            StatusCode::FORBIDDEN,
            "gatebot must be able to post statuses through {prefix}"
        );
        assert!(
            status.status().is_success(),
            "status post: {}",
            status.status()
        );

        let check = app
            .clone()
            .oneshot(request(
                HttpMethod::POST,
                format!("{prefix}/repos/alice/jeryu/check-runs"),
                serde_json::json!({
                    "name": "pr-gate-runner",
                    "head_sha": "deadbeef",
                    "status": "completed",
                    "conclusion": "success"
                })
                .to_string(),
            ))
            .await
            .unwrap();
        assert!(
            check.status().is_success(),
            "check-run post: {}",
            check.status()
        );

        let protection = app
            .clone()
            .oneshot(request(
                HttpMethod::PUT,
                format!("{prefix}/repos/alice/jeryu/branches/main/protection"),
                serde_json::json!({"required_status_checks": []}).to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(
            protection.status(),
            StatusCode::FORBIDDEN,
            "a gate identity must not change branch protection through {prefix}"
        );
    }
}

#[tokio::test]
async fn control_plane_repo_count_excludes_archived_and_reports_them_apart() {
    let core = ForgeCore::new();
    for name in ["live-a", "live-b", "old-a", "old-b", "old-c"] {
        core.create_repository(
            "alice",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }
    for name in ["old-a", "old-b", "old-c"] {
        core.set_repository_archived("alice", "alice", name, true)
            .unwrap();
    }
    let state = Arc::new(WebState::new(core));

    let summary = crate::web::control_plane::status(State(state), Query(Default::default()))
        .await
        .unwrap()
        .0
        .snapshot
        .summary;
    assert_eq!(
        summary.repo_count, 2,
        "active repos, as /api/v1/repos lists"
    );
    assert_eq!(summary.archived_repo_count, 3);
    let json = serde_json::to_value(&summary).unwrap();
    assert_eq!(json["repoCount"], 2);
    assert_eq!(json["archivedRepoCount"], 3);
}
