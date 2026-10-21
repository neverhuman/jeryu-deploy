//! What a held pull request's row says about the work it holds: the `Todo:`
//! trailers of its own commits and, when the branch name says so, the shift
//! they were worked on, so a fix todo is filed against the right shift
//! without another look at the branch.

use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use jeryu_core::{
    CreatePullRequestRequest, CreateRepositoryRequest, CreateReviewRequest, ForgeCore, ReviewState,
    UserRole,
};

use super::attention::{Item, PullFacts, pull_items, shift_of_head};
use crate::web::WebState;
use crate::web::pulls::PullPosture;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 21, 13, 0, 0).unwrap()
}

fn hosts() -> super::attention::Hosts {
    super::attention::Hosts::default()
}

fn held(repo: &str, number: u64, head_ref: &str, todos: &[&str]) -> PullFacts {
    PullFacts {
        repo: repo.to_string(),
        number,
        title: "Acme billing: charge once".to_string(),
        author: "acme-worker".to_string(),
        head_sha: "9f1c0f9d1c1f4b0a9d2e3f4a5b6c7d8e9f001122".to_string(),
        head_ref: head_ref.to_string(),
        base_ref: "main".to_string(),
        updated_at: now() - chrono::Duration::minutes(30),
        posture: PullPosture {
            changes_requested: 1,
            ..PullPosture::default()
        },
        changed_paths: vec!["src/billing.rs".to_string()],
        landed: None,
        changes_requested_since: Some(now() - chrono::Duration::hours(2)),
        changes_review: None,
        head_todos: todos.iter().map(|id| id.to_string()).collect(),
    }
}

fn of_kind<'a>(items: &'a [Item], kind: &str) -> &'a Item {
    items
        .iter()
        .find(|item| item.kind == kind)
        .unwrap_or_else(|| panic!("no {kind} row in {items:?}"))
}

/// A shift branch is recognised by its prefix, and nothing else claims a
/// shift: a row for ordinary work must not name one.
#[test]
fn only_a_shift_branch_names_a_shift() {
    for branch in [
        "nightshift/2026-10-20",
        "dayshift/2026-10-20",
        "replace/nightshift-2026-10-20",
    ] {
        assert_eq!(shift_of_head(branch).as_deref(), Some(branch));
    }
    for branch in [
        "main",
        "dev/acme-billing",
        "nightshift",
        "nightshift/",
        "replace/",
        "globex-nightshift/2026-10-20",
    ] {
        assert_eq!(shift_of_head(branch), None, "{branch}");
    }
}

/// The held row names the todos its commits carry and the shift they were
/// worked on.
#[test]
fn a_held_row_names_the_todos_and_the_shift_of_its_head() {
    let items = pull_items(
        &[held(
            "acme/billing",
            7,
            "nightshift/2026-10-20",
            &["20261020-140632-f645d0", "20261020-140626-a17878"],
        )],
        &[],
        now(),
        &hosts(),
    );
    let row = of_kind(&items, "pr_changes_requested");
    let json = serde_json::to_value(row).unwrap();
    assert_eq!(
        json["todos"],
        serde_json::json!(["20261020-140632-f645d0", "20261020-140626-a17878"])
    );
    assert_eq!(json["shift"], "nightshift/2026-10-20");
}

/// A pull request on a branch that is nobody's shift, and one whose commits
/// carry no trailer, leave both keys out rather than guessing.
#[test]
fn a_row_with_no_shift_branch_and_no_trailers_carries_neither_key() {
    let items = pull_items(
        &[held("acme/billing", 8, "dev/acme-billing", &[])],
        &[],
        now(),
        &hosts(),
    );
    let json = serde_json::to_value(of_kind(&items, "pr_changes_requested")).unwrap();
    assert!(json.get("todos").is_none(), "{json}");
    assert_eq!(json["shift"], serde_json::Value::Null);
}

