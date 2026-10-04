//! The trace: the pure stage rules, then the route over real bare repos.

use std::path::Path;

use axum::http::{Method as HttpMethod, StatusCode};
use chrono::Utc;
use jeryu_core::{
    CreateDeploymentRequest, CreateDeploymentStatusRequest, CreatePullRequestRequest,
    CreateRepositoryRequest, DeploymentState, ForgeCore, UserRole,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::super::attention::web_routes::route_of;
use super::super::tests::{body_json, request};
use super::super::types::Event;
use super::*;
use crate::web::shift::todo_file::TodoFile;
use crate::web::{WebState, app};

const TODO: &str = "20261003-020951-c0c38e";
const DIST: &str = "2ed65a68fb058a59fc3f9543d9337c1fa6d0903c91b293d4af635c8a1f575ba7";

fn todo(status: TodoStatus) -> ShiftTodo {
    let mut file = TodoFile::new(TODO.to_string(), "acme".to_string(), "Trace it".to_string());
    file.status = status;
    file.filed_at = "2026-10-03T02:09:51Z".to_string();
    file.claim_by = "dev@node-a/w3".to_string();
    file.shift = "nightshift/2026-10-03".to_string();
    file.commits = vec![("widgets-web".to_string(), "c0ffee1".to_string())];
    file.worked_by = vec![
        r#"{ by = "dev@node-a/w3", started = "2026-10-03T03:00:00Z", ended = "2026-10-03T03:40:00Z", outcome = "done" }"#
            .parse()
            .expect("one attempt"),
    ];
    file.to_api(Utc::now())
}

fn facts(todo: ShiftTodo) -> Facts {
    Facts {
        family: Some(todo.family.clone()),
        shift: Some(todo.shift.clone()).filter(|shift| !shift.is_empty()),
        todos: vec![todo],
        repo: Some("acme/widgets-web".to_string()),
        pr: None,
        events: Vec::new(),
        queue: Vec::new(),
        checks: None,
        review: None,
        ship: ShipFacts::default(),
        attention: Vec::new(),
    }
}

fn stage(facts: &Facts, stage: Stage) -> TraceStage {
    let stages = stages(facts);
    stages
        .into_iter()
        .find(|found| found.stage == stage)
        .expect("every stage is answered")
}

fn event(seq: i64, kind: &str, outcome: Option<&str>) -> Event {
    Event {
        seq,
        ts: "2026-10-03T04:00:00Z".to_string(),
        event_id: None,
        source: "pr-gate".to_string(),
        kind: kind.to_string(),
        reporter: "gatebot".to_string(),
        actor: None,
        family: Some("acme".to_string()),
        family_label: Some("acme".to_string()),
        repo: Some("acme/widgets-web".to_string()),
        pr: Some(7),
        sha: None,
        todo_id: Some(TODO.to_string()),
        shift: None,
        outcome: outcome.map(str::to_string),
        needs_human: false,
        summary: format!("{kind} happened"),
        reason: None,
        cost_usd: None,
        seconds: None,
        log_tail: None,
        log_url: None,
        detail: None,
    }
}

fn open_pr() -> PrFacts {
    PrFacts {
        repo: "acme/widgets-web".to_string(),
        number: 7,
        head_sha: "c0ffee1".to_string(),
        head_ref: "nightshift/2026-10-03".to_string(),
        base_ref: "main".to_string(),
        draft: false,
        merged: false,
        closed: false,
        opened_at: Some("2026-10-03T03:45:00Z".to_string()),
        merged_at: None,
        href: "/repos/jeryu/acme/widgets-web/pulls/7".to_string(),
    }
}

/// Twelve stages, always in the same order, and every href a page the web app
/// answers itself: an href that only redirects is a detour that breaks.
#[test]
fn every_stage_is_answered_once_in_order_with_a_page_behind_it() {
    let mut facts = facts(todo(TodoStatus::Done));
    facts.pr = Some(open_pr());
    facts.ship.consumer = Some("acme/deploy".to_string());
    facts.ship.href = Some("/releases?repo=acme/deploy".to_string());
    let stages = stages(&facts);
    assert_eq!(
        stages.iter().map(|stage| stage.stage).collect::<Vec<_>>(),
        Stage::ALL
    );
    for stage in &stages {
        assert!(!stage.summary.is_empty(), "{stage:?} says nothing");
        route_of(&stage.href).unwrap_or_else(|refusal| panic!("{:?}: {refusal}", stage.stage));
    }
}

/// A queued todo nobody has claimed: filed, and waiting at every step after.
#[test]
fn an_open_todo_is_filed_and_waiting_everywhere_else() {
    let mut waiting = todo(TodoStatus::Open);
    waiting.worked_by.clear();
    let queued = facts(waiting);
    let filed = stage(&queued, Stage::Filed);
    assert_eq!(filed.state, StageState::Done);
    assert_eq!(filed.at.as_deref(), Some("2026-10-03T02:09:51Z"));
    assert_eq!(filed.source.from, Origin::Derived);
    assert!(filed.summary.contains("Trace it"), "{filed:?}");
    assert_eq!(filed.href, format!("/work/{TODO}?family=acme"));
    assert_eq!(stage(&queued, Stage::Claimed).state, StageState::Waiting);
    assert_eq!(stage(&queued, Stage::Pr).state, StageState::Waiting);
    // Released back to the queue after an attempt, it was claimed once: the
    // stage records that it happened, and `done` says the work is not.
    let released = facts(todo(TodoStatus::Open));
    assert_eq!(stage(&released, Stage::Claimed).state, StageState::Done);
    assert_eq!(stage(&released, Stage::Done).state, StageState::Waiting);
    assert_eq!(stage(&queued, Stage::Merged).state, StageState::Waiting);
    // Nothing pins it and no deployment is known: the difference between a
    // stage that does not apply and one nothing can answer.
    assert_eq!(
        stage(&queued, Stage::Pinned).state,
        StageState::NotApplicable
    );
    assert_eq!(stage(&queued, Stage::Deployed).state, StageState::Unknown);
}

/// A stopped todo is blocked at `done`, not failed: a person moves it on.
#[test]
fn a_blocked_todo_leads_with_its_own_note() {
    let mut blocked = todo(TodoStatus::Blocked);
    blocked.note = "OWNER: the credential is only yours".to_string();
    let facts = facts(blocked);
    let done = stage(&facts, Stage::Done);
    assert_eq!(done.state, StageState::Blocked);
    assert_eq!(done.summary, "OWNER: the credential is only yours");
    assert_eq!(stage(&facts, Stage::Claimed).state, StageState::Done);
}

/// The event log wins over current state wherever it speaks, and says which
/// `seq` said so: the reader can go straight to it in `GET /api/v1/events`.
#[test]
fn an_event_answers_a_stage_and_names_its_sequence() {
    let mut facts = facts(todo(TodoStatus::Done));
    facts.pr = Some(open_pr());
    facts.checks = Some(Checks::Failing);
    facts.events = vec![
        event(31, "queue.enqueued", None),
        event(30, "gate.finished", Some("success")),
        event(12, "todo.claimed", None),
    ];
    let gate = stage(&facts, Stage::Gate);
    assert_eq!(gate.state, StageState::Done);
    assert_eq!(
        gate.source,
        Source {
            from: Origin::Event,
            seq: Some(30)
        }
    );
    assert_eq!(gate.at.as_deref(), Some("2026-10-03T04:00:00Z"));
    assert_eq!(stage(&facts, Stage::Queue).state, StageState::Active);
    assert_eq!(
        stage(&facts, Stage::Claimed).source.seq,
        Some(12),
        "the first claim dates the stage, not the newest event"
    );

    // Without the event the head's own checks answer, and say they were
    // reported rather than derived.
    facts.events.retain(|event| event.kind != "gate.finished");
    let fallback = stage(&facts, Stage::Gate);
    assert_eq!(fallback.state, StageState::Failed);
    assert_eq!(fallback.source.from, Origin::Reported);
    assert_eq!(fallback.source.seq, None);
}

/// A gate that is still running, a reviewer who asked for changes and a queue
/// that failed each stop their own stage and no other.
#[test]
fn a_stage_reads_the_newest_event_of_its_own_kinds() {
    let mut facts = facts(todo(TodoStatus::Done));
    facts.pr = Some(open_pr());
    facts.events = vec![
        event(44, "queue.failed", None),
        event(43, "pr.review", Some("request_changes")),
        event(42, "gate.started", None),
    ];
    assert_eq!(stage(&facts, Stage::Gate).state, StageState::Active);
    assert_eq!(stage(&facts, Stage::Review).state, StageState::Blocked);
    assert_eq!(stage(&facts, Stage::Queue).state, StageState::Failed);
    assert_eq!(
        stage(&facts, Stage::Pr).state,
        StageState::Active,
        "the pull request itself is still open"
    );
}

/// The pinned path: the consumer's pin, its staged release and what production
/// runs, each `done` only when it reaches the work.
#[test]
fn pinned_staged_and_deployed_follow_the_consumer_that_ships_the_work() {
    let mut facts = facts(todo(TodoStatus::Done));
    facts.ship = ShipFacts {
        consumer: Some("acme/deploy".to_string()),
        pinned: Some(Shipped {
            sha: Some("1234567890abcdef".to_string()),
            at: Some("2026-10-03T05:00:00Z".to_string()),
            reaches: false,
        }),
        staged: None,
        deployed: None,
        released: None,
        released_at: None,
        href: Some("/releases?repo=acme/deploy".to_string()),
    };
    let behind = stage(&facts, Stage::Pinned);
    assert_eq!(behind.state, StageState::Waiting);
    assert!(
        behind.summary.contains("acme/deploy still pins 1234567"),
        "{behind:?}"
    );
    assert_eq!(behind.href, "/releases?repo=acme/deploy");
    assert_eq!(stage(&facts, Stage::Staged).state, StageState::Unknown);

    let bumped = facts.ship.pinned.as_mut().expect("a pin");
    bumped.reaches = true;
    facts.ship.deployed = Some(Shipped {
        sha: Some("fedcba0987654321".to_string()),
        at: Some("2026-10-03T06:00:00Z".to_string()),
        reaches: true,
    });
    assert_eq!(stage(&facts, Stage::Pinned).state, StageState::Done);
    let deployed = stage(&facts, Stage::Deployed);
    assert_eq!(deployed.state, StageState::Done);
    assert_eq!(deployed.at.as_deref(), Some("2026-10-03T06:00:00Z"));
    assert_eq!(deployed.source.from, Origin::Reported);
    // Nothing reported the staging, but production cannot run a release that
    // was never staged.
    assert_eq!(stage(&facts, Stage::Staged).state, StageState::Done);
}

/// A repository that releases itself has no pin to wait for; its deployment
/// is the derived `released` of `GET /api/v1/shift/todos`.
#[test]
fn a_repository_that_releases_itself_is_deployed_by_its_own_production() {
    let mut facts = facts(todo(TodoStatus::Done));
    facts.ship.released = Some(false);
    facts.ship.released_at = Some("2026-10-03T06:00:00Z".to_string());
    facts.ship.href = Some("/releases?repo=acme/widgets-web".to_string());
    assert_eq!(
        stage(&facts, Stage::Pinned).state,
        StageState::NotApplicable
    );
    let deployed = stage(&facts, Stage::Deployed);
    assert_eq!(deployed.state, StageState::Waiting);
    assert!(
        deployed.summary.contains("older than the work"),
        "{deployed:?}"
    );
    facts.ship.released = Some(true);
    assert_eq!(stage(&facts, Stage::Deployed).state, StageState::Done);
}

/// Every attention kind the inbox can raise about one piece of work lands on
/// exactly one stage, so a stuck stage carries its own next step.
#[test]
fn each_attention_kind_belongs_to_one_stage_or_to_the_forge() {
    for kind in [
        "todo_untriaged",
        "todo_blocked",
        "todo_stuck_claim",
        "shift_without_pr",
        "pr_draft_waiting",
        "pr_checks_failing",
        "pr_awaiting_approval",
        "queue_refused",
        "pin_behind",
        "release_staged",
        "deploy_failed",
    ] {
        assert!(stage_of(kind).is_some(), "{kind} belongs to no stage");
    }
    // A diverged or failing mirror is about the forge, not about one piece of
    // work: the inbox keeps it and the trace stays quiet.
    assert_eq!(stage_of("mirror_diverged"), None);
    assert_eq!(stage_of("mirror_failing"), None);
    // A release board is a family's picture of what it runs, not a stage of
    // one todo's journey.
    assert_eq!(stage_of("release_board_problem"), None);
}

// The route, over hosted bare repositories: an acme todo in acme/widgets-web,
// which ships by being pinned into acme/deploy.

fn run_git(dir: &Path, args: &[&str]) -> String {
    crate::web::shift::tests::run_git(dir, args)
}

fn commit_file(work: &Path, path: &str, text: &str, message: &str) -> String {
    if let Some(parent) = work.join(path).parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(work.join(path), text).unwrap();
    run_git(work, &["add", "."]);
    run_git(work, &["commit", "-q", "-m", message]);
    run_git(work, &["rev-parse", "HEAD"])
}

fn bare(root: &Path, owner: &str, name: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(root.join(owner)).unwrap();
    let path = root.join(owner).join(format!("{name}.git"));
    if !path.exists() {
        run_git(
            &root.join(owner),
            &["init", "-q", "--bare", &format!("{name}.git")],
        );
    }
    path
}

fn push(work: &Path, bare: &Path, refs: &[&str]) {
    let mut args = vec!["push", "-q", "-f", bare.to_str().unwrap()];
    args.extend(refs);
    run_git(work, &args);
}

fn register(core: &ForgeCore, owner: &str, name: &str, default_branch: &str) {
    core.create_repository(
        owner,
        CreateRepositoryRequest {
            name: name.to_string(),
            private: false,
            description: None,
            default_branch: Some(default_branch.to_string()),
        },
    )
    .unwrap();
}

const FAMILY: &str = r#"
[family]
name = "acme"
base_branch = "main"
landing = "shifts"
shift_tz = "America/Los_Angeles"

[[repo]]
name = "widgets-web"
order = 1
"#;

fn todo_file(shift_sha: &str) -> String {
    let mut file = TodoFile::new(
        TODO.to_string(),
        "acme".to_string(),
        "Widget: one trace".to_string(),
    );
    file.status = TodoStatus::Done;
    file.repos = vec!["widgets-web".to_string()];
    file.filed_at = "2026-10-03T02:09:51Z".to_string();
    file.claim_by = "dev@node-a/w3".to_string();
    file.shift = "nightshift/2026-10-03".to_string();
    file.commits = vec![("widgets-web".to_string(), shift_sha.to_string())];
    file.dump()
}

fn lock(pin: &str) -> String {
    format!("[[repo]]\nname = \"widgets-web\"\ncommit = \"{pin}\"\nweb_dist_sha256 = \"{DIST}\"\n")
}

/// One look at the route. A fresh `WebState` per look on purpose: the pins
/// and inbox answers are cached for as long as a reader would want them, and
/// these tests move the repositories underneath them.
async fn look(core: &ForgeCore, root: &Path, path: &str, token: &str) -> axum::response::Response {
    let router = app(
        WebState::new_with_git_storage(core.clone(), root.to_path_buf())
            .with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    router
        .oneshot(request(HttpMethod::GET, path, token, None))
        .await
        .unwrap()
}

async fn trace(core: &ForgeCore, root: &Path, query: &str, token: &str) -> Value {
    let response = look(core, root, &format!("/api/v1/trace?{query}"), token).await;
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await
}

/// One stage of an answer, by name.
fn stage_of_body(body: &Value, stage: &str) -> Value {
    body["stages"]
        .as_array()
        .expect("stages")
        .iter()
        .find(|found| found["stage"] == stage)
        .unwrap_or_else(|| panic!("no {stage} stage in {body}"))
        .clone()
}

fn state_of(body: &Value, stage: &str) -> String {
    stage_of_body(body, stage)["state"]
        .as_str()
        .expect("a state")
        .to_string()
}

fn deploy_production(core: &ForgeCore, sha: &str, release: &str) {
    let deployment = core
        .create_deployment(
            "acme",
            "deploy",
            "alice",
            CreateDeploymentRequest {
                sha: sha.to_string(),
                ref_name: None,
                task: "deploy".to_string(),
                environment: "production".to_string(),
                description: None,
                payload: Some(json!({ "release": release })),
                production_environment: None,
                transient_environment: false,
            },
        )
        .unwrap();
    core.create_deployment_status(
        "acme",
        "deploy",
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

/// acme todo `TODO` lands in acme/widgets-web, which has no release of its
/// own: acme/deploy pins it and ships it. Before the pin bump the work is
/// merged and nowhere near production; after the bump and a deploy the trace
/// says `pinned = done` and `deployed = done`. This is what no surface could
/// answer before: `shift/truth.rs` leaves a pinned repository's work
/// `released = null` for ever.
#[tokio::test]
async fn a_pinned_repository_traces_through_the_consumer_that_releases_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();

    // The work: one shift commit on a nightshift branch, carrying its trailer.
    let web = root.join("work-web");
    std::fs::create_dir_all(&web).unwrap();
    run_git(&web, &["init", "-q"]);
    let before = commit_file(&web, "widget.js", "1", "the widget");
    run_git(&web, &["checkout", "-q", "-b", "nightshift/2026-10-03"]);
    let shift_head = commit_file(
        &web,
        "widget.js",
        "2",
        &format!("widget: one trace\n\nTodo: {TODO}"),
    );
    let web_bare = bare(root, "acme", "widgets-web");
    push(&web, &web_bare, &["main", "nightshift/2026-10-03"]);
    register(&core, "acme", "widgets-web", "main");

    // The family queue: one done todo, its commit on the shift branch.
    let queue = root.join("work-todo");
    std::fs::create_dir_all(queue.join("todos")).unwrap();
    run_git(&queue, &["init", "-q"]);
    std::fs::write(queue.join("family.toml"), FAMILY).unwrap();
    std::fs::write(
        queue.join(format!("todos/{TODO}-one-trace.md")),
        todo_file(&shift_head),
    )
    .unwrap();
    run_git(&queue, &["checkout", "-q", "-b", "queue"]);
    run_git(&queue, &["add", "."]);
    run_git(&queue, &["commit", "-q", "-m", "file the todo"]);
    push(&queue, &bare(root, "acme", "acme-todo"), &["queue"]);

    // The consumer, pinned at the commit before the work.
    let deploy = root.join("work-deploy");
    std::fs::create_dir_all(&deploy).unwrap();
    run_git(&deploy, &["init", "-q"]);
    commit_file(
        &deploy,
        "acme-split.lock.toml",
        &lock(&before),
        "pin widgets",
    );
    let deploy_bare = bare(root, "acme", "deploy");
    push(&deploy, &deploy_bare, &["main"]);
    register(&core, "acme", "deploy", "main");

    let pr = core
        .create_pull_request(
            "acme",
            "widgets-web",
            "dev",
            CreatePullRequestRequest {
                title: "widget: one trace".to_string(),
                head: "nightshift/2026-10-03".to_string(),
                base: "main".to_string(),
                head_sha: Some(shift_head.clone()),
                ..CreatePullRequestRequest::default()
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
    // The trace names the todos, pull requests and deployments of every
    // repository the work touches: an ordinary account may not read it.
    let refused = look(&core, root, &format!("/api/v1/trace?todo={TODO}"), &user).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(refused).await["code"], "permission_denied");

    // On the shift branch, with the consumer pinned behind it.
    let open = trace(&core, root, &format!("todo={TODO}"), &admin).await;
    assert_eq!(open["schema_version"], "jeryu.trace/v1");
    assert_eq!(open["subject"]["todos"], json!([TODO]));
    assert_eq!(open["subject"]["repo"], "acme/widgets-web");
    assert_eq!(open["subject"]["released_by"], "acme/deploy");
    assert_eq!(open["subject"]["shift"], "nightshift/2026-10-03");
    assert_eq!(open["subject"]["pr"], pr.number);
    assert_eq!(state_of(&open, "filed"), "done");
    assert_eq!(state_of(&open, "done"), "done");
    assert_eq!(state_of(&open, "shift"), "done");
    assert_eq!(state_of(&open, "pr"), "active");
    assert_eq!(state_of(&open, "merged"), "waiting");
    assert_eq!(state_of(&open, "pinned"), "waiting");
    assert_eq!(state_of(&open, "deployed"), "unknown");

    // The shift lands rebased onto main: a new sha carrying the trailer.
    run_git(&web, &["checkout", "-q", "main"]);
    let landed = commit_file(
        &web,
        "widget.js",
        "2",
        &format!("widget: one trace\n\nTodo: {TODO}"),
    );
    assert_ne!(landed, shift_head, "a rebase gives the work a new sha");
    push(&web, &web_bare, &["main"]);
    let merged = trace(&core, root, &format!("todo={TODO}"), &admin).await;
    assert_eq!(state_of(&merged, "merged"), "done");
    assert_eq!(
        state_of(&merged, "pinned"),
        "waiting",
        "merged work the consumer does not pin is not released"
    );

    // The pin bump, and a deploy of the consumer that carries it.
    commit_file(
        &deploy,
        "acme-split.lock.toml",
        &lock(&landed),
        &format!("release: pin widgets-web {}", &landed[..7]),
    );
    push(&deploy, &deploy_bare, &["main"]);
    let deploy_head = run_git(&deploy, &["rev-parse", "HEAD"]);
    deploy_production(&core, &deploy_head, "acme-deploy-2026.10.03");

    let shipped = trace(&core, root, &format!("todo={TODO}"), &admin).await;
    let pinned = stage_of_body(&shipped, "pinned");
    assert_eq!(pinned["state"], "done");
    assert!(
        pinned["summary"]
            .as_str()
            .unwrap()
            .starts_with("acme/deploy pins it at"),
        "{pinned}"
    );
    assert_eq!(pinned["href"], "/releases?repo=acme/deploy");
    assert_eq!(state_of(&shipped, "deployed"), "done");
    assert_eq!(
        state_of(&shipped, "staged"),
        "done",
        "production cannot run a release that was never staged"
    );

    // The same work from the other side: the pull request page's question,
    // answered by joining on the `Todo:` trailer the landing carries.
    let from_pr = trace(
        &core,
        root,
        &format!("repo=acme/widgets-web&pr={}", pr.number),
        &admin,
    )
    .await;
    assert_eq!(from_pr["subject"]["todos"], json!([TODO]));
    assert_eq!(from_pr["subject"]["family"], "acme");
    assert_eq!(state_of(&from_pr, "deployed"), "done");
}

/// A request that names neither one todo nor one pull request, or names work
/// this forge does not have, is a typed refusal: answering it with empty
/// stages would read as "nothing happened".
#[tokio::test]
async fn a_trace_of_nothing_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
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
    let call = |path: &'static str| {
        let router = router.clone();
        let admin = admin.clone();
        async move {
            let response = router
                .oneshot(request(HttpMethod::GET, path, &admin, None))
                .await
                .unwrap();
            (response.status(), body_json(response).await)
        }
    };
    let (status, body) = call("/api/v1/trace").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "trace_invalid_query");
    assert!(
        body["message"].as_str().unwrap().contains("todo=<id>"),
        "{body}"
    );

    let (status, body) = call("/api/v1/trace?repo=widgets-web&pr=7").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "trace_invalid_query");

    let (status, body) = call("/api/v1/trace?todo=20261003-000000-nobody").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "trace_todo_not_found");

    let (status, body) = call("/api/v1/trace?repo=acme/widgets-web&pr=7").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "trace_pull_not_found");
}
