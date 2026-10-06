use std::path::Path;

use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use chrono::{SecondsFormat, TimeZone, Utc};
use jeryu_core::{CreateRepositoryRequest, ForgeCore, UserRole};
use jeryu_gitd::{GitdConfig, RepoManager};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::heartbeats::{HeartbeatStore, StoredHeartbeat, build_history};
use super::queue::{QUEUE_REF, WriteError, commit_change, discover, read_todos, resolve};
use super::todo_file::{TodoFile, slugify};
use super::types::{Heartbeat, TodoStatus};
use crate::web::{WebState, app};

const PRE_SHIFT_TODO: &str = r#"+++
id = "20260919-004112-c52b54"
family = "jeryu"
title = "Backend: replace \"fake\" fixture"
repos = ["jeryu-deploy", "jeryu-ci-runner"]
mode = "night"
priority = 3
blocked_by = []
status = "open"
attempts = 0
filed_by = "operator@node-0"
filed_at = "2026-09-19T00:41:12Z"
claim_by = ""
lease_until = ""
change_set = ""
commits = {}
note = ""
+++
Backend cleanup: stop serving fake data.

Second paragraph.
"#;

const CURRENT: &str = r#"+++
id = "20260919-010000-abcdef"
family = "jeryu"
title = "Ship it"
repos = ["jeryu-web"]
mode = "now"
priority = 2
blocked_by = ["20260919-004112-c52b54"]
status = "done"
attempts = 1
requested_by = "operator"
filed_at = "2026-09-19T01:00:00Z"
claim_by = "operator@node-0/w2"
lease_until = ""
shift = "bulletshift/2026-09-18"
change_set = "cs-1"
commits = { "jeryu-web" = "0123456789abcdef" }
merged = false
note = "landed ü"
triaged = true
worked_by = [{ "by" = "operator@node-0/w2", "host" = "node-0", "slot" = "w2", "model" = "opus", "session" = "s-1", "started" = "2026-09-19T01:01:00Z", "ended" = "2026-09-19T01:30:00Z", "outcome" = "done", "cost_usd" = 1.25, "note" = "", "shift" = "bulletshift/2026-09-18" }]
future_field = { "a" = [1, 2], "b" = true }
+++
Do the thing.
"#;

#[test]
fn todoq_files_round_trip_byte_for_byte() {
    let todo = TodoFile::parse(CURRENT).expect("parse current");
    assert_eq!(todo.dump(), CURRENT);
    assert_eq!(todo.extra.len(), 1);
    let api = todo.to_api(Utc::now());
    assert_eq!(api.worked_by[0].cost_usd, Some(1.25));
    assert_eq!(api.worked_by[0].slot, "w2");
    assert_eq!(api.commits["jeryu-web"], "0123456789abcdef");
    assert!(!api.lease_live);
}

#[test]
fn filed_by_reads_as_requested_by_and_dumps_in_todoq_order() {
    let todo = TodoFile::parse(PRE_SHIFT_TODO).expect("parse pre-shift todo");
    assert_eq!(todo.requested_by, "operator@node-0");
    assert!(
        todo.triaged,
        "a todo filed before triage existed counts as triaged"
    );
    assert_eq!(
        todo.body,
        "Backend cleanup: stop serving fake data.\n\nSecond paragraph."
    );
    let dumped = todo.dump();
    assert!(dumped.contains("requested_by = \"operator@node-0\"\n"));
    assert!(!dumped.contains("filed_by"));
    assert!(dumped.contains("title = \"Backend: replace \\\"fake\\\" fixture\"\n"));
    let order: Vec<&str> = dumped
        .lines()
        .skip(1)
        .take_while(|l| *l != "+++")
        .map(|l| l.split(" = ").next().unwrap())
        .collect();
    assert_eq!(
        order,
        [
            "id",
            "family",
            "title",
            "repos",
            "mode",
            "priority",
            "blocked_by",
            "status",
            "attempts",
            "requested_by",
            "filed_at",
            "claim_by",
            "lease_until",
            "shift",
            "change_set",
            "commits",
            "merged",
            "note",
            "triaged",
            "worked_by"
        ]
    );
    assert_eq!(TodoFile::parse(&dumped).unwrap().dump(), dumped);
}

#[test]
fn bad_files_are_refused_and_lease_liveness_is_time_based() {
    assert!(TodoFile::parse("no fence").is_err());
    assert!(TodoFile::parse("+++\nid = \"x\"\n").is_err());
    assert!(TodoFile::parse("+++\nid = \"x\"\nstatus = \"weird\"\n+++\n").is_err());
    let mut todo = TodoFile::parse(PRE_SHIFT_TODO).unwrap();
    todo.status = TodoStatus::Claimed;
    todo.lease_until = "2026-09-19T02:00:00Z".to_string();
    let before = Utc.with_ymd_and_hms(2026, 9, 19, 1, 0, 0).unwrap();
    let after = Utc.with_ymd_and_hms(2026, 9, 19, 3, 0, 0).unwrap();
    assert!(todo.lease_live(before));
    assert!(!todo.lease_live(after));
    assert_eq!(slugify("  Fix: the THING!! now ", 48), "fix-the-thing-now");
    assert_eq!(slugify("!!!", 48), "todo");
}

pub(crate) fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = crate::test_git::git_command()
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const FAMILY_TOML: &str = r#"
[family]
name = "jeryu"
base_branch = "main"
landing = "shifts"
shift_tz = "America/Los_Angeles"

[[repo]]
name = "jeryu-web"
order = 2

[[repo]]
name = "jeryu-deploy"
order = 1
"#;

