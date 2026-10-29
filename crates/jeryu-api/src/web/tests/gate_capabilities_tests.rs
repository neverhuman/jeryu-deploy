//! Gate environment advertisements are authenticated, validated and visible to operators.
use super::*;
use tower::ServiceExt;

#[tokio::test]
async fn gate_capabilities_validate_before_replacing_the_last_heartbeat() {
    let core = ForgeCore::new();
    for (login, role) in [("operator", UserRole::Admin), ("outsider", UserRole::User)] {
        core.create_account(login, "fixture-password", role)
            .unwrap();
    }
    let token = |login| {
        core.create_personal_access_token(login, "fixture", None)
            .unwrap()
            .secret
    };
    let operator = token("operator");
    let outsider = token("outsider");
    let router = app(
        WebState::new(core).with_auth(true, false, false),
        Path::new("/nonexistent-spa"),
    );
    let post = |token: &str, capabilities: Option<Value>| {
        let mut beat = json!({"runnerId":"gate-a/slot0", "host":"gate-a", "slot":0});
        if let Some(capabilities) = capabilities {
            beat["capabilities"] = capabilities;
        }
        Request::builder()
            .method(HttpMethod::POST)
            .uri("/api/v1/runners/heartbeat")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(beat.to_string()))
            .unwrap()
    };
    let fleet = || {
        Request::builder()
            .uri("/api/v1/control-plane/runners")
            .header(header::AUTHORIZATION, format!("Bearer {operator}"))
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let response = router
        .clone()
        .oneshot(post(&outsider, Some(json!(["host-ci"]))))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    for caps in [None, Some(json!([])), Some(json!(["host-ci"]))] {
        let expected = caps.clone().unwrap_or(json!([]));
        let response = router.clone().oneshot(post(&operator, caps)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(router.clone().oneshot(fleet()).await.unwrap()).await;
        assert_eq!(body["local"]["nodeDetails"][0]["capabilities"], expected);
    }
    for (caps, reason) in [
        (json!(["host_ci"]), "unknown capability"),
        (json!(["host-ci", "host-ci"]), "duplicate capability"),
    ] {
        let response = router
            .clone()
            .oneshot(post(&operator, Some(caps)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = response_json(response).await;
        assert!(body["message"].as_str().unwrap().contains(reason), "{body}");
    }
    let body = response_json(router.clone().oneshot(fleet()).await.unwrap()).await;
    assert_eq!(
        body["local"]["nodeDetails"][0]["capabilities"],
        json!(["host-ci"])
    );
    // Removing the operator setting removes the advertisement on the next accepted beat.
    let response = router.clone().oneshot(post(&operator, None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(router.oneshot(fleet()).await.unwrap()).await;
    assert_eq!(body["local"]["nodeDetails"][0]["capabilities"], json!([]));
}
