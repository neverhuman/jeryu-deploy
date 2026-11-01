//! Scoped automation credentials report only with explicit site reporter rights.
use axum::http::{Method, StatusCode};
use jeryu_core::{BotKind, BotReach, ForgeCore, UserRole};
use serde_json::json;
use tower::ServiceExt;

use super::{WebState, app};
use crate::web::pipeline::tests::{body_json, request};

#[tokio::test]
async fn bot_reporting_requires_each_site_permission_without_inheriting_admin() {
    let core = ForgeCore::new();
    let mut tokens = Vec::new();
    for (login, role) in [("ci-bot", UserRole::User), ("owner", UserRole::Admin)] {
        core.create_account(login, "reporting-password", role)
            .unwrap();
        let bot = core
            .enroll_bot(
                login,
                "releasebot",
                "Releasebot",
                BotKind::MUSE,
                BotReach::General { refs: None },
            )
            .unwrap();
        tokens.push(
            core.issue_bot_access_token(login, bot.bot.id, None, None)
                .unwrap()
                .token,
        );
    }
    // Only ci-bot is an event reporter in the test site. Independently, only
    // owner is a runner reporter here; its admin role is stripped by bot auth.
    let router = app(
        WebState::new(core)
            .with_auth(true, false, false)
            .with_reporters(&["owner"], "rel-bot"),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    for (index, token) in tokens.iter().enumerate() {
        for kind in ["tag.cut", "pin.bump_opened"] {
            let response = router
                .clone()
                .oneshot(request(
                    Method::POST,
                    "/api/v1/events",
                    token,
                    Some(json!({
                        "source": "auto-tag", "kind": kind, "summary": "auto-tag reported a pin",
                        "repo": "jeryu/jeryu-deploy", "actor": "jeryu-auto-tag"
                    })),
                ))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if index == 0 {
                    StatusCode::CREATED
                } else {
                    StatusCode::FORBIDDEN
                }
            );
            if index == 1 {
                assert_eq!(
                    body_json(response).await["code"],
                    "events_reporter_required"
                );
            }
        }
        let response = router
            .clone()
            .oneshot(request(
                Method::POST,
                "/api/v1/runners/heartbeat",
                token,
                Some(json!({
                    "runnerId": "node-0/auto-tag", "host": "node-0", "slot": 0,
                    "labels": ["automation"], "intervalSeconds": 300
                })),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if index == 1 {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            }
        );
        let body = body_json(response).await;
        if index == 1 {
            assert_eq!(body["accepted"], true);
        } else {
            assert_eq!(body["code"], "permission_denied");
        }
        for uri in [
            "/api/v1/events",
            "/api/v1/runners",
            "/api/v1/user",
            "/api/v1/flow",
        ] {
            let response = router
                .clone()
                .oneshot(request(Method::GET, uri, token, None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
            assert_eq!(body_json(response).await["code"], "insufficient_scope");
        }
    }
}