/// A storage root with `jeryu/jeryu-todo.git` (queue branch: family.toml and
/// two todos) and `jeryu/jeryu-deploy.git` (main plus a nightshift branch).
pub(crate) fn fixture(root: &Path) {
    let owner = root.join("jeryu");
    std::fs::create_dir_all(&owner).unwrap();
    let work = root.join("work-todo");
    std::fs::create_dir_all(work.join("todos")).unwrap();
    run_git(&work, &["init", "-q"]);
    std::fs::write(work.join("family.toml"), FAMILY_TOML).unwrap();
    std::fs::write(
        work.join("todos/20260919-004112-c52b54-backend.md"),
        PRE_SHIFT_TODO,
    )
    .unwrap();
    std::fs::write(
        work.join("todos/20260919-010000-abcdef-ship-it.md"),
        CURRENT,
    )
    .unwrap();
    run_git(&work, &["checkout", "-q", "-b", "queue"]);
    run_git(&work, &["add", "."]);
    run_git(&work, &["commit", "-q", "-m", "seed"]);
    run_git(&owner, &["init", "-q", "--bare", "jeryu-todo.git"]);
    run_git(
        &work,
        &[
            "push",
            "-q",
            owner.join("jeryu-todo.git").to_str().unwrap(),
            "queue",
        ],
    );

    let deploy = root.join("work-deploy");
    std::fs::create_dir_all(&deploy).unwrap();
    run_git(&deploy, &["init", "-q"]);
    std::fs::write(deploy.join("README"), "x").unwrap();
    run_git(&deploy, &["add", "."]);
    run_git(&deploy, &["commit", "-q", "-m", "base"]);
    run_git(&deploy, &["checkout", "-q", "-b", "nightshift/2026-09-18"]);
    std::fs::write(deploy.join("README"), "y").unwrap();
    // Workers end every landing with the todo's trailer; it is how the forge
    // tells work that still needs a review from a branch replayed onto base.
    run_git(
        &deploy,
        &[
            "commit",
            "-q",
            "-am",
            "work\n\nTodo: 20260919-010000-abcdef",
        ],
    );
    run_git(&owner, &["init", "-q", "--bare", "jeryu-deploy.git"]);
    let bare = owner.join("jeryu-deploy.git");
    run_git(
        &deploy,
        &[
            "push",
            "-q",
            bare.to_str().unwrap(),
            "main",
            "nightshift/2026-09-18",
        ],
    );
    // Not a queue: no -todo suffix, and a -todo repo without a queue branch.
    run_git(&owner, &["init", "-q", "--bare", "other-todo.git"]);
}

#[test]
fn discovery_reads_family_toml_and_todos_from_the_bare_repo() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let manager = RepoManager::new(GitdConfig::new(dir.path()));
    let queues = discover(&manager);
    assert_eq!(queues.len(), 1);
    let queue = &queues[0];
    assert_eq!(queue.full_name(), "jeryu/jeryu-todo");
    assert_eq!(queue.family.landing, "shifts");
    assert_eq!(queue.family.repos[0].name, "jeryu-deploy");
    let todos = read_todos("git", &queue.path, &queue.head).unwrap();
    assert_eq!(todos.len(), 2);
    assert_eq!(todos[0].todo.requested_by, "operator@node-0");
}

/// A family queue is written by todoq directly, so the startup sweep opts it
/// out of automatic default-branch protection and removes the rule the forge
/// core created on repository create. Repos that are not queues keep theirs.
#[test]
fn queue_repos_are_exempt_from_default_branch_protection() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    for (name, default_branch) in [("jeryu-todo", "queue"), ("jeryu-deploy", "main")] {
        core.create_repository(
            "jeryu",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: false,
                description: None,
                default_branch: Some(default_branch.to_string()),
            },
        )
        .unwrap();
    }
    assert!(
        core.get_branch_protection("jeryu", "jeryu-todo", "queue")
            .is_ok(),
        "create protects the default branch before the sweep runs"
    );
    let state = WebState::new_with_git_storage(core.clone(), dir.path().to_path_buf());

    assert_eq!(
        super::exempt_queues_from_default_branch_protection(&state, "alice"),
        1
    );
    assert!(
        core.get_repository("jeryu", "jeryu-todo")
            .unwrap()
            .default_branch_protection_opt_out
    );
    assert!(
        core.get_branch_protection("jeryu", "jeryu-todo", "queue")
            .is_err(),
        "todoq pushes claims straight to queue: the branch is not PR-only"
    );
    assert!(
        core.get_branch_protection("jeryu", "jeryu-deploy", "main")
            .is_ok(),
        "a repo that is not a queue keeps its protected default branch"
    );

    // Idempotent: a second startup finds nothing left to exempt.
    assert_eq!(
        super::exempt_queues_from_default_branch_protection(&state, "alice"),
        0
    );
}

#[test]
fn cas_write_commits_on_queue_and_retries_a_lost_race() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let manager = RepoManager::new(GitdConfig::new(dir.path()));
    let queue = discover(&manager).remove(0);
    let before = queue.head.clone();

    // Move the ref underneath the writer on its first pass: the CAS must
    // fail, and the retry must build on the new head.
    let mut passes = 0;
    let result = commit_change(&manager, &queue, "alice", "test write", |todos| {
        passes += 1;
        if passes == 1 {
            let racer = run_git(
                &queue.path,
                &[
                    "commit-tree",
                    &format!("{before}^{{tree}}"),
                    "-p",
                    &before,
                    "-m",
                    "race",
                ],
            );
            run_git(&queue.path, &["update-ref", QUEUE_REF, &racer, &before]);
        }
        let mut todo = todos[0].todo.clone();
        todo.priority = 1;
        Ok::<_, String>((
            vec![(todos[0].path.clone(), Some(todo.dump()))],
            todos.len(),
        ))
    });
    assert_eq!(result.unwrap(), 2);
    assert_eq!(passes, 2);
    let head = resolve("git", &queue.path, QUEUE_REF).unwrap();
    let parent = run_git(&queue.path, &["rev-parse", &format!("{head}^")]);
    assert_eq!(
        run_git(&queue.path, &["log", "-1", "--format=%s", &parent]),
        "race"
    );
    assert_eq!(
        run_git(&queue.path, &["log", "-1", "--format=%an|%s", &head]),
        "alice|test write"
    );
    let todos = read_todos("git", &queue.path, &head).unwrap();
    assert_eq!(todos[0].todo.priority, 1);
    assert_eq!(
        todos[1].todo.dump(),
        CURRENT,
        "untouched files are unchanged"
    );

    let refused = commit_change(&manager, &queue, "alice", "nope", |_| {
        Err::<(Vec<super::queue::Change>, ()), _>("refused")
    });
    assert!(matches!(refused, Err(WriteError::Rejected("refused"))));
    assert_eq!(resolve("git", &queue.path, QUEUE_REF).unwrap(), head);
}

