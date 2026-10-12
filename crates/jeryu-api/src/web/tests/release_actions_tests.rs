//! `POST /api/v1/release-actions` and the routes the release host's poller
//! uses to take and finish one ask. The repository, accounts and release ids
//! below are invented.

use super::*;
use crate::web::release_actions;

const REPO: &str = "acme/forge-deploy";
const STAGED: &str = "prod-20261012T171717Z-f1b1fb4-unsigned";
const NEVER_STAGED: &str = "prod-20261012T181818Z-0123abc-unsigned";

/// A forge where auto-stage reported [`STAGED`] for [`REPO`]. `operator` is
/// the test site's deployer (an admin named in `JERYU_DEPLOYERS`).
fn fixture() -> Arc<WebState> {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    crate::web::pipeline::emit(
        &state,
        crate::web::pipeline::NewEvent {
            repo: Some(REPO.to_string()),
            detail: Some(json!({ "release": STAGED })),
            ..crate::web::pipeline::NewEvent::forge("release.staged", "staged")
        },
    );
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
        Query(serde_json::from_value(json!({})).expect("list query")),
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
        Query(serde_json::from_value(json!({})).expect("list query")),
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
