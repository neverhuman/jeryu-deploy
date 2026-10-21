//! Taking a todo on, through the full router with auth on: a person with write
//! access to the family's code claims, hands over and finishes a todo; another
//! user, an account with no grant (one that merely signed up), a reporter
//! login and a non-admin asking for an admin action are forbidden.

use std::path::Path;

use axum::http::{Method as HttpMethod, StatusCode};
use jeryu_core::{CreateRepositoryRequest, ForgeCore, RepoAccessLevel, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::tests::{body_json, fixture, request};
use crate::web::{WebState, app};

fn people_path(id: &str) -> String {
    format!("/api/v1/shift/todos/jeryu/{id}/people")
}

struct Site {
    router: axum::Router,
    admin: String,
    ada: String,
    bea: String,
    /// Signed up, no grant on any family repository.
    carl: String,
    reporter: String,
    operator: String,
}

fn site(dir: &Path) -> Site {
    fixture(dir);
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    for login in ["ada", "bea", "carl", "ci-bot", "operator"] {
        let role = if login == "operator" {
            UserRole::Admin
        } else {
            UserRole::User
        };
        core.create_account(login, &format!("{login}-password"), role)
            .unwrap();
    }
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "jeryu-deploy".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    // ada and bea work on the family's code; carl and ci-bot have no grant.
    for login in ["ada", "bea"] {
        core.grant_repo_access(
            "alice",
            login,
            "jeryu",
            "jeryu-deploy",
            RepoAccessLevel::Write,
        )
        .unwrap();
    }
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, ada, bea, carl, reporter, operator) = (
        token("alice"),
        token("ada"),
        token("bea"),
        token("carl"),
        token("ci-bot"),
        token("operator"),
    );
    let router = app(
        WebState::new_with_git_storage(core, dir.to_path_buf()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    Site {
        router,
        admin,
        ada,
        bea,
        carl,
        reporter,
        operator,
    }
}

impl Site {
    async fn call(
        &self,
        method: HttpMethod,
        uri: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let response = self
            .router
            .clone()
            .oneshot(request(method, uri, token, body))
            .await
            .unwrap();
        (response.status(), body_json(response).await)
    }

    async fn act(&self, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
        self.call(HttpMethod::POST, uri, token, Some(body)).await
    }

    /// File a todo and block it for a person, the state people take work on.
    async fn blocked_todo(&self) -> String {
        let (status, filed) = self
            .act(
                "/api/v1/shift/todos",
                &self.admin,
                json!({"family": "jeryu", "text": "Rotate the signing key", "mode": "now"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{filed}");
        let id = filed["id"].as_str().unwrap().to_string();
        let (status, blocked) = self
            .act(
                &format!("/api/v1/shift/todos/jeryu/{id}/action"),
                &self.admin,
                json!({"action": "block", "note": "needs root on the build host"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{blocked}");
        id
    }

    async fn people(&self, token: &str, id: &str) -> Vec<String> {
        let (status, body) = self
            .call(HttpMethod::GET, &people_path(id), token, None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body.as_array()
            .unwrap()
            .iter()
            .map(|p| p["login"].as_str().unwrap().to_string())
            .collect()
    }
}

#[tokio::test]
async fn any_person_claims_hands_over_and_finishes_a_blocked_todo() {
    let dir = tempfile::tempdir().unwrap();
    let site = site(dir.path());
    let (status, filed) = site
        .act(
            "/api/v1/shift/todos",
            &site.admin,
            json!({"family": "jeryu", "text": "Rotate the signing key", "mode": "now"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{filed}");
    let id = filed["id"].as_str().unwrap().to_string();
    let action = format!("/api/v1/shift/todos/jeryu/{id}/action");
    let (status, _) = site
        .act(
            &action,
            &site.admin,
            json!({"action": "block", "note": "needs root on the build host"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // The picker lists people who can work in the family: not the reporter
    // login (automation) and not carl (no grant).
    let people = site.people(&site.ada, &id).await;
    assert_eq!(people, ["ada", "alice", "bea"]);

    // carl merely signed up: no claim, no finish, no list of the forge's users.
    for body in [json!({"action": "claim"}), json!({"action": "done"})] {
        let (status, refused) = site.act(&action, &site.carl, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    }
    let (status, _) = site
        .call(HttpMethod::GET, &people_path(&id), &site.carl, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Nobody can hand the todo to carl either: he cannot work on it.
    let (status, refused) = site
        .act(
            &action,
            &site.admin,
            json!({"action": "assign", "value": "carl"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");

    // A reporter (automation) and a non-admin asking for an admin action are
    // forbidden, and nothing changes.
    let (status, body) = site
        .act(&action, &site.reporter, json!({"action": "claim"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "todo_action_forbidden");
    let (status, body) = site
        .act(&action, &site.ada, json!({"action": "release"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Ada claims it; the status and the note stay, the assignee is set.
    let (status, claimed) = site
        .act(&action, &site.ada, json!({"action": "claim"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
    assert_eq!(claimed["assignee"], "ada");
    assert_eq!(claimed["status"], "blocked");
    assert_eq!(claimed["note"], "needs root on the build host");

    // The inbox row says who has it.
    let (status, inbox) = site
        .call(HttpMethod::GET, "/api/v1/attention", &site.admin, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{inbox}");
    let row = inbox["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["todo_id"] == id.as_str())
        .expect("the blocked todo is in the inbox");
    assert_eq!(row["assignee"], "ada");

    // Another user may not take it or finish it while ada has it.
    for body in [json!({"action": "claim"}), json!({"action": "done"})] {
        let (status, refused) = site.act(&action, &site.bea, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    }

    // Ada can hand it only to a person.
    let (status, refused) = site
        .act(
            &action,
            &site.ada,
            json!({"action": "assign", "value": "ci-bot"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let (status, handed) = site
        .act(
            &action,
            &site.ada,
            json!({"action": "assign", "value": "bea"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{handed}");
    assert_eq!(handed["assignee"], "bea");

    // Now only bea finishes it.
    let (status, refused) = site
        .act(&action, &site.ada, json!({"action": "done"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    let (status, done) = site
        .act(
            &action,
            &site.bea,
            json!({"action": "done", "note": "rotated"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["status"], "done");
    assert_eq!(done["assignee"], "bea");
}

#[tokio::test]
async fn an_admin_marks_accounts_as_automation_and_nobody_else_can() {
    let dir = tempfile::tempdir().unwrap();
    let site = site(dir.path());
    let (status, users) = site
        .call(HttpMethod::GET, "/api/v1/admin/users", &site.admin, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{users}");
    let kind_of = |users: &Value, login: &str| {
        let user = users
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["login"] == login)
            .unwrap()
            .clone();
        (user["kind"].clone(), user["kind_source"].clone())
    };
    assert_eq!(kind_of(&users, "ada"), (json!("human"), json!("default")));
    assert_eq!(
        kind_of(&users, "ci-bot"),
        (json!("service"), json!("config"))
    );
    // Existing machine-held deployer credentials retain the service default.
    assert_eq!(
        kind_of(&users, "operator"),
        (json!("service"), json!("config"))
    );

    // A non-admin may neither read the list nor set a kind.
    let (status, _) = site
        .call(HttpMethod::GET, "/api/v1/admin/users", &site.ada, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = site
        .call(
            HttpMethod::PUT,
            "/api/v1/admin/users/bea/kind",
            &site.ada,
            Some(json!({"kind": "service"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, bea) = site
        .call(
            HttpMethod::PUT,
            "/api/v1/admin/users/bea/kind",
            &site.admin,
            Some(json!({"kind": "service"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{bea}");
    assert_eq!(bea["kind"], "service");
    assert_eq!(bea["kind_source"], "set");
    assert_eq!(bea["kind_updated_by"], "alice");
    let id = site.blocked_todo().await;
    assert_eq!(site.people(&site.ada, &id).await, ["ada", "alice"]);

    // A login the configuration names stays automation even if an admin marks
    // it a person: its credential is held by a machine.
    let (status, ci) = site
        .call(
            HttpMethod::PUT,
            "/api/v1/admin/users/ci-bot/kind",
            &site.admin,
            Some(json!({"kind": "human"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{ci}");
    assert_eq!(ci["kind"], "service");
    assert_eq!(ci["kind_source"], "config");

    let (status, _) = site
        .call(
            HttpMethod::PUT,
            "/api/v1/admin/users/nobody/kind",
            &site.admin,
            Some(json!({"kind": "human"})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = site
        .call(
            HttpMethod::PUT,
            "/api/v1/admin/users/bea/kind",
            &site.admin,
            Some(json!({"kind": "robot"})),
        )
        .await;
    assert!(status.is_client_error(), "{status}");
}

#[tokio::test]
async fn a_human_deployer_claims_and_accepts_assignment_only_after_an_explicit_mark() {
    let dir = tempfile::tempdir().unwrap();
    let site = site(dir.path());
    let id = site.blocked_todo().await;
    let action = format!("/api/v1/shift/todos/jeryu/{id}/action");
    let (status, _) = site
        .act(&action, &site.operator, json!({"action": "claim"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        !site
            .people(&site.admin, &id)
            .await
            .contains(&"operator".to_string())
    );

    let (status, operator) = site
        .call(
            HttpMethod::PUT,
            "/api/v1/admin/users/operator/kind",
            &site.admin,
            Some(json!({"kind": "human"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{operator}");
    assert_eq!(operator["kind"], "human");
    assert_eq!(operator["kind_source"], "set");
    assert_eq!(operator["kind_updated_by"], "alice");
    assert!(
        site.people(&site.admin, &id)
            .await
            .contains(&"operator".to_string())
    );
    let (status, claimed) = site
        .act(&action, &site.operator, json!({"action": "claim"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
    assert_eq!(claimed["assignee"], "operator");

    let other = site.blocked_todo().await;
    let other_action = format!("/api/v1/shift/todos/jeryu/{other}/action");
    let (status, assigned) = site
        .act(
            &other_action,
            &site.admin,
            json!({"action": "assign", "value": "operator"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{assigned}");
    assert_eq!(assigned["assignee"], "operator");

    let (status, operator) = site
        .call(
            HttpMethod::PUT,
            "/api/v1/admin/users/operator/kind",
            &site.admin,
            Some(json!({"kind": "service"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{operator}");
    assert_eq!(operator["kind"], "service");
    assert_eq!(operator["kind_source"], "set");
    assert!(
        !site
            .people(&site.admin, &id)
            .await
            .contains(&"operator".to_string())
    );
    let (status, _) = site
        .act(&action, &site.operator, json!({"action": "claim"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = site
        .act(
            &other_action,
            &site.admin,
            json!({"action": "assign", "value": "operator"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// `done` cannot be undone, so a person never finishes work an administrator
/// set aside; and a holder who stops being a person holds nothing.
#[tokio::test]
async fn a_person_cannot_finish_parked_work_and_a_lapsed_holder_frees_the_todo() {
    let dir = tempfile::tempdir().unwrap();
    let site = site(dir.path());
    let id = site.blocked_todo().await;
    let action = format!("/api/v1/shift/todos/jeryu/{id}/action");
    let (status, claimed) = site
        .act(&action, &site.ada, json!({"action": "claim"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{claimed}");

    // An admin parks it; ada still holds it but may no longer finish it.
    let (status, parked) = site
        .act(&action, &site.admin, json!({"action": "park"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{parked}");
    let (status, refused) = site
        .act(&action, &site.ada, json!({"action": "done"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["code"], "todo_action_forbidden");
    // Giving it back is still hers to do.
    let (status, back) = site
        .act(&action, &site.ada, json!({"action": "unassign"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{back}");

    // Blocked again and claimed by ada; then ada's login is marked automation.
    let (status, _) = site
        .act(
            &action,
            &site.admin,
            json!({"action": "block", "note": "again"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = site
        .act(&action, &site.ada, json!({"action": "claim"}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = site
        .call(
            HttpMethod::PUT,
            "/api/v1/admin/users/ada/kind",
            &site.admin,
            Some(json!({"kind": "service"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    // ada holds nothing now: bea may claim the todo.
    let (status, taken) = site
        .act(&action, &site.bea, json!({"action": "claim"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{taken}");
    assert_eq!(taken["assignee"], "bea");
}

/// The attention inbox for a person who is not an administrator: only the
/// blocked todos of a family they can write to, nothing else, and the
/// administrator's acknowledgements stay closed to them.
#[tokio::test]
async fn a_person_with_write_access_reads_their_familys_blocked_todos() {
    let dir = tempfile::tempdir().unwrap();
    let site = site(dir.path());
    let id = site.blocked_todo().await;
    let inbox = "/api/v1/attention";
    let todo_ids = |body: &Value| -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["todo_id"].as_str().unwrap_or_default().to_string())
            .collect()
    };

    let (status, body) = site.call(HttpMethod::GET, inbox, &site.ada, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(todo_ids(&body), std::slice::from_ref(&id));
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["kind"] == "todo_blocked")
    );
    assert_eq!(body["counts"]["action"], 1);
    assert_eq!(body["families"].as_array().unwrap().len(), 1);

    // The administrator still reads the whole inbox, this row among others.
    let (status, whole) = site.call(HttpMethod::GET, inbox, &site.admin, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(todo_ids(&whole).contains(&id));

    // carl has no grant: an empty inbox, and no family names leaked by a
    // filter that matches nothing.
    let (status, body) = site.call(HttpMethod::GET, inbox, &site.carl, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].as_array().unwrap().is_empty());
    let (status, body) = site
        .call(
            HttpMethod::GET,
            "/api/v1/attention?family=nope",
            &site.ada,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["items"].as_array().unwrap().is_empty());

    // Acknowledgements remain an administrator's.
    for token in [&site.ada, &site.carl] {
        let (status, _) = site
            .call(HttpMethod::GET, "/api/v1/attention/acks", token, None)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
