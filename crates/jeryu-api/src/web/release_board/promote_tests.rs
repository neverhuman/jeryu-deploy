//! Promote on the release board, with a fixture poller standing in for the
//! release host. Repository, accounts and release ids are invented.

use super::*;
use crate::web::pipeline::EventsQuery;
use crate::web::release_actions as actions;
use jeryu_core::{AccountStatus, ForgeCore};

const REPO: &str = "acme/forge-deploy";
const STAGED: &str = "prod-20261012T171717Z-f1b1fb4-unsigned";
const UNSTAGED: &str = "prod-20261012T181818Z-0123abc-unsigned";

fn account(login: &str, role: UserRole) -> Extension<AccountSummary> {
    Extension(AccountSummary {
        login: login.to_string(),
        display_name: login.to_string(),
        role,
        status: AccountStatus::Active,
        auth_epoch: 0,
        must_change_password: false,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    })
}

fn operator() -> Extension<AccountSummary> {
    account("operator", UserRole::Admin)
}

/// A forge that saw `STAGED` staged, holding the example board whose first
/// lane's last stage promotes `release`.
fn fixture(release: &str) -> Arc<WebState> {
    let core = ForgeCore::new();
    core.create_repository(
        "acme",
        jeryu_core::CreateRepositoryRequest {
            name: "forge-deploy".into(),
            ..Default::default()
        },
    )
    .unwrap();
    for login in ["operator", "review-bot"] {
        core.create_account(login, "fixture credential", UserRole::Admin)
            .unwrap();
    }
    let state = Arc::new(WebState::new(core));
    let staged = crate::web::pipeline::NewEvent {
        repo: Some(REPO.to_string()),
        detail: Some(json!({ "release": STAGED })),
        ..crate::web::pipeline::NewEvent::forge("release.staged", "staged")
    };
    crate::web::pipeline::record(&state, "operator", staged).unwrap();
    let mut board: ReleaseBoard = serde_json::from_str(include_str!(
        "../../../../../docs/release-board.example.json"
    ))
    .unwrap();
    board.collector.trigger = Trigger::Manual;
    let stage = &mut board.lanes[0].stages[3];
    stage.promote.as_mut().unwrap().deploy = Some(PromoteDeploy {
        repo: REPO.to_string(),
        release: release.to_string(),
    });
    state
        .release_boards
        .put("acme", board, Utc::now() - Duration::minutes(1))
        .unwrap_or_else(|error| panic!("{error}"));
    state
}

async fn body(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn press(state: &Arc<WebState>, who: Extension<AccountSummary>) -> (StatusCode, Value) {
    let lane = state.release_boards.get("acme").unwrap().lanes[0]
        .id
        .clone();
    let stage = state.release_boards.get("acme").unwrap().lanes[0].stages[3]
        .id
        .clone();
    let request = PromoteRequest { lane, stage };
    body(
        promote(
            State(state.clone()),
            who,
            Path("acme".into()),
            Json(request),
        )
        .await,
    )
    .await
}

fn run(state: &Arc<WebState>) -> Value {
    let board = state.release_boards.get("acme").unwrap();
    with_runs(state, board)
        .pointer("/lanes/0/stages/3/promote/run")
        .unwrap()
        .clone()
}

async fn poller(state: &Arc<WebState>) {
    let check = actions::check_in(
        State(state.clone()),
        operator(),
        Json(serde_json::from_value(json!({ "repo": REPO })).unwrap()),
    )
    .await;
    assert_eq!(check.status(), StatusCode::NO_CONTENT);
}

/// What the release host does with an ask: claim it, then report the result.
async fn host_finishes(state: &Arc<WebState>, id: &str, ok: bool) {
    poller(state).await;
    let claimed = actions::claim(State(state.clone()), operator(), Path(id.to_string())).await;
    assert_eq!(claimed.status(), StatusCode::OK);
    let result = serde_json::from_value(json!({
        "ok": ok, "message": "fixture host", "log_tail": "switching\nfixture log line"
    }))
    .unwrap();
    let done = actions::result(
        State(state.clone()),
        operator(),
        Path(id.to_string()),
        Json(result),
    )
    .await;
    assert_eq!(done.status(), StatusCode::OK);
}

fn promote_events(state: &Arc<WebState>) -> Vec<crate::web::pipeline::Event> {
    state
        .events
        .query(&EventsQuery {
            kind: Some("release.promote_requested".into()),
            ..EventsQuery::default()
        })
        .unwrap()
}

#[tokio::test]
async fn without_a_live_poller_promote_is_disabled_with_the_reason_and_queues_nothing() {
    let state = fixture(STAGED);
    let run = run(&state);
    assert_eq!(run["available"], false);
    assert!(
        run["reason"]
            .as_str()
            .unwrap()
            .contains("No release poller")
    );
    let (status, body) = press(&state, operator()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "promote_disabled");
    assert!(state.release_actions.all().is_empty());
    assert!(promote_events(&state).is_empty());
}

#[tokio::test]
async fn a_release_the_forge_never_saw_staged_is_disabled_even_with_a_poller() {
    let state = fixture(UNSTAGED);
    poller(&state).await;
    let run = run(&state);
    assert_eq!(run["available"], false);
    assert!(run["reason"].as_str().unwrap().contains("no record"));
    assert_eq!(press(&state, operator()).await.0, StatusCode::CONFLICT);
    assert!(state.release_actions.all().is_empty());
}

#[tokio::test]
async fn only_a_deployer_can_press_promote_or_read_its_log() {
    let state = fixture(STAGED);
    poller(&state).await;
    for who in [
        account("review-bot", UserRole::Admin),
        account("mallory", UserRole::User),
    ] {
        assert_eq!(press(&state, who.clone()).await.0, StatusCode::FORBIDDEN);
        let log = promote_action(
            State(state.clone()),
            who,
            Path(("acme".into(), "ra-1".into())),
        )
        .await;
        assert_eq!(log.status(), StatusCode::FORBIDDEN);
    }
    assert!(state.release_actions.all().is_empty());
    assert!(promote_events(&state).is_empty());
}

#[tokio::test]
async fn promote_queues_one_deploy_names_who_pressed_and_a_second_press_is_the_first() {
    let state = fixture(STAGED);
    poller(&state).await;
    assert_eq!(run(&state)["available"], true);
    let confirm = run(&state)["confirm"].as_str().unwrap().to_string();
    assert!(
        confirm.contains(STAGED) && confirm.contains(REPO),
        "{confirm}"
    );

    let (status, first) = press(&state, operator()).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["created"], true);
    let (status, second) = press(&state, operator()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["created"], false);
    assert_eq!(first["action"]["id"], second["action"]["id"]);

    let queued = state.release_actions.all();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].release.as_deref(), Some(STAGED));
    assert_eq!(queued[0].requested_by, "operator");
    let events = promote_events(&state);
    assert_eq!(events.len(), 1, "the second press adds no audit row");
    assert_eq!(events[0].actor.as_deref(), Some("operator"));
    assert_eq!(events[0].detail.as_ref().unwrap()["release"], STAGED);

    // The cell now shows the ask and offers no second button.
    let run = run(&state);
    assert_eq!(run["available"], false);
    assert_eq!(run["action"]["state"], "pending");
    assert_eq!(run["action"]["requested_by"], "operator");
}

