//! `POST /api/v1/release-actions` and the routes the release host's poller
//! uses to take and finish one ask. The repository, accounts and release ids
//! below are invented.

use super::*;
use crate::web::release_actions;

const REPO: &str = "acme/forge-deploy";
const STAGED: &str = "prod-20261012T171717Z-f1b1fb4-unsigned";
const NEVER_STAGED: &str = "prod-20261012T181818Z-0123abc-unsigned";

#[tokio::test]
async fn a_real_server_id_round_trips_through_the_release_host_poller() {
    use std::os::unix::fs::PermissionsExt;
    let state = fixture();
    let (status, body) = ask(
        &state,
        authenticated_admin_account("operator"),
        json!({"action": "auto_pin", "repo": REPO}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = body["id"].as_str().unwrap();
    assert!(id.starts_with("ra-") && id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-'));
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let queue = dir.path().join("queue.json");
    let claim = dir.path().join("claimed");
    let ran = dir.path().join("ran");
    std::fs::write(&queue, json!({"actions": [body]}).to_string()).unwrap();
    let curl = bin.join("curl");
    std::fs::write(
        &curl,
        r##"#!/bin/sh
case "$*" in
  *release-actions/poller*) echo '{}' ;;
  *'release-actions?state=pending'*) cat "$RA_TEST_QUEUE" ;;
  */claim*) printf '%s\n' "$*" > "$RA_TEST_CLAIM"; echo '{}' ;;
  */result*) printf 200 ;;
  *) exit 2 ;;
esac
"##,
    )
    .unwrap();
    let systemctl = bin.join("systemctl");
    std::fs::write(
        &systemctl,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$RA_TEST_RAN\"\necho fixture-ran\n",
    )
    .unwrap();
    for script in [&curl, &systemctl] {
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let credential = dir.path().join("credential");
    std::fs::write(&credential, "fixture").unwrap();
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/release/release-action.sh");
    let output = std::process::Command::new("bash")
        .arg(script)
        .arg("poll")
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("JERYU_RELEASE_ENV", dir.path().join("absent.env"))
        .env("JERYU_RELEASE_ACTIONS_STATE", dir.path().join("state"))
        .env("JERYU_DEPLOY_REPO", REPO)
        .env("JERYU_DEPLOY_TOKEN_FILE", credential)
        .env("JERYU_FORGE_URL", "https://forge.invalid")
        .env("RA_TEST_QUEUE", queue)
        .env("RA_TEST_CLAIM", &claim)
        .env("RA_TEST_RAN", &ran)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        std::fs::read_to_string(claim)
            .unwrap()
            .contains(&format!("release-actions/{id}/claim"))
    );
    assert!(
        std::fs::read_to_string(ran)
            .unwrap()
            .contains("start jeryu-auto-pin.service")
    );
}

