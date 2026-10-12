//! Taking a todo on, through the full router with auth on: any person claims,
//! hands over and finishes a todo; another user, a reporter login and a
//! non-admin asking for an admin action are forbidden.

use std::path::Path;

use axum::http::{Method as HttpMethod, StatusCode};
use jeryu_core::{ForgeCore, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::tests::{body_json, fixture, request};
use crate::web::{WebState, app};

struct Site {
    router: axum::Router,
    admin: String,
    ada: String,
    bea: String,
    reporter: String,
}

fn site(dir: &Path) -> Site {
    fixture(dir);
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    for login in ["ada", "bea", "ci-bot"] {
        core.create_account(login, &format!("{login}-password"), UserRole::User)
            .unwrap();
    }
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, ada, bea, reporter) = (token("alice"), token("ada"), token("bea"), token("ci-bot"));
    let router = app(
        WebState::new_with_git_storage(core, dir.to_path_buf()).with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    Site {
        router,
        admin,
        ada,
        bea,
        reporter,
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

    async fn people(&self, token: &str) -> Vec<String> {
        let (status, body) = self
            .call(HttpMethod::GET, "/api/v1/people", token, None)
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

    // The picker lists people; the configured reporter login is automation.
    let people = site.people(&site.ada).await;
    assert_eq!(people, ["ada", "alice", "bea"]);

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
    assert_eq!(site.people(&site.ada).await, ["ada", "alice"]);

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
