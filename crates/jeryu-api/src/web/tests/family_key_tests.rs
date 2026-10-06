//! One canonical family key across every endpoint that speaks family.
//!
//! The forge hosts one invented family here, `vexel`, whose split manifest
//! and queue spell it `vexel-split`. Every endpoint must answer `vexel` for
//! either spelling, carry a label beside it, and refuse a family nobody hosts
//! with the typed `family_unknown` instead of an empty page.

use super::*;

use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::ForgeCore;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::web::shift::tests::run_git;
use crate::web::{WebState, app};

const EXAMPLE_BOARD: &str = include_str!("../../../../../docs/release-board.example.json");
const CANONICAL: &str = "vexel";
const ALIAS: &str = "vexel-split";
const NIGHTSHIFT: &str = "nightshift/2026-10-01";

const FAMILY_TOML: &str = r#"
[family]
name = "vexel-split"
base_branch = "main"
landing = "shifts"

[[repo]]
name = "vexel-core"
order = 1
"#;

const TODO: &str = r#"+++
id = "20261001-090000-aa11bb"
family = "vexel-split"
title = "Widen the lane"
repos = ["vexel-core"]
mode = "night"
priority = 2
blocked_by = []
status = "open"
attempts = 0
requested_by = "operator"
filed_at = "2026-10-01T09:00:00Z"
claim_by = ""
lease_until = ""
shift = "nightshift/2026-10-01"
change_set = ""
commits = {}
note = ""
triaged = true
+++
Widen the lane.
"#;

/// `vexel/vexel-split-todo.git` (a queue naming its family `vexel-split`) and
/// `vexel/vexel-core.git` with a shift branch on it.
fn storage(root: &std::path::Path) {
    let owner = root.join("vexel");
    std::fs::create_dir_all(&owner).unwrap();

    let queue = root.join("work-queue");
    std::fs::create_dir_all(queue.join("todos")).unwrap();
    run_git(&queue, &["init", "-q"]);
    std::fs::write(queue.join("family.toml"), FAMILY_TOML).unwrap();
    std::fs::write(queue.join("todos/20261001-090000-aa11bb-widen.md"), TODO).unwrap();
    run_git(&queue, &["checkout", "-q", "-b", "queue"]);
    run_git(&queue, &["add", "."]);
    run_git(&queue, &["commit", "-q", "-m", "seed"]);
    run_git(&owner, &["init", "-q", "--bare", "vexel-split-todo.git"]);
    let queue_bare = owner.join("vexel-split-todo.git");
    run_git(
        &queue,
        &["push", "-q", queue_bare.to_str().unwrap(), "queue"],
    );

    let code = root.join("work-core");
    std::fs::create_dir_all(&code).unwrap();
    run_git(&code, &["init", "-q"]);
    std::fs::write(code.join("README"), "x").unwrap();
    run_git(&code, &["add", "."]);
    run_git(&code, &["commit", "-q", "-m", "base"]);
    run_git(&code, &["checkout", "-q", "-b", NIGHTSHIFT]);
    std::fs::write(code.join("README"), "y").unwrap();
    run_git(
        &code,
        &[
            "commit",
            "-q",
            "-am",
            "work\n\nTodo: 20261001-090000-aa11bb",
        ],
    );
    run_git(&owner, &["init", "-q", "--bare", "vexel-core.git"]);
    let code_bare = owner.join("vexel-core.git");
    run_git(
        &code,
        &[
            "push",
            "-q",
            code_bare.to_str().unwrap(),
            "main",
            NIGHTSHIFT,
        ],
    );
}

struct Forge {
    router: axum::Router,
    admin: String,
    _dir: tempfile::TempDir,
}

async fn forge() -> Forge {
    let dir = tempdir().unwrap();
    storage(dir.path());
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    for (name, default_branch) in [("vexel-core", "main"), ("vexel-split-todo", "queue")] {
        core.create_repository(
            "vexel",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: false,
                description: None,
                default_branch: Some(default_branch.to_string()),
            },
        )
        .unwrap();
    }
    // The repositories list learns the family the alias way, from a row
    // written with the manifest's spelling.
    core.set_repository_family("vexel", "vexel-core", Some(ALIAS.to_string()))
        .unwrap();
    let admin = core
        .create_personal_access_token("alice", "test", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new_with_git_storage(core, dir.path().to_path_buf())
            .with_auth(true, false, false),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    );
    let forge = Forge {
        router,
        admin,
        _dir: dir,
    };
    // One pipeline event and one release board, both reported the alias way.
    let posted = forge
        .call(
            HttpMethod::POST,
            "/api/v1/events",
            Some(json!({
                "source": "ci-bot",
                "kind": "todo.claimed",
                "family": ALIAS,
                "summary": "claimed the lane widening",
            })),
        )
        .await;
    assert_eq!(posted.status(), StatusCode::CREATED);
    let board: Value = serde_json::from_str(EXAMPLE_BOARD).unwrap();
    let mut board = board;
    board["family"] = Value::String(ALIAS.to_string());
    let accepted = forge
        .call(
            HttpMethod::PUT,
            &format!("/api/v1/release-board/{ALIAS}"),
            Some(board),
        )
        .await;
    assert_eq!(accepted.status(), StatusCode::OK);
    forge
}