fn beat(
    ms: i64,
    slot: &str,
    state: &str,
    todo: Option<&str>,
    planned: Option<i64>,
) -> StoredHeartbeat {
    StoredHeartbeat {
        received_ms: ms,
        heartbeat: Heartbeat {
            operator: "operator".into(),
            host: "node-0".into(),
            slot: slot.into(),
            family: "jeryu".into(),
            state: state.into(),
            todo_id: todo.map(str::to_string),
            stage: None,
            lease_until: None,
            shift: None,
            planned_slots: planned,
            schedule: None,
            version: None,
        },
    }
}

#[test]
fn history_merges_runs_breaks_on_silence_and_buckets_capacity() {
    let hour = 3_600_000;
    let t0 = Utc
        .with_ymd_and_hms(2026, 9, 19, 8, 0, 0)
        .unwrap()
        .timestamp_millis();
    let rows = vec![
        beat(t0, "w1", "working", Some("a"), Some(3)),
        beat(t0 + 30_000, "w1", "working", Some("a"), Some(3)),
        beat(t0 + 60_000, "w1", "idle", None, Some(3)),
        // Silent for 10 minutes, then back.
        beat(t0 + 660_000, "w1", "idle", None, Some(2)),
        beat(t0 + hour + 10, "w2", "working", Some("b"), None),
    ];
    let history = build_history(&rows, t0, t0 + 2 * hour);
    assert_eq!(history.slots.len(), 2);
    let w1 = &history.slots[0].segments;
    assert_eq!(w1.len(), 3);
    assert_eq!(w1[0].state, "working");
    assert_eq!(w1[0].to, "2026-09-19T08:01:00Z");
    assert_eq!(
        w1[1].to, "2026-09-19T08:03:00Z",
        "silence ends a segment after 120s"
    );
    assert_eq!(w1[2].from, "2026-09-19T08:11:00Z");
    assert_eq!(history.capacity.len(), 3);
    assert_eq!(history.capacity[0].at, "2026-09-19T08:00:00Z");
    assert_eq!(history.capacity[0].planned, 3);
    assert_eq!(history.capacity[0].busy, 1);
    assert_eq!(history.capacity[1].busy, 1);
    assert_eq!(history.capacity[1].planned, 0);
}

#[test]
fn heartbeat_store_migrates_once_and_prunes_old_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shift.sqlite");
    let store = HeartbeatStore::open(&path).unwrap();
    let now = Utc::now().timestamp_millis();
    let old = beat(0, "w1", "idle", None, None);
    store
        .insert("operator", &old.heartbeat, now - 15 * 86_400_000)
        .unwrap();
    assert_eq!(store.count(), 1);
    // A write more than an hour later prunes the 15-day-old row.
    store.insert("operator", &old.heartbeat, now).unwrap();
    assert_eq!(store.count(), 1);
    drop(store);
    // Reopening re-checks the applied migration's checksum and keeps the data.
    let store = HeartbeatStore::open(&path).unwrap();
    assert_eq!(store.count(), 1);
    assert_eq!(store.latest(now - 1000).unwrap().len(), 1);
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn request(
    method: HttpMethod,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> Request<axum::body::Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json");
    builder
        .body(match body {
            Some(b) => axum::body::Body::from(b.to_string()),
            None => axum::body::Body::empty(),
        })
        .unwrap()
}

