use super::*;

const EXAMPLE: &str = include_str!("../../../../../docs/release-board.example.json");

struct Board {
    router: axum::Router,
    admin: String,
    gatebot: String,
    mallory: String,
}

fn board_router() -> Board {
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
    Board {
        router,
        admin,
        gatebot,
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
        .oneshot(put(&board.mallory, "veox-ai", EXAMPLE.to_string()))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(response_json(refused).await["code"], "permission_denied");

    let accepted = board
        .router
        .clone()
        .oneshot(put(&board.gatebot, "veox-ai", EXAMPLE.to_string()))
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let accepted = response_json(accepted).await;
    assert_eq!(accepted["family"], "veox-ai");
    assert!(accepted.get("ignored").is_none(), "{accepted}");

    let list = board
        .router
        .clone()
        .oneshot(get(&board.admin, "/api/v1/release-board"))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let list = response_json(list).await;
    assert_eq!(list["boards"][0]["family"], "veox-ai");
    assert_eq!(list["boards"][0]["collector"]["host"], "xbabe0");

    let one = board
        .router
        .clone()
        .oneshot(get(&board.admin, "/api/v1/release-board/veox-ai"))
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
        .oneshot(put(&board.gatebot, "veox-ai", EXAMPLE.to_string()))
        .await
        .unwrap();

    for path in ["/api/v1/release-board", "/api/v1/release-board/veox-ai"] {
        // gatebot may write a board but, like any ordinary account, not read one.
        for token in [&board.gatebot, &board.mallory] {
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
        .oneshot(get(&board.admin, "/api/v1/release-board/jain"))
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    let body = response_json(answer).await;
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn a_malformed_or_mismatched_board_is_refused_with_the_reason() {
    use tower::ServiceExt;
    let board = board_router();

    let garbage = board
        .router
        .clone()
        .oneshot(put(&board.gatebot, "veox-ai", "{\"schema\":1}".to_string()))
        .await
        .unwrap();
    assert_eq!(garbage.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response_json(garbage).await["code"], "invalid_input");

    let mismatched = board
        .router
        .clone()
        .oneshot(put(&board.gatebot, "jain", EXAMPLE.to_string()))
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
        .oneshot(put(&board.gatebot, "veox-ai", huge))
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