/// The folded campaign row carries the branch's shift and every todo its pull
/// requests hold, each pull request keeping its own list.
#[test]
fn a_campaign_row_names_the_shift_and_every_todo_it_holds() {
    let pulls = vec![
        held(
            "acme/billing",
            11,
            "nightshift/2026-10-20",
            &["20261020-140632-f645d0"],
        ),
        held(
            "acme/web",
            12,
            "nightshift/2026-10-20",
            &["20261020-140632-f645d0", "20261020-140626-a17878"],
        ),
        held(
            "globex/tools",
            13,
            "nightshift/2026-10-20",
            &["20261020-140626-a17878"],
        ),
    ];
    let items = pull_items(&pulls, &[], now(), &hosts());
    let json = serde_json::to_value(of_kind(&items, "pr_branch_campaign")).unwrap();
    assert_eq!(json["shift"], "nightshift/2026-10-20");
    // Sorted and deduplicated: the campaign holds two todos, not four.
    assert_eq!(
        json["todos"],
        serde_json::json!(["20261020-140626-a17878", "20261020-140632-f645d0"])
    );
    assert_eq!(
        json["pulls"][1]["todos"],
        serde_json::json!(["20261020-140632-f645d0", "20261020-140626-a17878"])
    );
    assert_eq!(
        json["pulls"][0]["todos"],
        serde_json::json!(["20261020-140632-f645d0"])
    );
}

fn run_git(dir: &Path, args: &[&str]) -> String {
    crate::web::shift::tests::run_git(dir, args)
}

fn commit(work: &Path, path: &str, text: &str, message: &str) -> String {
    std::fs::write(work.join(path), text).unwrap();
    run_git(work, &["add", "."]);
    run_git(work, &["commit", "-q", "-m", message]);
    run_git(work, &["rev-parse", "HEAD"])
}

/// The collector reads the trailers from the hosted repository, so a held
/// pull request on a real shift branch reaches the inbox naming the two todos
/// its commits carry — and the commit already on `main` is not one of them.
#[test]
fn the_inbox_reads_the_held_heads_trailers_from_the_repository() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = ForgeCore::new();
    core.create_account("acme-owner", "acme-password", UserRole::Admin)
        .unwrap();
    core.create_repository(
        "acme-owner",
        CreateRepositoryRequest {
            name: "billing".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();

    let work = root.join("work-billing");
    std::fs::create_dir_all(&work).unwrap();
    run_git(&work, &["init", "-q", "-b", "main"]);
    commit(
        &work,
        "billing.rs",
        "one\n",
        "billing: start\n\nTodo: 20261001-090000-000000",
    );
    run_git(&work, &["checkout", "-q", "-b", "nightshift/2026-10-20"]);
    commit(
        &work,
        "billing.rs",
        "two\n",
        "billing: charge once\n\nTodo: 20261020-140632-f645d0",
    );
    let head = commit(
        &work,
        "billing.rs",
        "three\n",
        "billing: name the shift\n\nTodo: 20261020-140626-a17878",
    );
    let bare = root.join("acme-owner").join("billing.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    run_git(
        bare.parent().unwrap(),
        &["init", "-q", "--bare", "billing.git"],
    );
    run_git(
        &work,
        &[
            "push",
            "-q",
            bare.to_str().unwrap(),
            "main",
            "nightshift/2026-10-20",
        ],
    );

    let pr = core
        .create_pull_request(
            "acme-owner",
            "billing",
            "acme-worker",
            CreatePullRequestRequest {
                title: "billing: charge once".to_string(),
                head: "nightshift/2026-10-20".to_string(),
                base: "main".to_string(),
                head_sha: Some(head.clone()),
                ..CreatePullRequestRequest::default()
            },
        )
        .unwrap();
    core.create_review(
        "acme-owner",
        "billing",
        pr.number,
        "acme-reviewer",
        CreateReviewRequest {
            body: Some(
                "**Red team: HOLD.**\n\n| | Finding | Where |\n|---|---|---|\n\
                 | 🔴 critical | **Charge runs twice on retry** — d | `billing.rs:2` |"
                    .to_string(),
            ),
            event: ReviewState::ChangesRequested,
            comments: Vec::new(),
            expected_head_sha: Some(head.clone()),
        },
    )
    .unwrap();

    let state = WebState::new_with_git_storage(core, root.to_path_buf());
    let items = super::attention::collect(&state, now()).items;
    let row = of_kind(&items, "pr_changes_requested");
    let json = serde_json::to_value(row).unwrap();
    assert_eq!(json["shift"], "nightshift/2026-10-20", "{json}");
    assert_eq!(
        json["todos"],
        serde_json::json!(["20261020-140626-a17878", "20261020-140632-f645d0"]),
        "{json}"
    );
    assert_eq!(json["review"]["reviewer"], "acme-reviewer", "{json}");
}