#[tokio::test]
async fn shift_routes_serve_queue_heartbeats_shifts_and_prs() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "jeryu-deploy".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let admin = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let user = core
        .create_personal_access_token("bob", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new_with_git_storage(core.clone(), dir.path().to_path_buf())
            .with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let call = |method, uri: &str, token: &str, body| {
        router.clone().oneshot(request(method, uri, token, body))
    };

    let families = call(HttpMethod::GET, "/api/v1/shift/families", &user, None)
        .await
        .unwrap();
    assert_eq!(families.status(), StatusCode::OK);
    let families = body_json(families).await;
    assert_eq!(families["families"][0]["queue_repo"], "jeryu/jeryu-todo");
    assert_eq!(families["families"][0]["repos"][1]["name"], "jeryu-web");
    assert_eq!(
        families["families"][0]["repos"][1]["owner"],
        Value::Null,
        "a family repo this forge does not host has no owner to link to"
    );

    let todos = body_json(
        call(
            HttpMethod::GET,
            "/api/v1/shift/todos?family=jeryu&mode=now",
            &user,
            None,
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(todos["todos"].as_array().unwrap().len(), 1);
    assert_eq!(
        todos["todos"][0]["worked_by"][0]["by"],
        "operator@node-0/w2"
    );
    let by_worker = body_json(
        call(
            HttpMethod::GET,
            "/api/v1/shift/todos?worked_by=operator@node-0",
            &user,
            None,
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(by_worker["todos"].as_array().unwrap().len(), 1);

    // POSTs are admin-only.
    let denied = call(
        HttpMethod::POST,
        "/api/v1/shift/todos",
        &user,
        Some(json!({"family": "jeryu", "text": "x", "mode": "now"})),
    )
    .await
    .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let filed = call(
        HttpMethod::POST,
        "/api/v1/shift/todos",
        &admin,
        Some(json!({"family": "jeryu", "text": "Fix the header\nmore detail", "mode": "now"})),
    )
    .await
    .unwrap();
    assert_eq!(filed.status(), StatusCode::CREATED);
    let filed = body_json(filed).await;
    assert_eq!(filed["title"], "Fix the header");
    assert_eq!(filed["requested_by"], "alice");
    assert_eq!(filed["triaged"], false);
    let id = filed["id"].as_str().unwrap().to_string();

    let bulk = body_json(
        call(
            HttpMethod::POST,
            "/api/v1/shift/todos",
            &admin,
            Some(
                json!({"family": "jeryu", "texts": ["one", "two"], "mode": "night",
                        "repos": ["jeryu-web"], "priority": 2}),
            ),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(bulk["todos"].as_array().unwrap().len(), 2);

    let bad = call(
        HttpMethod::POST,
        "/api/v1/shift/todos",
        &admin,
        Some(json!({"family": "jeryu", "text": "x", "mode": "now", "repos": ["nope"]})),
    )
    .await
    .unwrap();
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // The listing is paged: the applied limit comes back, and a limit out of
    // range is refused rather than clamped.
    let get = |uri: String| {
        let call = &call;
        let user = &user;
        async move {
            let response = call(HttpMethod::GET, &uri, user, None).await.unwrap();
            (response.status(), body_json(response).await)
        }
    };
    let (_, all) = get("/api/v1/shift/todos".to_string()).await;
    assert_eq!(all["page"]["limit"], 100);
    assert_eq!(all["page"]["has_more"], false);
    let total = all["todos"].as_array().unwrap().len();
    assert!(total >= 4, "{all}");
    let (_, first) = get("/api/v1/shift/todos?limit=2".to_string()).await;
    assert_eq!(first["todos"].as_array().unwrap().len(), 2);
    assert_eq!(first["page"]["limit"], 2);
    assert_eq!(first["page"]["total"], total);
    assert_eq!(first["page"]["has_more"], true);
    let (_, second) = get("/api/v1/shift/todos?per_page=2&page=2".to_string()).await;
    assert_eq!(second["page"]["page"], 2);
    assert_eq!(second["todos"][0]["id"], all["todos"][2]["id"]);
    for query in [
        "limit=0",
        "limit=501",
        "per_page=abc",
        "page=0",
        "limit=2&per_page=3",
    ] {
        let (status, body) = get(format!("/api/v1/shift/todos?{query}")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{query}");
        assert_eq!(body["code"], "invalid_page_parameter", "{query}");
    }

    let acted = body_json(
        call(
            HttpMethod::POST,
            &format!("/api/v1/shift/todos/jeryu/{id}/action"),
            &admin,
            Some(json!({"action": "priority", "value": 1})),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(acted["priority"], 1);
    let blocked = body_json(
        call(
            HttpMethod::POST,
            &format!("/api/v1/shift/todos/jeryu/{id}/action"),
            &admin,
            Some(json!({"action": "block", "note": "needs design"})),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(blocked["status"], "blocked");
    assert_eq!(blocked["note"], "needs design");
    let missing = call(
        HttpMethod::POST,
        "/api/v1/shift/todos/jeryu/nope/action",
        &admin,
        Some(json!({"action": "release"})),
    )
    .await
    .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    // The fixture's done todo cannot be reopened: the queue is not rewritten.
    let queue_path = dir.path().join("jeryu/jeryu-todo.git");
    let head_before = resolve("git", &queue_path, QUEUE_REF);
    let reopen = call(
        HttpMethod::POST,
        "/api/v1/shift/todos/jeryu/20260919-010000-abcdef/action",
        &admin,
        Some(json!({"action": "release"})),
    )
    .await
    .unwrap();
    assert_eq!(reopen.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(resolve("git", &queue_path, QUEUE_REF), head_before);
    let all = body_json(
        call(HttpMethod::GET, "/api/v1/shift/todos", &user, None)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(all["todos"].as_array().unwrap().len(), 5);

    // Heartbeats: admin token only, then visible to any login.
    let hb = json!({"operator": "operator", "host": "node-0", "slot": "w1", "family": "jeryu",
                    "state": "working", "todo_id": id, "stage": "agent", "planned_slots": 3,
                    "schedule": {"always": 1, "tz": "America/Los_Angeles"}, "version": "todoq-shifts-1"});
    let refused = call(
        HttpMethod::POST,
        "/api/v1/shift/heartbeat",
        &user,
        Some(hb.clone()),
    )
    .await
    .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let ok = body_json(
        call(
            HttpMethod::POST,
            "/api/v1/shift/heartbeat",
            &admin,
            Some(hb),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(ok["ok"], true);
    let invalid = call(
        HttpMethod::POST,
        "/api/v1/shift/heartbeat",
        &admin,
        Some(json!({"operator": "a", "host": "h", "slot": "w1", "family": "jeryu", "state": "dancing"})),
    )
    .await
    .unwrap();
    assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let workers = body_json(
        call(HttpMethod::GET, "/api/v1/shift/workers", &user, None)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(workers["workers"][0]["slot"], "w1");
    assert_eq!(workers["workers"][0]["healthy"], true);
    assert_eq!(workers["workers"][0]["schedule"]["always"], 1);
    let history = body_json(
        call(
            HttpMethod::GET,
            "/api/v1/shift/workers/history?hours=2",
            &user,
            None,
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(history["slots"][0]["segments"][0]["state"], "working");
    assert!(!history["capacity"].as_array().unwrap().is_empty());

    // A family nobody hosts is a mistake in the request, not an empty queue:
    // an agent that misspells one must not read "nothing to do".
    for uri in [
        "/api/v1/shift/todos?family=jeryo",
        "/api/v1/shift/shifts?family=jeryo",
    ] {
        let unknown = call(HttpMethod::GET, uri, &user, None).await.unwrap();
        assert_eq!(unknown.status(), StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
        assert_eq!(body_json(unknown).await["code"], "family_unknown");
    }

    // Nor is a value no todo can carry, or a key the route does not read: the
    // refusal names the values and the keys it does accept.
    for (uri, named) in [
        (
            "/api/v1/shift/todos?status=bogus",
            "open, claimed, done, blocked, handoff, parked, closed",
        ),
        ("/api/v1/shift/todos?mode=bogus", "now, night"),
        ("/api/v1/shift/workers?state=working", "family"),
        ("/api/v1/shift/todos?statuss=open", "status"),
    ] {
        let refused = call(HttpMethod::GET, uri, &user, None).await.unwrap();
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
        let body = body_json(refused).await;
        assert_eq!(body["code"], "invalid_query", "{uri}");
        let said = format!("{} {}", body["reason"], body["common_fixes"]);
        assert!(said.contains(named), "{uri}: {said}");
    }

    // A value in the set still filters.
    let open = body_json(
        call(
            HttpMethod::GET,
            "/api/v1/shift/todos?status=open&mode=night",
            &user,
            None,
        )
        .await
        .unwrap(),
    )
    .await;
    assert!(
        open["todos"]
            .as_array()
            .unwrap()
            .iter()
            .all(|todo| todo["status"] == "open" && todo["mode"] == "night"),
        "{open}"
    );

    // Workers filter by family, and a family nobody hosts is refused rather
    // than answered with every slot.
    let mine = body_json(
        call(
            HttpMethod::GET,
            "/api/v1/shift/workers?family=jeryu",
            &user,
            None,
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(mine["workers"].as_array().unwrap().len(), 1, "{mine}");
    let elsewhere = call(
        HttpMethod::GET,
        "/api/v1/shift/workers?family=jeryo",
        &user,
        None,
    )
    .await
    .unwrap();
    assert_eq!(elsewhere.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(elsewhere).await["code"], "family_unknown");

    // Shifts and the review PR (authored by the configured shift author).
    let shifts = body_json(
        call(
            HttpMethod::GET,
            "/api/v1/shift/shifts?family=jeryu",
            &user,
            None,
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(shifts["shifts"][0]["branch"], "nightshift/2026-09-18");
    assert_eq!(shifts["shifts"][0]["kind"], "nightshift");
    assert_eq!(shifts["shifts"][0]["repos"][0]["ahead"], 1);
    assert_eq!(shifts["shifts"][0]["repos"][0]["behind"], 0);
    let pr = body_json(
        call(
            HttpMethod::POST,
            "/api/v1/shift/shifts/jeryu/pr",
            &admin,
            Some(json!({"branch": "nightshift/2026-09-18"})),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(pr["prs"][0]["repo"], "jeryu-deploy");
    assert_eq!(pr["prs"][0]["created"], true);
    let number = pr["prs"][0]["number"].as_u64().unwrap();
    let created = core
        .get_pull_request("jeryu", "jeryu-deploy", number)
        .unwrap();
    assert_eq!(created.author, "rel-bot");
    assert_eq!(created.title, "nightshift/2026-09-18: 0 todos");
    let again = body_json(
        call(
            HttpMethod::POST,
            "/api/v1/shift/shifts/jeryu/pr",
            &admin,
            Some(json!({"branch": "nightshift/2026-09-18"})),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(again["prs"][0]["created"], false);
    let shifts = body_json(
        call(HttpMethod::GET, "/api/v1/shift/shifts", &user, None)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(shifts["shifts"][0]["repos"][0]["pr"]["number"], number);
    let not_shift = call(
        HttpMethod::POST,
        "/api/v1/shift/shifts/jeryu/pr",
        &admin,
        Some(json!({"branch": "main"})),
    )
    .await
    .unwrap();
    assert_eq!(not_shift.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

fn deploy(core: &ForgeCore, sha: &str, payload: Value) {
    use jeryu_core::{CreateDeploymentRequest, CreateDeploymentStatusRequest, DeploymentState};
    let deployment = core
        .create_deployment(
            "jeryu",
            "jeryu-deploy",
            "alice",
            CreateDeploymentRequest {
                sha: sha.to_string(),
                ref_name: None,
                task: "deploy".to_string(),
                environment: "production".to_string(),
                description: None,
                payload: Some(payload),
                production_environment: None,
                transient_environment: false,
            },
        )
        .unwrap();
    core.create_deployment_status(
        "jeryu",
        "jeryu-deploy",
        deployment.id,
        "alice",
        CreateDeploymentStatusRequest {
            state: DeploymentState::Success,
            description: None,
            environment_url: None,
            log_url: None,
            auto_inactive: true,
        },
    )
    .unwrap();
}

/// The queue file says `merged = false` for nearly every landed todo, so the
/// todos route derives merged, released and the carrying PR from the hosted
/// repositories: through a rebase (new sha, `Todo:` trailer) and a deployment.
#[tokio::test]
async fn todos_route_derives_merged_released_and_pr() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let root = dir.path();
    let bare = root.join("jeryu/jeryu-deploy.git");
    let shift_head = run_git(&bare, &["rev-parse", "refs/heads/nightshift/2026-09-18"]);
    let base = run_git(&bare, &["rev-parse", "refs/heads/main"]);
    let todo_id = "20260919-020000-feed01";
    let landed_todo = CURRENT
        .replace("20260919-010000-abcdef", todo_id)
        .replace("bulletshift/2026-09-18", "nightshift/2026-09-18")
        .replace(
            r#"commits = { "jeryu-web" = "0123456789abcdef" }"#,
            &format!(r#"commits = {{ "jeryu-deploy" = "{shift_head}" }}"#),
        );
    let work = root.join("work-todo");
    std::fs::write(
        work.join("todos/20260919-020000-feed01-landed.md"),
        landed_todo,
    )
    .unwrap();
    run_git(&work, &["add", "."]);
    run_git(&work, &["commit", "-q", "-m", "done feed01"]);
    run_git(
        &work,
        &[
            "push",
            "-q",
            root.join("jeryu/jeryu-todo.git").to_str().unwrap(),
            "queue",
        ],
    );

    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "jeryu-deploy".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let admin = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new_with_git_storage(core.clone(), root.to_path_buf())
            .with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let todo = || async {
        let todos = body_json(
            router
                .clone()
                .oneshot(request(
                    HttpMethod::GET,
                    "/api/v1/shift/todos?family=jeryu&status=done",
                    &admin,
                    None,
                ))
                .await
                .unwrap(),
        )
        .await;
        let todos = todos["todos"].as_array().unwrap().clone();
        // The other done todo names a repo this forge does not host: the file
        // value stands and nothing is known about its release.
        let unhosted = todos.iter().find(|t| t["id"] != todo_id).unwrap();
        assert_eq!(unhosted["merged"], false);
        assert_eq!(unhosted["released"], Value::Null);
        assert_eq!(unhosted["cost_usd"], 1.25, "cost is summed over attempts");
        todos.into_iter().find(|t| t["id"] == todo_id).unwrap()
    };

    // On the shift branch only: not merged, no deployment known, no PR yet.
    let before = todo().await;
    assert_eq!(before["merged"], false);
    assert_eq!(before["released"], Value::Null);
    assert_eq!(before["pr"], Value::Null);

    let opened = router
        .clone()
        .oneshot(request(
            HttpMethod::POST,
            "/api/v1/shift/shifts/jeryu/pr",
            &admin,
            Some(json!({"branch": "nightshift/2026-09-18"})),
        ))
        .await
        .unwrap();
    assert_eq!(opened.status(), StatusCode::OK);
    deploy(&core, &base, json!({"release": "prod-1"}));
    let with_pr = todo().await;
    assert_eq!(with_pr["pr"]["repo"], "jeryu-deploy");
    assert_eq!(with_pr["pr"]["number"], 1);
    // The state is core's PullRequestState, the same spelling the shift
    // cards already use (`mergeable` for an open PR with nothing blocking it).
    assert_eq!(with_pr["pr"]["state"], "mergeable");
    assert_eq!(with_pr["prs"].as_array().unwrap().len(), 1);
    assert_eq!(with_pr["merged"], false);
    assert_eq!(
        with_pr["released"], false,
        "production is known and does not have the work"
    );

    // The shift lands rebased: a new sha on main carrying the Todo trailer.
    let deploy_work = root.join("work-deploy");
    run_git(&deploy_work, &["checkout", "-q", "main"]);
    std::fs::write(deploy_work.join("README"), "y").unwrap();
    run_git(
        &deploy_work,
        &[
            "commit",
            "-q",
            "-am",
            &format!("work\n\nTodo: {todo_id}\nShift: nightshift/2026-09-18"),
        ],
    );
    let landed = run_git(&deploy_work, &["rev-parse", "HEAD"]);
    assert_ne!(landed, shift_head, "a rebase gives the work a new sha");
    run_git(
        &deploy_work,
        &["push", "-q", bare.to_str().unwrap(), "main"],
    );
    let merged = todo().await;
    assert_eq!(merged["merged"], true, "found by its Todo trailer");
    assert_eq!(merged["released"], false);

    deploy(&core, &landed, json!({"release": "prod-2"}));
    let released = todo().await;
    assert_eq!(released["merged"], true);
    assert_eq!(released["released"], true);
}

/// The jain queue is `jain-split/jain-todo` while its code is `veox/*`: a
/// family repo hosted under another owner than its queue still lists its
/// shift branches and gets its review PR.
#[tokio::test]
async fn shifts_resolve_a_family_repo_hosted_under_another_owner() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let root = dir.path();
    std::fs::create_dir_all(root.join("veox")).unwrap();
    std::fs::rename(
        root.join("jeryu/jeryu-deploy.git"),
        root.join("veox/jeryu-deploy.git"),
    )
    .unwrap();

    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();
    core.create_repository(
        "veox",
        CreateRepositoryRequest {
            name: "jeryu-deploy".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let admin = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let outsider = core
        .create_personal_access_token("bob", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new_with_git_storage(core.clone(), root.to_path_buf())
            .with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );

    let shifts = body_json(
        router
            .clone()
            .oneshot(request(
                HttpMethod::GET,
                "/api/v1/shift/shifts?family=jeryu",
                &admin,
                None,
            ))
            .await
            .unwrap(),
    )
    .await;
    let families = body_json(
        router
            .clone()
            .oneshot(request(
                HttpMethod::GET,
                "/api/v1/shift/families",
                &admin,
                None,
            ))
            .await
            .unwrap(),
    )
    .await;
    let hosted: Vec<(&str, Option<&str>)> = families["families"][0]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| (repo["name"].as_str().unwrap(), repo["owner"].as_str()))
        .collect();
    assert!(
        hosted.contains(&("jeryu-deploy", Some("veox"))),
        "the families route names the hosting owner, not the queue's: {hosted:?}"
    );

    // Where a private repo is hosted is not told to an account that cannot read it.
    let seen = body_json(
        router
            .clone()
            .oneshot(request(
                HttpMethod::GET,
                "/api/v1/shift/families",
                &outsider,
                None,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        seen["families"][0]["repos"]
            .as_array()
            .unwrap()
            .iter()
            .all(|repo| repo["owner"].is_null()),
        "{seen}"
    );

    let branches: Vec<&str> = shifts["shifts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["branch"].as_str())
        .collect();
    assert!(
        branches.contains(&"nightshift/2026-09-18"),
        "the veox-hosted repo's shift is listed: {branches:?}"
    );

    let opened = body_json(
        router
            .clone()
            .oneshot(request(
                HttpMethod::POST,
                "/api/v1/shift/shifts/jeryu/pr",
                &admin,
                Some(json!({"branch": "nightshift/2026-09-18"})),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(opened["prs"][0]["created"], true, "{opened}");
    assert!(
        opened["prs"][0]["url"]
            .as_str()
            .unwrap()
            .contains("/veox/jeryu-deploy/"),
        "the PR is opened in the hosting owner: {opened}"
    );
}

/// A linear-history merge replays commits under new shas, so a merged shift
/// still looks "ahead". The `Todo:` trailer tells merged work from work that
/// landed on the branch after its pull request merged and never reached base.
#[test]
fn unmerged_todos_are_found_by_trailer_not_by_sha() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    run_git(repo, &["init", "-q", "."]);
    let commit = |file: &str, message: &str| {
        std::fs::write(repo.join(file), file).unwrap();
        run_git(repo, &["add", "."]);
        run_git(repo, &["commit", "-q", "-m", message]);
    };
    commit("base.txt", "base");
    run_git(repo, &["checkout", "-q", "-b", "nightshift/2026-09-18"]);
    commit("a.txt", "first todo\n\nTodo: 20260919-000001-aaaaaa");
    commit("b.txt", "second todo\n\nTodo: 20260919-000002-bbbbbb");
    commit("c.txt", "no trailer at all");
    // The pull request merged when only the first todo was there, replayed
    // onto main under a new sha.
    run_git(repo, &["checkout", "-q", "main"]);
    commit(
        "a.txt",
        "first todo (replayed)\n\nTodo: 20260919-000001-aaaaaa",
    );

    let found = super::shifts::unmerged_todos("git", repo, "main", "nightshift/2026-09-18");
    assert_eq!(found, ["20260919-000002-bbbbbb"]);
    assert!(super::shifts::unmerged_todos("git", repo, "main", "main").is_empty());
}

/// A shift pull request closed on a queue conflict, replaced by one opened
/// from another branch cherry-picked onto the base: the replacement's commits
/// carry the shift's `Todo:` trailers, so the work is under review there.
#[test]
fn a_replacement_pull_request_is_found_by_the_trailers_it_carries() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    run_git(repo, &["init", "-q", "."]);
    let commit = |file: &str, message: &str| {
        std::fs::write(repo.join(file), file).unwrap();
        run_git(repo, &["add", "."]);
        run_git(repo, &["commit", "-q", "-m", message]);
    };
    commit("base.txt", "base");
    run_git(repo, &["checkout", "-q", "-b", "nightshift/2026-09-28"]);
    commit("a.txt", "first todo\n\nTodo: 20260928-000001-aaaaaa");
    commit("b.txt", "second todo\n\nTodo: 20260928-000002-bbbbbb");
    // The replacement branch: the same work cherry-picked onto the base.
    run_git(repo, &["checkout", "-q", "main"]);
    run_git(
        repo,
        &["checkout", "-q", "-b", "operator/nightshift-2026-09-28-web"],
    );
    commit("a2.txt", "first todo again\n\nTodo: 20260928-000001-aaaaaa");
    commit(
        "b2.txt",
        "second todo again\n\nTodo: 20260928-000002-bbbbbb",
    );
    // A third branch that carries only one of them.
    run_git(repo, &["checkout", "-q", "main"]);
    run_git(repo, &["checkout", "-q", "-b", "operator/partial"]);
    commit("a3.txt", "first todo only\n\nTodo: 20260928-000001-aaaaaa");

    let pr = |number: u64, head: &str, state: &str| {
        serde_json::from_value::<jeryu_core::PullRequest>(json!({
            "id": "00000000-0000-0000-0000-00000000000f",
            "owner": "jeryu",
            "repo": "jeryu-web",
            "number": number,
            "issue_number": number,
            "title": format!("PR {number}"),
            "body": null,
            "state": state,
            "draft": false,
            "author": "rel-bot",
            "head": {"label": format!("jeryu:{head}"), "ref": head, "sha": "a".repeat(40)},
            "base": {"label": "jeryu:main", "ref": "main", "sha": "b".repeat(40)},
            "mergeable": true,
            "mergeable_state": "clean",
            "merged": false,
            "merged_at": null,
            "merge_commit_sha": null,
            "created_at": "2026-09-28T00:00:00Z",
            "updated_at": "2026-09-28T00:00:00Z",
        }))
        .unwrap()
    };
    let todos = vec![
        "20260928-000001-aaaaaa".to_string(),
        "20260928-000002-bbbbbb".to_string(),
    ];
    let find = |prs: &[jeryu_core::PullRequest]| {
        super::shifts::review_elsewhere("git", repo, "main", "nightshift/2026-09-28", &todos, prs)
    };

    let closed = pr(71, "nightshift/2026-09-28", "closed");
    let whole = pr(78, "operator/nightshift-2026-09-28-web", "mergeable");
    let partial = pr(79, "operator/partial", "open");
    let (found, carried) = find(&[closed.clone(), whole.clone()]).unwrap();
    assert_eq!(found.number, 78);
    assert_eq!(carried, todos, "the replacement carries every todo");

    let (found, carried) = find(&[closed.clone(), partial.clone()]).unwrap();
    assert_eq!(found.number, 79);
    assert_eq!(carried, ["20260928-000001-aaaaaa"]);

    // The one carrying the most wins, and a closed replacement is no review.
    let (found, _) = find(&[partial, whole]).unwrap();
    assert_eq!(found.number, 78);
    assert!(
        find(&[
            closed,
            pr(80, "operator/nightshift-2026-09-28-web", "closed")
        ])
        .is_none(),
        "only an open pull request is a review"
    );
}

/// done, close, park and edit over the route: the queue is rewritten, the
/// answer is the stored todo, and an edit may only name a family repo.
#[tokio::test]
async fn todo_actions_finish_park_and_edit_a_todo() {
    let dir = tempfile::tempdir().unwrap();
    let (router, admin) = crate::web::pipeline::tests::shift_forge(dir.path());
    let call = |method, uri: String, body| {
        let router = router.clone();
        let admin = admin.clone();
        async move {
            let response = router
                .oneshot(request(method, &uri, &admin, body))
                .await
                .unwrap();
            (response.status(), body_json(response).await)
        }
    };
    let file = |text: &str| {
        let call = &call;
        let text = text.to_string();
        async move {
            let (status, filed) = call(
                HttpMethod::POST,
                "/api/v1/shift/todos".to_string(),
                Some(json!({"family": "jeryu", "text": text, "mode": "now"})),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{filed}");
            filed["id"].as_str().unwrap().to_string()
        }
    };
    let act = |id: &str, body: Value| {
        let call = &call;
        let uri = format!("/api/v1/shift/todos/jeryu/{id}/action");
        async move { call(HttpMethod::POST, uri, Some(body)).await }
    };

    let id = file("Cut the tag").await;
    let (status, done) = act(&id, json!({"action": "done", "note": "did it by hand"})).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["status"], "done");
    assert_eq!(done["note"], "did it by hand");
    assert_eq!(done["block_kind"], Value::Null);

    let id = file("Rename the thing").await;
    let (status, closed) = act(&id, json!({"action": "close", "note": "not worth it"})).await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(closed["status"], "closed");

    let id = file("Wait for split.8").await;
    let (status, parked) = act(
        &id,
        json!({"action": "park", "until": "2026-10-20T09:00:00Z"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{parked}");
    assert_eq!(parked["status"], "parked");
    assert_eq!(parked["park_until"], "2026-10-20T09:00:00Z");
    let (status, refused) = act(&id, json!({"action": "park", "until": "soon"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["code"], "shift_invalid_request");

    // A todo filed from one line of text is untriaged; the edit is triage.
    let id = file("vague ask").await;
    let (_, before) = call(
        HttpMethod::GET,
        "/api/v1/shift/todos?family=jeryu&status=open".to_string(),
        None,
    )
    .await;
    assert!(
        before["todos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == id.as_str() && t["triaged"] == false),
        "{before}"
    );
    let (status, edited) = act(
        &id,
        json!({"action": "edit", "title": "Pin jeryu-web", "body": "bump the lock",
               "repos": ["jeryu-web"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["title"], "Pin jeryu-web");
    assert_eq!(edited["body"], "bump the lock");
    assert_eq!(edited["repos"], json!(["jeryu-web"]));
    assert_eq!(edited["triaged"], true);
    assert_eq!(edited["status"], "open");

    // A repo the family config does not list is refused, and nothing changes.
    let (status, refused) = act(&id, json!({"action": "edit", "repos": ["acme-ops"]})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["code"], "shift_invalid_request");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("\"acme-ops\" is not in family"),
        "{refused}"
    );
    let (_, after) = act(&id, json!({"action": "priority", "value": 2})).await;
    assert_eq!(after["repos"], json!(["jeryu-web"]), "{after}");

    // The queue holds every change: a fresh read sees the stored statuses.
    let (_, listed) = call(
        HttpMethod::GET,
        "/api/v1/shift/todos?family=jeryu".to_string(),
        None,
    )
    .await;
    let status_of = |id: &str| {
        listed["todos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id)
            .map(|t| t["status"].as_str().unwrap().to_string())
    };
    assert_eq!(status_of(&id).as_deref(), Some("open"));
    assert_eq!(listed["todos"].as_array().unwrap().len(), 6, "{listed}");
}

/// A release while a worker is still running the todo needs `force`: without
/// it the todo would be open work again, a second worker would claim it, and
/// two attempts would run at once.
#[tokio::test]
async fn releasing_a_live_claim_needs_force_and_replaces_the_note() {
    let dir = tempfile::tempdir().unwrap();
    let (router, admin) = crate::web::pipeline::tests::shift_forge(dir.path());
    let call = |uri: String, body| {
        let router = router.clone();
        let admin = admin.clone();
        async move {
            let response = router
                .oneshot(request(HttpMethod::POST, &uri, &admin, body))
                .await
                .unwrap();
            (response.status(), body_json(response).await)
        }
    };
    let (status, filed) = call(
        "/api/v1/shift/todos".to_string(),
        Some(json!({"family": "jeryu", "text": "Fix the cache key", "mode": "now"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{filed}");
    let id = filed["id"].as_str().unwrap().to_string();
    let action = format!("/api/v1/shift/todos/jeryu/{id}/action");

    // Claim it the way a worker does: a lease that has not run out yet.
    let manager = RepoManager::new(GitdConfig::new(dir.path()));
    let queue = discover(&manager).remove(0);
    let claim = |lease_until: &str| {
        let lease_until = lease_until.to_string();
        let id = id.clone();
        commit_change(&manager, &queue, "operator", "claim", move |todos| {
            let found = todos.iter().find(|t| t.todo.id == id).expect("filed todo");
            let mut todo = found.todo.clone();
            todo.status = TodoStatus::Claimed;
            todo.claim_by = "operator@node-0/w1".to_string();
            todo.lease_until = lease_until.clone();
            todo.attempts = 1;
            todo.note = "blocked on the design".to_string();
            Ok::<_, String>((vec![(found.path.clone(), Some(todo.dump()))], ()))
        })
        .expect("claim the todo");
    };
    let live = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    claim(&live);

    let head_before = resolve("git", &queue.path, QUEUE_REF).unwrap();
    let (status, refused) = call(action.clone(), Some(json!({"action": "release"}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["code"], "claim_live");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("operator@node-0/w1"),
        "{refused}"
    );
    assert_eq!(
        resolve("git", &queue.path, QUEUE_REF).unwrap(),
        head_before,
        "a refused release leaves the queue alone"
    );

    // Forced, the same release goes through and the note says why.
    let (status, released) = call(
        action.clone(),
        Some(json!({"action": "release", "force": true, "note": "the slot is gone"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{released}");
    assert_eq!(released["status"], "open");
    assert_eq!(released["note"], "the slot is gone");
    assert_eq!(released["lease_until"], "");
    assert_eq!(released["lease_live"], false);
    assert_eq!(released["attempts"], 0);

    // A lease that has run out means nobody is running the todo, so a plain
    // release still works, and it leaves no reason behind.
    let spent =
        (Utc::now() - chrono::Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    claim(&spent);
    let (status, released) = call(action, Some(json!({"action": "release"}))).await;
    assert_eq!(status, StatusCode::OK, "{released}");
    assert_eq!(released["status"], "open");
    assert_eq!(released["note"], "");
}
