//! The outcome ledger: reverts, rework links that arrive late, changes that
//! span repositories, re-posted events, replay and the admin route. Every
//! name below is invented (families acme and globex, host node-a).

use std::path::Path;

use axum::http::{Request as HttpRequest, StatusCode, header};
use jeryu_core::{ForgeCore, UserRole};
use rusqlite::Connection;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{OBSERVATION_MS, build, outcome_of};
use crate::web::pipeline::{EventStore, NewEvent};
use crate::web::{WebState, app};

const DAY: i64 = 24 * 60 * 60 * 1000;
const START: i64 = 1_760_000_000_000;

fn store() -> (tempfile::TempDir, EventStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = EventStore::open(&dir.path().join("shift.sqlite")).unwrap();
    (dir, store)
}

/// An event under construction.
struct Ev {
    event: NewEvent,
}

fn ev(kind: &str, at_family: &str) -> Ev {
    Ev {
        event: NewEvent {
            source: "todoq".to_string(),
            kind: kind.to_string(),
            summary: kind.to_string(),
            family: Some(at_family.to_string()),
            ..NewEvent::default()
        },
    }
}

impl Ev {
    fn todo(mut self, todo: &str) -> Self {
        self.event.todo_id = Some(todo.to_string());
        self
    }
    fn repo(mut self, repo: &str, pr: Option<i64>) -> Self {
        self.event.repo = Some(repo.to_string());
        self.event.pr = pr;
        self
    }
    fn sha(mut self, sha: &str) -> Self {
        self.event.sha = Some(sha.to_string());
        self
    }
    fn outcome(mut self, outcome: &str) -> Self {
        self.event.outcome = Some(outcome.to_string());
        self
    }
    fn summary(mut self, summary: &str) -> Self {
        self.event.summary = summary.to_string();
        self
    }
    fn id(mut self, id: &str) -> Self {
        self.event.event_id = Some(id.to_string());
        self
    }
    fn detail(mut self, detail: Value) -> Self {
        self.event.detail = Some(detail);
        self
    }
    fn at(self, store: &EventStore, at_ms: i64) {
        store.insert("node-a", &self.event, at_ms).unwrap();
    }
}

fn changes(store: &EventStore) -> Vec<super::project::Change> {
    store.outcome_changes(None, None).unwrap()
}

fn change(store: &EventStore, lot: &str) -> super::project::Change {
    changes(store)
        .into_iter()
        .find(|c| c.lot_id == lot)
        .unwrap_or_else(|| panic!("no change {lot}"))
}

/// A todo filed, done, merged in `repo#pr` at `sha`, and shipped by a deploy.
fn shipped(store: &EventStore, todo: &str, repo: &str, pr: i64, sha: &str, at: i64) {
    ev("todo.filed", "acme").todo(todo).at(store, at);
    ev("todo.attempt_finished", "acme")
        .todo(todo)
        .outcome("done")
        .detail(json!({"agent": "codex", "model": "model-a", "experiment": "arms-1"}))
        .at(store, at + 1000);
    ev("pr.merged", "acme")
        .todo(todo)
        .repo(repo, Some(pr))
        .sha(sha)
        .at(store, at + 2000);
    ev("deploy.status", "acme")
        .repo(repo, None)
        .outcome("success")
        .detail(json!({"release": format!("rel-{pr}")}))
        .at(store, at + 3000);
}

#[test]
fn a_shipped_change_is_pending_then_successful_and_carries_its_arm() {
    let (_dir, store) = store();
    shipped(
        &store,
        "20261001-000001-aaaaaa",
        "acme/widget",
        7,
        "abc1234def",
        START,
    );
    let c = change(&store, "20261001-000001-aaaaaa");
    assert_eq!(c.legs.len(), 1);
    assert_eq!(c.legs[0].release.as_deref(), Some("rel-7"));
    assert_eq!(c.agent.as_deref(), Some("codex"));
    assert_eq!(c.model.as_deref(), Some("model-a"));
    assert_eq!(outcome_of(&c, START + DAY), "pending");
    assert_eq!(
        outcome_of(&c, START + OBSERVATION_MS + 10_000),
        "successful"
    );
}

