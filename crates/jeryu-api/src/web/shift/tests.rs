use std::path::Path;
use std::process::Command;

use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use chrono::{TimeZone, Utc};
use jeryu_core::{CreateRepositoryRequest, ForgeCore, UserRole};
use jeryu_gitd::{GitdConfig, RepoManager};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::heartbeats::{HeartbeatStore, StoredHeartbeat, build_history};
use super::queue::{QUEUE_REF, WriteError, commit_change, discover, read_todos, resolve};
use super::todo_file::{TodoFile, slugify};
use super::types::Heartbeat;
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
filed_by = "alton@xbabe0"
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
requested_by = "alton"
filed_at = "2026-09-19T01:00:00Z"
claim_by = "alton@xbabe0/w2"
lease_until = ""
shift = "bulletshift/2026-09-18"
change_set = "cs-1"
commits = { "jeryu-web" = "0123456789abcdef" }
merged = false
note = "landed ü"
triaged = true
worked_by = [{ "by" = "alton@xbabe0/w2", "host" = "xbabe0", "slot" = "w2", "model" = "opus", "session" = "s-1", "started" = "2026-09-19T01:01:00Z", "ended" = "2026-09-19T01:30:00Z", "outcome" = "done", "cost_usd" = 1.25, "note" = "", "shift" = "bulletshift/2026-09-18" }]
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
    assert_eq!(todo.requested_by, "alton@xbabe0");
    assert!(
        todo.triaged,
        "a todo filed before triage existed counts as triaged"
    );
    assert_eq!(
        todo.body,
        "Backend cleanup: stop serving fake data.\n\nSecond paragraph."
    );
    let dumped = todo.dump();
    assert!(dumped.contains("requested_by = \"alton@xbabe0\"\n"));
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
    todo.status = "claimed".to_string();
    todo.lease_until = "2026-09-19T02:00:00Z".to_string();
    let before = Utc.with_ymd_and_hms(2026, 9, 19, 1, 0, 0).unwrap();
    let after = Utc.with_ymd_and_hms(2026, 9, 19, 3, 0, 0).unwrap();
    assert!(todo.lease_live(before));
    assert!(!todo.lease_live(after));
    assert_eq!(slugify("  Fix: the THING!! now ", 48), "fix-the-thing-now");
    assert_eq!(slugify("!!!", 48), "todo");
}

pub(crate) fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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
    run_git(&deploy, &["commit", "-q", "-am", "work"]);
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
    assert_eq!(todos[0].todo.requested_by, "alton@xbabe0");
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
            operator: "alton".into(),
            host: "xbabe0".into(),
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
        .insert("alton", &old.heartbeat, now - 15 * 86_400_000)
        .unwrap();
    assert_eq!(store.count(), 1);
    // A write more than an hour later prunes the 15-day-old row.
    store.insert("alton", &old.heartbeat, now).unwrap();
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
    assert_eq!(todos["todos"][0]["worked_by"][0]["by"], "alton@xbabe0/w2");
    let by_worker = body_json(
        call(
            HttpMethod::GET,
            "/api/v1/shift/todos?worked_by=alton@xbabe0",
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
    let all = body_json(
        call(HttpMethod::GET, "/api/v1/shift/todos", &user, None)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(all["todos"].as_array().unwrap().len(), 5);

    // Heartbeats: admin token only, then visible to any login.
    let hb = json!({"operator": "alton", "host": "xbabe0", "slot": "w1", "family": "jeryu",
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
    assert_eq!(created.author, "alton2");
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
    core.create_repository(
        "veox",
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