impl Forge {
    async fn call(
        &self,
        method: HttpMethod,
        uri: &str,
        body: Option<Value>,
    ) -> axum::response::Response {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.admin))
            .header(header::CONTENT_TYPE, "application/json")
            .body(match body {
                Some(body) => axum::body::Body::from(body.to_string()),
                None => axum::body::Body::empty(),
            })
            .unwrap();
        self.router.clone().oneshot(request).await.unwrap()
    }

    async fn get(&self, uri: &str) -> Value {
        let response = self.call(HttpMethod::GET, uri, None).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        response_json(response).await
    }
}

/// `(what the endpoint is, its URI with `{family}` to fill in, the keys and
/// labels its answer carries).
type Reading = fn(&Value) -> (Vec<String>, Vec<String>);

fn strings(values: &Value, pointer: &str, field: &str) -> Vec<String> {
    values
        .pointer(pointer)
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get(field))
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn endpoints() -> Vec<(&'static str, &'static str, Reading)> {
    vec![
        (
            "repositories",
            "/api/v1/repos?family={family}",
            (|answer| {
                (
                    strings(answer, "/repositories", "family"),
                    // The repositories list is a jeryu-core contract type and
                    // carries the key alone; its label is `family::label`.
                    strings(answer, "/repositories", "family"),
                )
            }) as Reading,
        ),
        ("attention", "/api/v1/attention?family={family}", |answer| {
            (
                strings(answer, "/items", "family"),
                strings(answer, "/items", "family_label"),
            )
        }),
        ("events", "/api/v1/events?family={family}", |answer| {
            (
                strings(answer, "/events", "family"),
                strings(answer, "/events", "family_label"),
            )
        }),
        (
            "shift todos",
            "/api/v1/shift/todos?family={family}",
            |answer| {
                (
                    strings(answer, "/todos", "family"),
                    strings(answer, "/todos", "family_label"),
                )
            },
        ),
        (
            "shift shifts",
            "/api/v1/shift/shifts?family={family}",
            |answer| {
                (
                    strings(answer, "/shifts", "family"),
                    strings(answer, "/shifts", "family_label"),
                )
            },
        ),
        (
            "release board",
            "/api/v1/release-board/{family}",
            |answer| {
                (
                    vec![answer["family"].as_str().unwrap_or_default().to_string()],
                    vec![
                        answer["family_label"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    ],
                )
            },
        ),
    ]
}

#[tokio::test]
async fn every_family_endpoint_answers_one_key_for_either_spelling() {
    let forge = forge().await;

    // The families list is the directory the others agree with.
    let families = forge.get("/api/v1/shift/families").await;
    assert_eq!(families["families"][0]["name"], CANONICAL);
    assert_eq!(families["families"][0]["label"], CANONICAL);
    // The facet a client builds its family chips from.
    let facets = forge.get("/api/v1/repos").await;
    assert_eq!(facets["facets"]["families"], json!([CANONICAL]));

    for (what, uri, read) in endpoints() {
        for spelling in [CANONICAL, ALIAS] {
            let answer = forge.get(&uri.replace("{family}", spelling)).await;
            let (keys, labels) = read(&answer);
            assert!(
                !keys.is_empty(),
                "{what} answered nothing for ?family={spelling}: {answer}"
            );
            for key in &keys {
                assert_eq!(key, CANONICAL, "{what} for ?family={spelling}: {answer}");
            }
            for label in &labels {
                assert_eq!(label, CANONICAL, "{what} for ?family={spelling}: {answer}");
            }
        }
    }
}

#[tokio::test]
async fn a_family_nobody_hosts_is_typed_not_an_empty_page() {
    let forge = forge().await;
    for (what, uri, _) in endpoints() {
        let response = forge
            .call(HttpMethod::GET, &uri.replace("{family}", "vexelle"), None)
            .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{what} answered a typo'd family with a page"
        );
        let body = response_json(response).await;
        assert_eq!(body["code"], "family_unknown", "{what}");
        assert!(
            body["reason"]
                .as_str()
                .unwrap_or_default()
                .contains(CANONICAL),
            "{what}: the refusal names the families there are: {body}"
        );
    }
}