#[test]
fn a_revert_trailer_or_subject_naming_the_sha_fails_the_original() {
    let (_dir, store) = store();
    shipped(
        &store,
        "20261001-000001-aaaaaa",
        "acme/widget",
        7,
        "abc1234def5678",
        START,
    );
    shipped(
        &store,
        "20261001-000002-bbbbbb",
        "acme/widget",
        8,
        "fff9999aaa0000",
        START + 10_000,
    );
    // A trailer in the merged commit's message.
    ev("pr.merged", "acme")
        .repo("acme/widget", Some(9))
        .sha("1111111222")
        .detail(json!({
            "subject": "Undo the widget change",
            "body": "This reverts commit abc1234def5678."
        }))
        .at(&store, START + DAY);
    // A bare "Revert" subject naming the second sha.
    ev("pr.merged", "acme")
        .repo("acme/widget", Some(10))
        .sha("2222222333")
        .summary("Revert \"Add gadget\" (fff9999)")
        .at(&store, START + 2 * DAY);
    let first = change(&store, "20261001-000001-aaaaaa");
    assert_eq!(first.reverted_ms, Some(START + DAY));
    assert_eq!(first.reverted_by.as_deref(), Some("acme/widget#9"));
    let second = change(&store, "20261001-000002-bbbbbb");
    assert_eq!(second.reverted_ms, Some(START + 2 * DAY));
    assert_eq!(outcome_of(&second, START + 40 * DAY), "failed");
    assert_eq!(
        change(&store, "acme/widget#9").work_kind.as_deref(),
        Some("ops"),
        "a revert is ops work"
    );
    // A merge whose subject only mentions "revert" in passing, or names an
    // unknown sha, reverts nothing.
    ev("pr.merged", "acme")
        .repo("acme/widget", Some(11))
        .summary("Fix the revert button (deadbee1234)")
        .at(&store, START + 3 * DAY);
    assert_eq!(
        changes(&store)
            .iter()
            .filter(|c| c.reverted_ms.is_some())
            .count(),
        2
    );
}

#[test]
fn a_deploy_rollback_fails_the_changes_of_that_release() {
    let (_dir, store) = store();
    shipped(
        &store,
        "20261001-000001-aaaaaa",
        "acme/widget",
        7,
        "abc1234def",
        START,
    );
    shipped(
        &store,
        "20261001-000002-bbbbbb",
        "acme/widget",
        8,
        "fff9999aaa",
        START + 10_000,
    );
    ev("deploy.status", "acme")
        .repo("acme/widget", None)
        .outcome("rollback")
        .detail(json!({"rolled_back": "rel-8"}))
        .at(&store, START + DAY);
    assert_eq!(
        change(&store, "20261001-000001-aaaaaa").rolled_back_ms,
        None
    );
    let second = change(&store, "20261001-000002-bbbbbb");
    assert_eq!(second.rolled_back_ms, Some(START + DAY));
    assert_eq!(outcome_of(&second, START + 20 * DAY), "failed");
}

#[test]
fn a_rework_link_after_the_window_flips_a_successful_change_and_counts_false_done() {
    let (_dir, store) = store();
    let original = "20261001-000001-aaaaaa";
    shipped(&store, original, "acme/widget", 7, "abc1234def", START);
    let late = START + 40 * DAY;
    assert_eq!(outcome_of(&change(&store, original), late), "successful");
    ev("todo.filed", "acme")
        .todo("20261111-000009-cccccc")
        .detail(json!({"reworks": [original]}))
        .at(&store, late);
    let c = change(&store, original);
    assert_eq!(c.reworked_by, vec!["20261111-000009-cccccc".to_string()]);
    assert_eq!(outcome_of(&c, late), "reworked");
    assert_eq!(
        change(&store, "20261111-000009-cccccc")
            .work_kind
            .as_deref(),
        Some("rework")
    );
    let report = build(&changes(&store), 90, late + DAY, None, None, None);
    assert_eq!(report["false_done"], 1);
    let row = report["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["lot_id"] == original)
        .unwrap();
    assert_eq!(row["outcome"], "reworked");
    assert_eq!(row["false_done"], true);
    assert_eq!(row["arm"], "codex:model-a");
}

#[test]
fn a_multi_repo_todo_is_one_change_shipped_when_every_repo_shipped() {
    let (_dir, store) = store();
    let todo = "20261001-000005-eeeeee";
    ev("todo.filed", "globex").todo(todo).at(&store, START);
    for (repo, pr, sha) in [
        ("globex/api", 21, "aaaa1111bb"),
        ("globex/web", 22, "bbbb2222cc"),
    ] {
        ev("pr.merged", "globex")
            .todo(todo)
            .repo(repo, Some(pr))
            .sha(sha)
            .at(&store, START + 1000);
    }
    ev("deploy.status", "globex")
        .repo("globex/api", None)
        .outcome("success")
        .detail(json!({"release": "api-1"}))
        .at(&store, START + 2000);
    let c = change(&store, todo);
    assert_eq!(c.legs.len(), 2, "one row, one leg per repository");
    assert_eq!(
        outcome_of(&c, START + 30 * DAY),
        "pending",
        "web has not shipped"
    );
    ev("deploy.status", "globex")
        .repo("globex/web", None)
        .outcome("success")
        .detail(json!({"release": "web-4"}))
        .at(&store, START + 3000);
    let c = change(&store, todo);
    assert_eq!(outcome_of(&c, START + 30 * DAY), "successful");
    // Either repository finds it; neither duplicates it.
    assert_eq!(
        store
            .outcome_changes(None, Some("globex/web"))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .outcome_changes(None, Some("globex/api"))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.outcome_changes(Some("acme"), None).unwrap().len(), 0);
}

#[test]
fn a_pull_request_lot_is_adopted_by_its_todo_and_keeps_its_facts() {
    let (_dir, store) = store();
    ev("pr.merged", "acme")
        .repo("acme/widget", Some(31))
        .sha("cafe1234ab")
        .at(&store, START);
    assert!(changes(&store).iter().any(|c| c.lot_id == "acme/widget#31"));
    ev("pr.merged", "acme")
        .todo("20261001-000007-ffffff")
        .repo("acme/widget", Some(31))
        .at(&store, START + 1000);
    let all = changes(&store);
    assert_eq!(all.len(), 1, "the pull-request lot folded into the todo");
    assert_eq!(all[0].lot_id, "20261001-000007-ffffff");
    assert_eq!(all[0].legs[0].merged_ms, Some(START), "earliest merge kept");
    assert_eq!(all[0].legs[0].sha.as_deref(), Some("cafe1234ab"));
}

#[test]
fn reposted_events_change_nothing_even_after_the_log_forgets_them() {
    let (_dir, store) = store();
    let post = |at: i64| {
        ev("todo.filed", "acme")
            .id("filed-1")
            .todo("20261111-000009-cccccc")
            .detail(json!({"reworks": ["20261001-000001-aaaaaa"]}))
            .at(&store, at);
        ev("incident.opened", "acme")
            .id("inc-1")
            .todo("20261001-000001-aaaaaa")
            .detail(json!({"incident_id": "INC-7"}))
            .at(&store, at);
    };
    post(START);
    let once = changes(&store);
    post(START + 1000);
    // Beyond the 30-day retention the log has pruned the originals.
    post(START + 45 * DAY);
    assert_eq!(changes(&store).len(), once.len());
    let c = change(&store, "20261001-000001-aaaaaa");
    assert_eq!(c.incidents, vec!["INC-7".to_string()]);
    assert_eq!(c.reworked_by, vec!["20261111-000009-cccccc".to_string()]);
    assert_eq!(
        change(&store, "20261111-000009-cccccc").filed_ms,
        Some(START),
        "the first filing stands"
    );
    assert_eq!(
        outcome_of(&c, START),
        "failed",
        "an incident outranks rework"
    );
}

#[test]
fn the_ledger_replays_from_the_event_log_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shift.sqlite");
    {
        let store = EventStore::open(&path).unwrap();
        shipped(
            &store,
            "20261001-000001-aaaaaa",
            "acme/widget",
            7,
            "abc1234def",
            START,
        );
        ev("todo.filed", "acme")
            .todo("20261001-000002-bbbbbb")
            .detail(json!({"kind": "fix"}))
            .at(&store, START + 5000);
    }
    let before = {
        let store = EventStore::open(&path).unwrap();
        changes(&store)
    };
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "DELETE FROM outcome_ledger; DELETE FROM outcome_ledger_repos;
         DELETE FROM outcome_ledger_shas; DELETE FROM outcome_ledger_events;
         DELETE FROM outcome_ledger_replay;",
    )
    .unwrap();
    drop(conn);
    let store = EventStore::open(&path).unwrap();
    assert_eq!(changes(&store), before);
    assert_eq!(
        change(&store, "20261001-000002-bbbbbb")
            .work_kind
            .as_deref(),
        Some("fix")
    );
}

