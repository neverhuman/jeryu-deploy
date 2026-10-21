//! Who may write the event kinds the forge believes
//! (`crate::web::pipeline::trust`): the gate verdicts behind a red main, and
//! the incident causes that charge a defect against a change. The logins,
//! repositories and incident ids below are invented.

use super::*;
use crate::web::pipeline::main_health;
use crate::web::pipeline::tests::{body_json, request};
use crate::web::pipeline::{EventsQuery, NewEvent};
use axum::http::Method as HttpMethod;
use tower::ServiceExt;

const REPO: &str = "acme/widgets";
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// A forge with one repository and three identities: an admin, the site's
/// incident tooling (`ci-bot`) and an event reporter that is neither
/// (`review-bot`, see `WebState::configure_test_site`).
fn site() -> (ForgeCore, Arc<WebState>, axum::Router) {
    let core = ForgeCore::new();
    core.create_repository(
        "acme",
        CreateRepositoryRequest {
            name: "widgets".into(),
            ..Default::default()
        },
    )
    .unwrap();
    for (login, role) in [
        ("alice", UserRole::Admin),
        ("ci-bot", UserRole::User),
        ("review-bot", UserRole::User),
    ] {
        core.create_account(login, &format!("{login}-password"), role)
            .unwrap();
    }
    let state = super::super::shared_state(
        WebState::new(core.clone()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let router = super::super::router(state.clone());
    (core, state, router)
}

fn token(core: &ForgeCore, login: &str) -> String {
    core.create_personal_access_token(login, "t", None)
        .unwrap()
        .secret
}

fn red_main(sha: &str) -> Value {
    json!({
        "source": "forge", "kind": "main.status", "repo": REPO, "sha": sha,
        "outcome": "red", "summary": "acme/widgets main is red",
    })
}

/// `main.status` decides whether a repository's default branch is red, and
/// only the gate runner's heartbeat produces one. No posting identity may
/// write it -- not an event reporter, not a global admin -- so an event
/// reporter cannot mark a repository failing.
#[tokio::test]
async fn no_posting_identity_can_mark_a_repository_failing() {
    let (core, state, router) = site();
    for login in ["review-bot", "ci-bot", "alice"] {
        let refused = router
            .clone()
            .oneshot(request(
                HttpMethod::POST,
                "/api/v1/events",
                &token(&core, login),
                Some(red_main(SHA)),
            ))
            .await
            .unwrap();
        assert_eq!(
            refused.status(),
            StatusCode::FORBIDDEN,
            "{login} posted main.status"
        );
        let body = body_json(refused).await;
        assert_eq!(body["code"], "events_kind_reserved");
        assert!(
            body["reason"].as_str().unwrap().contains("main.status"),
            "{body}"
        );
    }
    assert!(
        state
            .events
            .query(&EventsQuery::default())
            .unwrap()
            .is_empty(),
        "a refused batch stores nothing"
    );
    assert!(!main_health::red_now(&state, REPO).unwrap());
    assert!(main_health::red_repos(&state).unwrap().is_empty());
    assert_eq!(repo_list_response(&state).repositories[0].health, "healthy");
}

/// A batch is refused whole: one reserved kind beside honest telemetry stores
/// neither.
#[tokio::test]
async fn a_batch_with_a_reserved_kind_stores_nothing_at_all() {
    let (core, state, router) = site();
    let refused = router
        .oneshot(request(
            HttpMethod::POST,
            "/api/v1/events",
            &token(&core, "ci-bot"),
            Some(json!({"events": [
                {"source": "todoq", "kind": "todo.claimed", "summary": "w1 claimed t1", "todo_id": "t1"},
                red_main(SHA),
            ]})),
        ))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let body = body_json(refused).await;
    assert_eq!(body["code"], "events_kind_reserved");
    assert!(body["reason"].as_str().unwrap().starts_with("events[1]:"));
    assert!(
        state
            .events
            .query(&EventsQuery::default())
            .unwrap()
            .is_empty()
    );
}

/// Telemetry a producer owns keeps working: `gate.log` from the gate runner,
/// and the agent reviewer's own findings.
#[tokio::test]
async fn a_reporter_still_posts_the_kinds_it_owns() {
    let (core, state, router) = site();
    let reporter = token(&core, "ci-bot");
    for event in [
        json!({"source": "pr-gate", "kind": "gate.log", "repo": REPO, "pr": 7,
               "sha": SHA, "outcome": "failure", "summary": "gate failed on acme/widgets#7"}),
        json!({"source": "pr-redteam", "kind": "review.finding", "repo": REPO, "pr": 7,
               "sha": SHA, "event_id": "pr-redteam:1", "summary": "a finding on acme/widgets#7",
               "detail": {"reviewer_kind": "agent", "severity": "medium", "path": "src/lib.rs"}}),
        json!({"source": "pr-redteam", "kind": "pr.skipped", "repo": REPO, "pr": 8,
               "summary": "pr-redteam skipped acme/widgets#8",
               "detail": {"automation": "pr-redteam", "skipped": "draft"}}),
    ] {
        let kind = event["kind"].as_str().unwrap().to_string();
        let posted = router
            .clone()
            .oneshot(request(
                HttpMethod::POST,
                "/api/v1/events",
                &reporter,
                Some(event),
            ))
            .await
            .unwrap();
        assert_eq!(posted.status(), StatusCode::CREATED, "{kind} was refused");
    }
    assert_eq!(
        state.events.query(&EventsQuery::default()).unwrap().len(),
        3
    );
}

/// A `main.status` row stored under another reporter -- a row from before the
/// kind was reserved -- is no verdict: every reader asks for the forge's own
/// rows, and no number of other rows can crowd them out of a page.
#[test]
fn only_the_forges_own_rows_are_read_as_a_gate_verdict() {
    let (_core, state, _router) = site();
    let verdict = |reporter: &str, sha: &str, outcome: &str| {
        state
            .events
            .insert(
                reporter,
                &NewEvent {
                    repo: Some(REPO.to_string()),
                    sha: Some(sha.to_string()),
                    outcome: Some(outcome.to_string()),
                    ..NewEvent::forge("main.status", "a gate verdict on main")
                },
                chrono::Utc::now().timestamp_millis(),
            )
            .unwrap();
    };

    verdict("ci-bot", SHA, "red");
    assert!(
        !main_health::red_now(&state, REPO).unwrap(),
        "a reporter's row is not a verdict"
    );
    assert!(main_health::red_repos(&state).unwrap().is_empty());
    assert!(
        main_health::for_repo(&state, Some(REPO), chrono::Utc::now())
            .unwrap()
            .is_empty(),
        "the projection folds no reporter rows"
    );
    assert_eq!(repo_list_response(&state).repositories[0].health, "healthy");

    // The forge's own verdict is read, and later reporter rows cannot hide it.
    verdict("forge", SHA, "red");
    for _ in 0..5 {
        verdict("review-bot", &"b".repeat(40), "green");
    }
    assert!(main_health::red_now(&state, REPO).unwrap());
    assert_eq!(
        main_health::red_repos(&state).unwrap().len(),
        1,
        "the forge's red verdict stands"
    );
    let health = main_health::for_repo(&state, Some(REPO), chrono::Utc::now()).unwrap();
    assert_eq!(health.len(), 1);
    assert!(health[0].red_now);
    assert_eq!(health[0].gate_runs, 1, "only the forge's run is counted");
    assert_eq!(repo_list_response(&state).repositories[0].health, "failing");
}

fn cause(id: &str) -> Value {
    json!({
        "source": "alert-webhook", "kind": "incident.caused_by", "family": "acme",
        "summary": "checkout is down",
        "detail": {"incident_id": id, "changes": [format!("{REPO}#7")]},
    })
}

fn causes(state: &WebState) -> Vec<String> {
    state
        .events
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT change_key FROM incident_causes ORDER BY change_key")
                .map_err(|err| err.to_string())?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|err| err.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|err| err.to_string())
        })
        .unwrap()
}