#[tokio::test]
async fn invented_repositories_and_untrusted_staging_events_are_refused() {
    let state = fixture();
    assert_eq!(
        ask(
            &state,
            authenticated_admin_account("operator"),
            json!({"action": "auto_pin", "repo": "acme/invented"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let check = release_actions::check_in(
        State(state.clone()),
        authenticated_admin_account("operator"),
        Json(serde_json::from_value(json!({"repo": "acme/invented"})).unwrap()),
    )
    .await;
    assert_eq!(check.status(), StatusCode::UNPROCESSABLE_ENTITY);
    crate::web::pipeline::record(
        &state,
        "review-bot",
        crate::web::pipeline::NewEvent {
            repo: Some(REPO.into()),
            detail: Some(json!({"release": NEVER_STAGED})),
            ..crate::web::pipeline::NewEvent::forge("release.staged", "untrusted")
        },
    )
    .unwrap();
    assert_eq!(
        ask(
            &state,
            authenticated_admin_account("operator"),
            deploy(NEVER_STAGED)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn release_action_queries_refuse_unknown_keys_and_values() {
    use axum::extract::FromRequestParts;
    for query in ["state=al", "state=landed", "unexpected=all"] {
        let request = axum::http::Request::builder()
            .uri(format!("/api/v1/release-actions?{query}"))
            .body(())
            .unwrap();
        let (mut parts, _) = request.into_parts();
        let parsed = crate::web::strict_query::StrictQuery::<release_actions::ListQuery>::from_request_parts(&mut parts, &()).await;
        assert!(parsed.is_err(), "{query}");
    }
}

#[tokio::test]
async fn only_deployers_can_advertise_pollers_and_cancel_pending_asks() {
    let state = fixture();
    state
        .gate_runners
        .record(
            serde_json::from_value(json!({
                "runnerId": "invented/release-actions", "host": "invented", "slot": 0,
                "labels": ["automation", "release-actions"], "intervalSeconds": 30
            }))
            .unwrap(),
            "review-bot",
            chrono::Utc::now(),
        )
        .unwrap();
    assert!(!release_actions::ReleaseQueue::snapshot(&state, chrono::Utc::now()).polls(REPO));
    let check_in = |who| {
        release_actions::check_in(
            State(state.clone()),
            who,
            Json(serde_json::from_value(json!({"repo": REPO})).unwrap()),
        )
    };
    assert_eq!(
        check_in(authenticated_admin_account("review-bot"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(!release_actions::ReleaseQueue::snapshot(&state, chrono::Utc::now()).polls(REPO));
    assert_eq!(
        check_in(authenticated_admin_account("rel-bot"))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert!(release_actions::ReleaseQueue::snapshot(&state, chrono::Utc::now()).polls(REPO));
    let (_, body) = ask(
        &state,
        authenticated_admin_account("operator"),
        deploy(STAGED),
    )
    .await;
    let id = body["id"].as_str().unwrap().to_string();
    let cancel = |who| release_actions::cancel(State(state.clone()), who, AxumPath(id.clone()));
    assert_eq!(
        cancel(authenticated_admin_account("review-bot"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        cancel(authenticated_admin_account("operator"))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(pending(&state).await, json!([]));
}

#[tokio::test]
async fn retry_requires_a_reported_give_up_for_the_exact_repository_and_head() {
    let state = fixture();
    let head = "a".repeat(40);
    let body = json!({"action": "auto_pin_retry", "repo": REPO, "head": head});
    assert_eq!(
        ask(
            &state,
            authenticated_admin_account("operator"),
            body.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    crate::web::pipeline::emit(
        &state,
        crate::web::pipeline::NewEvent {
            repo: Some(REPO.into()),
            sha: Some(head),
            needs_human: true,
            ..crate::web::pipeline::NewEvent::forge("pin.bump_failed", "retry needed")
        },
    );
    assert_eq!(
        ask(&state, authenticated_admin_account("operator"), body)
            .await
            .0,
        StatusCode::CREATED
    );
}

/// A forge where auto-stage reported [`STAGED`] for [`REPO`]. `operator` is
/// the test site's deployer (an admin named in `JERYU_DEPLOYERS`).
fn fixture() -> Arc<WebState> {
    let core = ForgeCore::new();
    for name in ["forge-deploy", "other"] {
        core.create_repository(
            "acme",
            jeryu_core::CreateRepositoryRequest {
                name: name.into(),
                ..Default::default()
            },
        )
        .unwrap();
    }
    core.create_account("operator", "fixture credential", UserRole::Admin)
        .unwrap();
    core.create_account("review-bot", "fixture credential", UserRole::Admin)
        .unwrap();
    let state = Arc::new(WebState::new(core));
    crate::web::pipeline::record(
        &state,
        "operator",
        crate::web::pipeline::NewEvent {
            repo: Some(REPO.to_string()),
            detail: Some(json!({ "release": STAGED })),
            ..crate::web::pipeline::NewEvent::forge("release.staged", "staged")
        },
    )
    .unwrap();
    state
}

async fn ask(
    state: &Arc<WebState>,
    who: Extension<AccountSummary>,
    body: Value,
) -> (StatusCode, Value) {
    let response = release_actions::request(
        State(state.clone()),
        who,
        Json(serde_json::from_value(body).expect("action body")),
    )
    .await;
    let status = response.status();
    (status, response_json(response).await)
}

fn deploy(release: &str) -> Value {
    json!({ "action": "deploy", "repo": REPO, "release": release })
}

async fn pending(state: &Arc<WebState>) -> Value {
    let response = release_actions::list(
        State(state.clone()),
        authenticated_admin_account("operator"),
        Ok(crate::web::strict_query::StrictQuery(
            serde_json::from_value(json!({})).expect("list query"),
        )),
    )
    .await;
    response_json(response).await["actions"].clone()
}

#[tokio::test]
async fn a_deployer_asks_and_the_release_host_claims_it_once_then_finishes_it() {
    let state = fixture();
    let (status, body) = ask(
        &state,
        authenticated_admin_account("operator"),
        deploy(STAGED),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "pending");
    assert_eq!(body["requested_by"], "operator");
    let id = body["id"].as_str().expect("id").to_string();

    // A second click while it waits is the same ask.
    let (status, again) = ask(
        &state,
        authenticated_admin_account("operator"),
        deploy(STAGED),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["id"], id.as_str());
    assert_eq!(pending(&state).await.as_array().map(Vec::len), Some(1));

    let claim = |who| release_actions::claim(State(state.clone()), who, AxumPath(id.clone()));
    let first = claim(authenticated_admin_account("rel-bot")).await;
    assert_eq!(first.status(), StatusCode::OK);
    // Claimed, so no second poller can run it, and it is no longer pending.
    let second = claim(authenticated_admin_account("operator")).await;
    assert_eq!(second.status(), StatusCode::CONFLICT);
    assert_eq!(pending(&state).await, json!([]));

    let finished = release_actions::result(
        State(state.clone()),
        authenticated_admin_account("rel-bot"),
        AxumPath(id.clone()),
        Json(
            serde_json::from_value(
                json!({ "ok": true, "message": "live", "log_tail": "switched" }),
            )
            .expect("result body"),
        ),
    )
    .await;
    assert_eq!(finished.status(), StatusCode::OK);
    let finished = response_json(finished).await;
    assert_eq!(finished["state"], "succeeded");
    assert_eq!(finished["claimed_by"], "rel-bot");
}

#[tokio::test]
async fn an_admin_who_is_not_a_deployer_is_forbidden_and_a_plain_user_too() {
    let state = fixture();
    // The gate and reviewer identities are admins whose tokens pull request
    // code can read, so admin alone must not be able to ask for a deploy.
    let (status, body) = ask(
        &state,
        authenticated_admin_account("review-bot"),
        deploy(STAGED),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = ask(&state, authenticated_account("operator"), deploy(STAGED)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Nor read the queue, nor claim from it.
    let list = release_actions::list(
        State(state.clone()),
        authenticated_admin_account("review-bot"),
        Ok(crate::web::strict_query::StrictQuery(
            serde_json::from_value(json!({})).expect("list query"),
        )),
    )
    .await;
    assert_eq!(list.status(), StatusCode::FORBIDDEN);
    let (_, ask_body) = ask(
        &state,
        authenticated_admin_account("operator"),
        deploy(STAGED),
    )
    .await;
    let claim = release_actions::claim(
        State(state.clone()),
        authenticated_admin_account("review-bot"),
        AxumPath(ask_body["id"].as_str().expect("id").to_string()),
    )
    .await;
    assert_eq!(claim.status(), StatusCode::FORBIDDEN);
    assert_eq!(pending(&state).await.as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn only_a_release_the_forge_saw_staged_can_be_asked_for() {
    let state = fixture();
    let (status, body) = ask(
        &state,
        authenticated_admin_account("operator"),
        deploy(NEVER_STAGED),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "not_staged");
    // Staged for another repository is not staged for this one.
    let (status, _) = ask(
        &state,
        authenticated_admin_account("operator"),
        json!({ "action": "deploy", "repo": "acme/other", "release": STAGED }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Anything that is not a release id never reaches the host.
    let (status, _) = ask(
        &state,
        authenticated_admin_account("operator"),
        deploy("prod-1; reboot"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(pending(&state).await, json!([]));
}