fn get(uri: &str, token: &str) -> HttpRequest<axum::body::Body> {
    HttpRequest::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap()
}

#[tokio::test]
async fn the_route_is_admin_only_and_filters_by_window_family_and_repo() {
    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    let admin = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let user = core
        .create_personal_access_token("bob", "t", None)
        .unwrap()
        .secret;
    let state = WebState::new_with_git_storage(core, dir.path().to_path_buf())
        .with_auth(true, false, false);
    let now = chrono::Utc::now().timestamp_millis();
    shipped(
        &state.events,
        "20261001-000001-aaaaaa",
        "acme/widget",
        7,
        "abc1234def",
        now - 3 * DAY,
    );
    shipped(
        &state.events,
        "20261001-000002-bbbbbb",
        "acme/gadget",
        8,
        "fff9999aaa",
        now - 60 * DAY,
    );
    let router = app(state, Path::new("/tmp/jeryu-no-spa"));
    let call = |uri: &str, token: &str| router.clone().oneshot(get(uri, token));
    let body = |response: axum::response::Response| async move {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice::<Value>(&bytes).unwrap()
    };

    let refused = call("/api/v1/outcomes", &user).await.unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    for bad in [
        "/api/v1/outcomes?days=14",
        "/api/v1/outcomes?outcome=great",
        "/api/v1/outcomes?nope=1",
    ] {
        let response = call(bad, &admin).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    let week = body(call("/api/v1/outcomes?days=7", &admin).await.unwrap()).await;
    assert_eq!(week["total"], 1);
    assert_eq!(week["changes"][0]["outcome"], "pending");
    assert_eq!(week["changes"][0]["work_kind"], Value::Null);
    let quarter = body(call("/api/v1/outcomes?days=90", &admin).await.unwrap()).await;
    assert_eq!(quarter["total"], 2);
    assert_eq!(quarter["by_outcome"]["successful"], 1);
    let by_repo = body(
        call("/api/v1/outcomes?days=90&repo=acme/gadget", &admin)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(by_repo["total"], 1);
    assert_eq!(by_repo["changes"][0]["repo"], "acme/gadget");
    let none = body(
        call(
            "/api/v1/outcomes?days=90&family=acme&outcome=failed",
            &admin,
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(none["total"], 0);
}
