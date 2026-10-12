//! `/api/v1/agent-presence` and `/api/v1/repos/:id/agent-presence` through
//! the full router with real tokens: who may report which scope, that a
//! session belongs to the login that registered it (another user can neither
//! overwrite nor end it), the family fan-out, collisions, expiry, the
//! forge-side sources, and that `host` reaches admins only.

use std::path::Path;

use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::{CreateRepositoryRequest, ForgeCore, RepoAccessLevel, UserRole};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::super::{WebState, app};
use crate::web::agent_presence::{LINGER_MS, PresenceRow, PresenceState};
use crate::web::control_plane::{GateRunnerHeartbeat, REVIEWER_LABEL};
use crate::web::shift::Heartbeat;

const MINUTE: i64 = 60 * 1000;

struct Fixture {
    _dir: tempfile::TempDir,
    state: WebState,
    router: axum::Router,
    /// Global admin.
    admin: String,
    /// Writes both acme repositories.
    ada: String,
    /// Writes acme/widgets only.
    grace: String,
    /// Named in the reporter list; no repository grants.
    reporter: String,
    /// Reads nothing beyond public repositories.
    outsider: String,
}

/// `acme/widgets` and `acme/gears` (family acme, public) and `globex/portal`
/// (family globex, private).
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).unwrap();
    core.create_account("root", "admin-password", UserRole::Admin)
        .unwrap();
    for login in ["ada", "grace", "barbara", "edsger"] {
        core.create_account(login, "user-password", UserRole::User)
            .unwrap();
    }
    for (owner, name, private, family) in [
        ("acme", "widgets", false, "acme"),
        ("acme", "gears", false, "acme"),
        ("globex", "portal", true, "globex"),
    ] {
        core.create_repository(
            owner,
            CreateRepositoryRequest {
                name: name.to_string(),
                private,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
        core.set_repository_family(owner, name, Some(family.to_string()))
            .unwrap();
    }
    for (login, repo) in [("ada", "widgets"), ("ada", "gears"), ("grace", "widgets")] {
        core.grant_repo_access("root", login, "acme", repo, RepoAccessLevel::Write)
            .unwrap();
    }
    let token = |login: &str| {
        core.create_personal_access_token(login, "t", None)
            .unwrap()
            .secret
    };
    let (admin, ada, grace, reporter, outsider) = (
        token("root"),
        token("ada"),
        token("grace"),
        token("barbara"),
        token("edsger"),
    );
    let mut state = WebState::new(core).with_auth(true, false, false);
    state.agent_presence = state.agent_presence.clone().with_reporters(&["barbara"]);
    let router = app(state.clone(), Path::new("/tmp/jeryu-no-spa"));
    Fixture {
        _dir: dir,
        state,
        router,
        admin,
        ada,
        grace,
        reporter,
        outsider,
    }
}

async fn call(
    router: &axum::Router,
    method: HttpMethod,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(body.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn session(id: &str, repos: &[&str], families: &[&str]) -> Value {
    let now = chrono::Utc::now().timestamp_millis();
    json!({
        "session_id": id,
        "agent": "claude-code",
        "model": "model-x",
        "host": "node-a",
        "operator": "ada",
        "lane": "agent-presence",
        "branch": "ada/agent-presence",
        "title": "Wire presence into the repo tab",
        "state": "active",
        "repos": repos,
        "families": families,
        "started_at": now - 60_000,
        "last_active_at": now,
        "ttl_seconds": 300,
        "cost_usd": 1.25,
        "tokens": {"input": 10, "output": 20},
        "links": {"pr": "acme/widgets#12", "todo": "acme:20261012-1200-abc123"},
    })
}

impl Fixture {
    async fn post(&self, token: &str, body: Value) -> (StatusCode, Value) {
        call(
            &self.router,
            HttpMethod::POST,
            "/api/v1/agent-presence",
            Some(token),
            Some(body),
        )
        .await
    }

    async fn delete(&self, token: &str, id: &str) -> (StatusCode, Value) {
        call(
            &self.router,
            HttpMethod::DELETE,
            &format!("/api/v1/agent-presence/{id}"),
            Some(token),
            None,
        )
        .await
    }

    async fn repo_items(&self, token: &str, repo: &str) -> Vec<Value> {
        let (status, body) = call(
            &self.router,
            HttpMethod::GET,
            &format!("/api/v1/repos/{repo}/agent-presence"),
            Some(token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["total"], body["items"].as_array().unwrap().len());
        body["items"].as_array().unwrap().clone()
    }

    async fn overview(&self, token: &str, query: &str) -> (StatusCode, Value) {
        call(
            &self.router,
            HttpMethod::GET,
            &format!("/api/v1/agent-presence{query}"),
            Some(token),
            None,
        )
        .await
    }
}

fn find<'a>(items: &'a [Value], id: &str) -> &'a Value {
    items
        .iter()
        .find(|item| item["id"] == id)
        .unwrap_or_else(|| panic!("{id} not in {items:?}"))
}

#[tokio::test]
async fn register_then_heartbeat_upserts_one_session_and_host_is_admin_only() {
    let fx = fixture();
    let (status, body) = fx
        .post(&fx.grace, session("cli:one", &["acme/widgets"], &[]))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["session_id"], "cli:one");
    let first_expiry = body["expires_at"].as_i64().unwrap();
    assert!(first_expiry > chrono::Utc::now().timestamp_millis());

    let mut beat = session("cli:one", &["acme/widgets"], &[]);
    beat["state"] = json!("idle");
    beat["title"] = json!("Waiting for review");
    let (status, body) = fx.post(&fx.grace, beat).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["expires_at"].as_i64().unwrap() >= first_expiry);
    assert_eq!(fx.state.agent_presence.store.count(), 1);

    let items = fx.repo_items(&fx.grace, "acme%2Fwidgets").await;
    assert_eq!(items.len(), 1, "{items:?}");
    let item = &items[0];
    assert_eq!(item["id"], "external:cli:one");
    assert_eq!(item["source"], "external");
    assert_eq!(item["state"], "idle");
    assert_eq!(item["title"], "Waiting for review");
    assert_eq!(item["via"], "repo");
    assert_eq!(item["family"], "acme");
    assert_eq!(item["links"]["pr"], "acme/widgets#12");
    assert_eq!(item["cost_usd"], 1.25);
    assert!(item.get("host").is_none(), "host leaked to a non-admin");

    let items = fx.repo_items(&fx.admin, "acme%2Fwidgets").await;
    assert_eq!(items[0]["host"], "node-a");
}

