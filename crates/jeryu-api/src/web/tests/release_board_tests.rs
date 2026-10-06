use super::*;

const EXAMPLE: &str = include_str!("../../../../../docs/release-board.example.json");

struct Board {
    router: axum::Router,
    admin: String,
    ci_bot: String,
    mallory: String,
}

fn board_router() -> Board {
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("ci-bot", "ci-bot-password", UserRole::User)
        .unwrap();
    core.create_account("mallory", "mallory-password", UserRole::User)
        .unwrap();
    let token = |login: &str| {
        core.create_personal_access_token(login, "test", None)
            .unwrap()
            .secret
    };
    let (admin, ci_bot, mallory) = (token("alice"), token("ci-bot"), token("mallory"));
    let router = app(
        WebState::new(core.clone()).with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    Board {
        router,
        admin,
        ci_bot,
        mallory,
    }
}

fn put(token: &str, family: &str, body: String) -> Request<axum::body::Body> {
    Request::builder()
        .method(HttpMethod::PUT)
        .uri(format!("/api/v1/release-board/{family}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .unwrap()
}

fn get(token: &str, path: &str) -> Request<axum::body::Body> {
    Request::builder()
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap()
}

#[tokio::test]
async fn a_reporter_writes_a_board_and_an_admin_reads_it_back() {
    use tower::ServiceExt;
    let board = board_router();

    let refused = board
        .router
        .clone()
        .oneshot(put(&board.mallory, "acme", EXAMPLE.to_string()))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(response_json(refused).await["code"], "permission_denied");

    let accepted = board
        .router
        .clone()
        .oneshot(put(&board.ci_bot, "acme", EXAMPLE.to_string()))
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let accepted = response_json(accepted).await;
    assert_eq!(accepted["family"], "acme");
    assert!(accepted.get("ignored").is_none(), "{accepted}");

    let list = board
        .router
        .clone()
        .oneshot(get(&board.admin, "/api/v1/release-board"))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let list = response_json(list).await;
    assert_eq!(list["boards"][0]["family"], "acme");
    assert_eq!(list["boards"][0]["collector"]["host"], "collector-1");

    let one = board
        .router
        .clone()
        .oneshot(get(&board.admin, "/api/v1/release-board/acme"))
        .await
        .unwrap();
    assert_eq!(one.status(), StatusCode::OK);
    let one = response_json(one).await;
    assert_eq!(one["lanes"][0]["id"], "cloud-app");
    assert_eq!(
        one["lanes"][0]["stages"][3]["forge"]["environment"],
        "production"
    );
    assert!(one["accepted_at"].is_string(), "{one}");
}

#[tokio::test]
async fn reading_a_board_is_for_admins() {
    use tower::ServiceExt;
    let board = board_router();
    board
        .router
        .clone()
        .oneshot(put(&board.ci_bot, "acme", EXAMPLE.to_string()))
        .await
        .unwrap();

    for path in ["/api/v1/release-board", "/api/v1/release-board/acme"] {
        // ci_bot may write a board but, like any ordinary account, not read one.
        for token in [&board.ci_bot, &board.mallory] {
            let answer = board
                .router
                .clone()
                .oneshot(get(token, path))
                .await
                .unwrap();
            assert_eq!(answer.status(), StatusCode::FORBIDDEN, "{path}");
        }
    }
}

#[tokio::test]
async fn a_family_without_a_board_is_not_found() {
    use tower::ServiceExt;
    let board = board_router();
    let answer = board
        .router
        .clone()
        .oneshot(get(&board.admin, "/api/v1/release-board/initech"))
        .await
        .unwrap();
    // The forge hosts no "initech": a typo is answered as one, not as a
    // family that merely has no board yet.
    assert_eq!(answer.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response_json(answer).await;
    assert_eq!(body["code"], "family_unknown");
}

#[tokio::test]
async fn a_malformed_or_mismatched_board_is_refused_with_the_reason() {
    use tower::ServiceExt;
    let board = board_router();

    let garbage = board
        .router
        .clone()
        .oneshot(put(&board.ci_bot, "acme", "{\"schema\":1}".to_string()))
        .await
        .unwrap();
    assert_eq!(garbage.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response_json(garbage).await["code"], "invalid_input");

    let mismatched = board
        .router
        .clone()
        .oneshot(put(&board.ci_bot, "initech", EXAMPLE.to_string()))
        .await
        .unwrap();
    assert_eq!(mismatched.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response_json(mismatched).await;
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("must match the path"),
        "{body}"
    );
}

#[tokio::test]
async fn an_oversized_board_is_refused_before_it_is_parsed() {
    use tower::ServiceExt;
    let board = board_router();
    let huge = format!("{{\"pad\":\"{}\"}}", "x".repeat(600 * 1024));
    let answer = board
        .router
        .clone()
        .oneshot(put(&board.ci_bot, "acme", huge))
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

/// The example board with `runners` set on lane 0, stage 1, target 0.
fn with_runners(runners: serde_json::Value) -> String {
    let mut example: serde_json::Value = serde_json::from_str(EXAMPLE).unwrap();
    example["lanes"][0]["stages"][1]["targets"][0]["runners"] = runners;
    example.to_string()
}

#[tokio::test]
async fn target_runners_round_trip_and_an_empty_list_is_omitted() {
    use tower::ServiceExt;
    let board = board_router();

    let accepted = board
        .router
        .clone()
        .oneshot(put(
            &board.ci_bot,
            "acme",
            with_runners(serde_json::json!(["gate-a/slot0", "gate-a/slot1"])),
        ))
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let one = response_json(
        board
            .router
            .clone()
            .oneshot(get(&board.admin, "/api/v1/release-board/acme"))
            .await
            .unwrap(),
    )
    .await;
    let stage = &one["lanes"][0]["stages"][1];
    assert_eq!(
        stage["targets"][0]["runners"],
        serde_json::json!(["gate-a/slot0", "gate-a/slot1"])
    );
    assert!(stage["targets"][1].get("runners").is_none(), "{stage}");

    let empty = board
        .router
        .clone()
        .oneshot(put(
            &board.ci_bot,
            "acme",
            with_runners(serde_json::json!([])),
        ))
        .await
        .unwrap();
    assert_eq!(empty.status(), StatusCode::OK);
    let one = response_json(
        board
            .router
            .clone()
            .oneshot(get(&board.admin, "/api/v1/release-board/acme"))
            .await
            .unwrap(),
    )
    .await;
    let target = &one["lanes"][0]["stages"][1]["targets"][0];
    assert!(target.get("runners").is_none(), "{target}");
}

#[tokio::test]
async fn bad_target_runners_are_refused_with_the_path() {
    use tower::ServiceExt;
    let board = board_router();
    let too_many: Vec<String> = (0..65).map(|n| format!("gate-a/slot{n}")).collect();
    let cases = [
        (
            serde_json::json!(too_many),
            "lanes[0].stages[1].targets[0].runners has 65 entries",
        ),
        (
            serde_json::json!(["gate-a/slot0", "gate-a/\u{7}"]),
            "lanes[0].stages[1].targets[0].runners[1]",
        ),
        (
            serde_json::json!(["gate-a/slot0", "gate-b/slot0", "gate-a/slot0"]),
            "lanes[0].stages[1].targets[0].runners[2] \"gate-a/slot0\" is listed twice",
        ),
        (
            serde_json::json!([""]),
            "lanes[0].stages[1].targets[0].runners[0] is 0 characters",
        ),
        (
            serde_json::json!(["x".repeat(201)]),
            "lanes[0].stages[1].targets[0].runners[0] is 201 characters",
        ),
    ];
    for (runners, expected) in cases {
        let answer = board
            .router
            .clone()
            .oneshot(put(&board.ci_bot, "acme", with_runners(runners)))
            .await
            .unwrap();
        assert_eq!(
            answer.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{expected}"
        );
        let body = response_json(answer).await;
        assert_eq!(body["code"], "invalid_input");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains(expected),
            "{expected}: {body}"
        );
    }
}