/// `incident.caused_by` writes a never-pruned cause row, fails the change for
/// the change outcomes and charges a `defect.linked` against the pull request
/// the key names. An event reporter that is not the site's incident tooling
/// may not blame a change, on either route; what it can still do is report
/// the incident itself.
#[tokio::test]
async fn only_incident_tooling_may_blame_a_change() {
    let (core, state, router) = site();
    let call = |uri: &'static str, login: &str, body: Value| {
        router.clone().oneshot(request(
            HttpMethod::POST,
            uri,
            &token(&core, login),
            Some(body),
        ))
    };

    let refused = call("/api/v1/events", "review-bot", cause("inc-1"))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(refused).await["code"], "incident_blame_reserved");
    // The alert webhook builds the same event from a blaming alert body.
    let refused = call(
        "/api/v1/incidents/webhook",
        "review-bot",
        json!({"incident_id": "inc-2", "status": "firing", "changes": [format!("{REPO}#7")]}),
    )
    .await
    .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(refused).await["code"], "incident_blame_reserved");
    // And so does the "Report incident" form.
    let refused = call(
        "/api/v1/incidents",
        "review-bot",
        json!({"incident_id": "inc-3", "summary": "checkout is down",
               "changes": [format!("{REPO}#7")]}),
    )
    .await
    .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert!(
        causes(&state).is_empty(),
        "a refused blame writes no cause row"
    );
    assert!(
        state
            .events
            .query(&EventsQuery::default())
            .unwrap()
            .is_empty(),
        "a refused incident stores no event at all"
    );

    // The same reporter still opens and restores incidents.
    let opened = call(
        "/api/v1/incidents/webhook",
        "review-bot",
        json!({"incident_id": "inc-4", "status": "firing", "service": "checkout"}),
    )
    .await
    .unwrap();
    assert_eq!(opened.status(), StatusCode::CREATED);

    // The incident tooling blames the change, and the ledger records it.
    let blamed = call("/api/v1/events", "ci-bot", cause("inc-4"))
        .await
        .unwrap();
    assert_eq!(blamed.status(), StatusCode::CREATED);
    assert_eq!(causes(&state), vec![format!("{REPO}#7")]);
    let defects = state
        .events
        .query(&EventsQuery {
            kind: Some(crate::web::review_catch::DEFECT_KIND.to_string()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(defects.len(), 1, "the blamed change is a linked defect");
    assert_eq!(defects[0].pr, Some(7));
}