#[tokio::test]
async fn the_cell_follows_the_ask_to_success_with_a_log_and_never_redeploys() {
    let state = fixture(STAGED);
    poller(&state).await;
    let (_, first) = press(&state, operator()).await;
    let id = first["action"]["id"].as_str().unwrap().to_string();
    host_finishes(&state, &id, true).await;

    let run = run(&state);
    assert_eq!(run["action"]["state"], "succeeded");
    assert_eq!(run["available"], false);
    let log = body(
        promote_action(
            State(state.clone()),
            operator(),
            Path(("acme".into(), id.clone())),
        )
        .await,
    )
    .await;
    assert_eq!(log.0, StatusCode::OK);
    assert!(
        log.1["log_tail"]
            .as_str()
            .unwrap()
            .contains("fixture log line")
    );
    assert_eq!(
        run["action"]["log_href"],
        format!("/api/v1/release-board/acme/promote/{id}")
    );

    let (status, again) = press(&state, operator()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["created"], false);
    assert_eq!(state.release_actions.all().len(), 1);
}

#[tokio::test]
async fn a_failed_deploy_can_be_promoted_again() {
    let state = fixture(STAGED);
    poller(&state).await;
    let (_, first) = press(&state, operator()).await;
    host_finishes(&state, first["action"]["id"].as_str().unwrap(), false).await;
    assert_eq!(run(&state)["action"]["state"], "failed");
    assert_eq!(run(&state)["available"], true);
    let (status, retry) = press(&state, operator()).await;
    assert_eq!(status, StatusCode::CREATED, "{retry}");
    assert_ne!(first["action"]["id"], retry["action"]["id"]);
    assert_eq!(promote_events(&state).len(), 2);
}

#[tokio::test]
async fn the_inbox_deploy_button_and_promote_share_one_open_ask() {
    let state = fixture(STAGED);
    poller(&state).await;
    let inbox = actions::request(
        State(state.clone()),
        operator(),
        Json(
            serde_json::from_value(json!({"action": "deploy", "repo": REPO, "release": STAGED}))
                .unwrap(),
        ),
    )
    .await;
    assert_eq!(inbox.status(), StatusCode::CREATED);
    let (status, pressed) = press(&state, operator()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pressed["created"], false);
    assert_eq!(state.release_actions.all().len(), 1);
}

#[tokio::test]
async fn the_request_cannot_name_a_release_or_an_unknown_stage() {
    assert!(
        serde_json::from_value::<PromoteRequest>(
            json!({"lane": "l", "stage": "s", "release": STAGED})
        )
        .is_err()
    );
    let state = fixture(STAGED);
    poller(&state).await;
    let request = PromoteRequest {
        lane: "nope".into(),
        stage: "nope".into(),
    };
    let (status, _) = body(
        promote(
            State(state.clone()),
            operator(),
            Path("acme".into()),
            Json(request),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn a_board_naming_a_malformed_deploy_is_refused() {
    let state = fixture(STAGED);
    let mut board = state.release_boards.get("acme").unwrap();
    board.lanes[0].stages[3].promote.as_mut().unwrap().deploy = Some(PromoteDeploy {
        repo: REPO.into(),
        release: "prod-1; reboot".into(),
    });
    let error = validate("acme", &board, Utc::now()).unwrap_err();
    assert!(error.contains("promote.deploy.release"), "{error}");
}