#[tokio::test]
async fn a_family_session_fans_out_to_every_repo_of_the_family() {
    let fx = fixture();
    let (status, body) = fx.post(&fx.ada, session("cli:fam", &[], &["acme"])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = fx
        .post(
            &fx.ada,
            session("cli:both", &["acme/widgets"], &["acme-split"]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let widgets = fx.repo_items(&fx.ada, "acme%2Fwidgets").await;
    assert_eq!(find(&widgets, "external:cli:fam")["via"], "family");
    assert_eq!(find(&widgets, "external:cli:fam")["family"], "acme");
    assert_eq!(find(&widgets, "external:cli:both")["via"], "repo");

    let gears = fx.repo_items(&fx.ada, "acme%2Fgears").await;
    assert_eq!(find(&gears, "external:cli:fam")["via"], "family");
    assert_eq!(find(&gears, "external:cli:both")["via"], "family");

    assert!(fx.repo_items(&fx.admin, "globex%2Fportal").await.is_empty());

    // Ending the family session takes it off every repository at once.
    let (status, _) = fx.delete(&fx.ada, "cli:fam").await;
    assert_eq!(status, StatusCode::OK);
    for repo in ["acme%2Fwidgets", "acme%2Fgears"] {
        let items = fx.repo_items(&fx.ada, repo).await;
        assert_eq!(find(&items, "external:cli:fam")["state"], "ended");
    }
}

#[tokio::test]
async fn sessions_on_the_same_repo_and_branch_collide() {
    let fx = fixture();
    for id in ["cli:a", "cli:b"] {
        let (status, body) = fx.post(&fx.ada, session(id, &["acme/widgets"], &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let mut other_branch = session("cli:c", &["acme/widgets"], &[]);
    other_branch["branch"] = json!("ada/something-else");
    fx.post(&fx.ada, other_branch).await;
    // A family-scoped session is not known to be on this repository.
    fx.post(&fx.ada, session("cli:fam", &[], &["acme"])).await;

    let items = fx.repo_items(&fx.ada, "acme%2Fwidgets").await;
    assert_eq!(
        find(&items, "external:cli:a")["collides_with"],
        json!(["external:cli:b"])
    );
    assert_eq!(
        find(&items, "external:cli:b")["collides_with"],
        json!(["external:cli:a"])
    );
    assert_eq!(find(&items, "external:cli:c")["collides_with"], json!([]));
    assert_eq!(find(&items, "external:cli:fam")["collides_with"], json!([]));

    // An ended session collides with nothing.
    fx.delete(&fx.ada, "cli:b").await;
    let items = fx.repo_items(&fx.ada, "acme%2Fwidgets").await;
    assert_eq!(find(&items, "external:cli:a")["collides_with"], json!([]));
}

#[tokio::test]
async fn a_non_writer_is_forbidden_to_claim_a_repo_or_family_it_cannot_write() {
    let fx = fixture();
    // grace writes widgets only: gears and the acme family are refused.
    let (status, body) = fx
        .post(&fx.grace, session("cli:g1", &["acme/gears"], &[]))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "presence_scope_forbidden");
    assert!(body["message"].as_str().unwrap().contains("acme/gears"));
    let (status, body) = fx.post(&fx.grace, session("cli:g2", &[], &["acme"])).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body["message"].as_str().unwrap().contains("family acme"));
    // A public repository the outsider can read but not write.
    let (status, body) = fx
        .post(&fx.outsider, session("cli:o1", &["acme/widgets"], &[]))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // A private repository the outsider cannot see reads as unknown, so the
    // refusal does not confirm it exists.
    let (status, body) = fx
        .post(&fx.outsider, session("cli:o2", &["globex/portal"], &[]))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "presence_repo_unknown");
    assert_eq!(fx.state.agent_presence.store.count(), 0);

    // A named reporter and an admin report any scope.
    let (status, body) = fx
        .post(
            &fx.reporter,
            session("cli:r1", &["acme/gears"], &["globex"]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = fx
        .post(&fx.admin, session("cli:a1", &["globex/portal"], &[]))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn another_user_cannot_overwrite_or_end_someone_elses_session() {
    let fx = fixture();
    let (status, _) = fx
        .post(&fx.grace, session("cli:mine", &["acme/widgets"], &[]))
        .await;
    assert_eq!(status, StatusCode::OK);

    // ada writes widgets too, but the session is grace's.
    let mut hijack = session("cli:mine", &["acme/widgets"], &[]);
    hijack["title"] = json!("taken over");
    let (status, body) = fx.post(&fx.ada, hijack).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "presence_session_owned");
    let (status, body) = fx.delete(&fx.ada, "cli:mine").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "presence_session_owned");
    // A non-owner reporter login is refused the same way.
    let (status, _) = fx.delete(&fx.reporter, "cli:mine").await;
    assert_eq!(status, StatusCode::CONFLICT);

    let items = fx.repo_items(&fx.grace, "acme%2Fwidgets").await;
    assert_eq!(items[0]["state"], "active");
    assert_eq!(items[0]["title"], "Wire presence into the repo tab");

    // An admin may beat it; it stays grace's, so grace can still end it.
    let (status, _) = fx
        .post(&fx.admin, session("cli:mine", &["acme/widgets"], &[]))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = fx.delete(&fx.grace, "cli:mine").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "ended");
    let (status, body) = fx.delete(&fx.grace, "cli:nobody").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "presence_session_not_found");
}

#[tokio::test]
async fn anonymous_callers_get_401_on_every_presence_route() {
    let fx = fixture();
    for (method, uri) in [
        (
            HttpMethod::GET,
            "/api/v1/repos/acme%2Fwidgets/agent-presence",
        ),
        (HttpMethod::GET, "/api/v1/repos/acme/widgets/agent-presence"),
        (HttpMethod::GET, "/api/v1/agent-presence"),
        (HttpMethod::DELETE, "/api/v1/agent-presence/cli:one"),
    ] {
        let (status, body) = call(&fx.router, method.clone(), uri, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}: {body}");
    }
    let (status, _) = call(
        &fx.router,
        HttpMethod::POST,
        "/api/v1/agent-presence",
        None,
        Some(session("cli:one", &["acme/widgets"], &[])),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_private_repo_read_is_forbidden_to_a_user_without_access() {
    let fx = fixture();
    let (status, body) = call(
        &fx.router,
        HttpMethod::GET,
        "/api/v1/repos/globex%2Fportal/agent-presence",
        Some(&fx.outsider),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn malformed_beats_are_422_with_a_typed_code() {
    let fx = fixture();
    let mut cases = Vec::new();
    let mut bad_id = session("has space", &["acme/widgets"], &[]);
    cases.push((bad_id.take(), "presence_invalid_request"));
    let mut unknown_field = session("cli:x", &["acme/widgets"], &[]);
    unknown_field["colour"] = json!("blue");
    cases.push((unknown_field, "presence_invalid_request"));
    let mut short_ttl = session("cli:x", &["acme/widgets"], &[]);
    short_ttl["ttl_seconds"] = json!(10);
    cases.push((short_ttl, "presence_invalid_request"));
    let mut bad_state = session("cli:x", &["acme/widgets"], &[]);
    bad_state["state"] = json!("busy");
    cases.push((bad_state, "presence_invalid_request"));
    cases.push((session("cli:x", &[], &[]), "presence_scope_empty"));
    cases.push((
        session("cli:x", &["acme/nope"], &[]),
        "presence_repo_unknown",
    ));
    cases.push((session("cli:x", &[], &["initech"]), "family_unknown"));
    for (body, code) in cases {
        let (status, answer) = fx.post(&fx.admin, body.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {answer}");
        assert_eq!(answer["code"], code, "{body}: {answer}");
    }
    assert_eq!(fx.state.agent_presence.store.count(), 0);
}

fn stored(id: &str, ttl_seconds: i64, state: PresenceState) -> PresenceRow {
    PresenceRow {
        session_id: id.to_string(),
        reporter: "ada".to_string(),
        agent: "codex".to_string(),
        model: None,
        host: None,
        operator: None,
        lane: None,
        branch: None,
        title: None,
        state,
        repos: vec!["acme/widgets".to_string()],
        families: Vec::new(),
        started_ms: 0,
        last_active_ms: 0,
        ttl_seconds,
        received_ms: 0,
        expires_ms: 0,
        ended_ms: None,
        cost_usd: None,
        tokens: None,
        links: None,
    }
}

#[tokio::test]
async fn stale_and_ended_sessions_linger_ten_minutes_then_drop_off() {
    let fx = fixture();
    let store = &fx.state.agent_presence.store;
    let now = chrono::Utc::now().timestamp_millis();
    let mut live = stored("cli:live", 300, PresenceState::Active);
    live.last_active_ms = now;
    store.upsert(&live, now).unwrap();
    // Expired three minutes ago: stale, still listed.
    store
        .upsert(
            &stored("cli:stale", 300, PresenceState::Active),
            now - 8 * MINUTE,
        )
        .unwrap();
    // Expired fifteen minutes ago: past the linger, gone.
    store
        .upsert(
            &stored("cli:gone", 300, PresenceState::Active),
            now - 20 * MINUTE,
        )
        .unwrap();
    // Ended five minutes ago: listed as ended.
    store
        .upsert(
            &stored("cli:ended", 300, PresenceState::Ended),
            now - 5 * MINUTE,
        )
        .unwrap();
    // Ended just past the linger: gone.
    store
        .upsert(
            &stored("cli:ended-old", 3600, PresenceState::Ended),
            now - LINGER_MS - MINUTE,
        )
        .unwrap();

    let items = fx.repo_items(&fx.ada, "acme%2Fwidgets").await;
    let ids: Vec<&str> = items.iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "external:cli:live",
            "external:cli:stale",
            "external:cli:ended"
        ]
    );
    let states: Vec<&str> = items.iter().map(|i| i["state"].as_str().unwrap()).collect();
    assert_eq!(states, ["active", "stale", "ended"]);
}

#[tokio::test]
async fn gate_runners_reviewers_and_queue_workers_show_on_the_repo() {
    let fx = fixture();
    let now = chrono::Utc::now();
    for (runner_id, labels, pr) in [
        ("node-a/slot0", vec![], 7),
        ("node-b/review", vec![REVIEWER_LABEL.to_string()], 8),
    ] {
        let heartbeat: GateRunnerHeartbeat = serde_json::from_value(json!({
            "runnerId": runner_id,
            "host": runner_id.split('/').next().unwrap(),
            "slot": 0,
            "labels": labels,
            "current": {
                "repo": "acme/widgets",
                "pr": pr,
                "sha": "0123456789abcdef0123456789abcdef01234567",
                "recipe": "required",
                "startedAt": now.to_rfc3339(),
            },
        }))
        .unwrap();
        fx.state
            .gate_runners
            .record(heartbeat, "root", now)
            .unwrap();
    }
    let slot = Heartbeat {
        operator: "ada".to_string(),
        host: "node-a".to_string(),
        slot: "1".to_string(),
        family: "acme".to_string(),
        state: "working".to_string(),
        todo_id: Some("20261012-120000-abc123".to_string()),
        stage: Some("agent".to_string()),
        lease_until: None,
        shift: None,
        planned_slots: None,
        schedule: None,
        version: None,
        kind: "todoq".to_string(),
        detail: None,
        capacity: None,
        active: None,
    };
    fx.state
        .shift
        .heartbeats
        .insert("root", &slot, now.timestamp_millis())
        .unwrap();
    let mut idle = slot.clone();
    idle.slot = "2".to_string();
    idle.state = "idle".to_string();
    fx.state
        .shift
        .heartbeats
        .insert("root", &idle, now.timestamp_millis())
        .unwrap();

    let items = fx.repo_items(&fx.ada, "acme%2Fwidgets").await;
    let by_source =
        |source: &str| -> Vec<&Value> { items.iter().filter(|i| i["source"] == source).collect() };
    let gates = by_source("gate");
    assert_eq!(gates.len(), 1, "{items:?}");
    assert_eq!(gates[0]["links"]["pr"], "acme/widgets#7");
    assert_eq!(gates[0]["via"], "repo");
    assert!(gates[0].get("host").is_none());
    assert!(!gates[0]["session_id"].as_str().unwrap().contains("node-a"));
    let reviewers = by_source("reviewer");
    assert_eq!(reviewers.len(), 1, "{items:?}");
    assert_eq!(reviewers[0]["links"]["pr"], "acme/widgets#8");
    // The family hosts no queue, so the slot's todo cannot be read and the
    // working slot is listed for the whole family; the idle one is not.
    let workers = by_source("queue_worker");
    assert_eq!(workers.len(), 1, "{items:?}");
    assert_eq!(workers[0]["via"], "family");
    assert_eq!(workers[0]["links"]["todo"], "20261012-120000-abc123");
    assert!(workers[0].get("host").is_none());
    let gears = fx.repo_items(&fx.ada, "acme%2Fgears").await;
    assert_eq!(gears.len(), 1, "{gears:?}");
    assert_eq!(gears[0]["source"], "queue_worker");

    let admin_items = fx.repo_items(&fx.admin, "acme%2Fwidgets").await;
    let gate = admin_items.iter().find(|i| i["source"] == "gate").unwrap();
    assert_eq!(gate["host"], "node-a");
}

#[tokio::test]
async fn the_overview_hides_sessions_on_repos_a_user_cannot_read() {
    let fx = fixture();
    fx.post(&fx.admin, session("cli:secret", &["globex/portal"], &[]))
        .await;
    fx.post(&fx.ada, session("cli:open", &["acme/widgets"], &[]))
        .await;
    fx.post(&fx.ada, session("cli:fam", &[], &["acme"])).await;

    let (status, body) = fx.overview(&fx.outsider, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&"external:cli:secret"), "{ids:?}");
    assert!(ids.contains(&"external:cli:open"), "{ids:?}");
    let open = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "external:cli:open")
        .unwrap();
    assert!(open.get("via").is_none());
    assert_eq!(open["repos"], json!(["acme/widgets"]));
    assert!(open.get("host").is_none());

    let (_, body) = fx.overview(&fx.admin, "").await;
    assert_eq!(body["total"], 3, "{body}");

    let (status, body) = fx.overview(&fx.ada, "?family=acme").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 2, "{body}");
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["family"] == "acme")
    );
    let (status, body) = fx.overview(&fx.admin, "?family=globex").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 1, "{body}");

    let (status, body) = fx.overview(&fx.ada, "?family=initech").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "family_unknown");
    let (status, body) = fx.overview(&fx.ada, "?colour=blue").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}
