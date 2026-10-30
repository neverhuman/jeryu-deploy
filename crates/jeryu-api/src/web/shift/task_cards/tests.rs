use std::path::Path;

use axum::http::{Method, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use super::*;
use crate::web::shift::queue::{read_todos, resolve};
use crate::web::shift::tests::{body_json, request, run_git};

const FAMILY: &str =
    "[family]\nname = \"jopedime\"\ntask_cards = true\n[[repo]]\nname = \"JopeDime\"\n";
const CARD: &str = "# API-01 — Contract crate\n\n| Field | Value |\n|---|---|\n| Status | claimed |\n| Assigned screen / host | sol_api / xbabe2 |\n| Depends on | — |\n\n**Owns:** crates/api\n**Done when:** Round-trip proof and OWNER approval.\n";

fn commit(dir: &Path, files: &[(&str, &str)]) -> String {
    for (path, text) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-q", "-m", "fixture"]);
    run_git(dir, &["rev-parse", "HEAD"])
}

fn repo(root: &Path) -> std::path::PathBuf {
    let dir = root.join("root/jopedime-todo.git");
    std::fs::create_dir_all(&dir).unwrap();
    run_git(&dir, &["init", "-q", "-b", "queue"]);
    dir
}

#[test]
fn task_cards_are_opt_in_and_replace_only_matching_native_ids() {
    let temp = tempfile::tempdir().unwrap();
    let dir = repo(temp.path());
    let native = TodoFile::new(
        "jopedime-api-01".into(),
        "jopedime".into(),
        "outdated native title".into(),
    );
    let other = TodoFile::new("unrelated".into(), "jopedime".into(), "native work".into());
    let mut legacy = native.clone();
    legacy.id = "old-tracking-id".to_string();
    legacy.extra.push((
        "external_task".to_string(),
        toml_edit::Value::from("API-01"),
    ));
    let old = commit(
        &dir,
        &[
            (
                "family.toml",
                &FAMILY.replace("task_cards = true", "task_cards = false"),
            ),
            ("tasks/T001-API-01-contract.md", CARD),
            ("todos/api.md", &native.dump()),
            ("todos/other.md", &other.dump()),
            ("todos/legacy.md", &legacy.dump()),
        ],
    );
    let before = read_todos("git", &dir, &old).unwrap();
    assert_eq!(before.len(), 3);
    assert!(before.iter().all(|todo| todo.path.starts_with("todos/")));
    let new = commit(
        &dir,
        &[
            ("family.toml", FAMILY),
            (
                "status/API-01.md",
                "# Self-review\n\nStatus: READY_FOR_REVIEW — skeleton only\n",
            ),
        ],
    );
    let after = read_todos("git", &dir, &new).unwrap();
    assert_eq!(after.len(), 2);
    let card = after
        .iter()
        .find(|todo| todo.todo.id == "jopedime-api-01")
        .unwrap();
    assert_eq!(card.path, "tasks/T001-API-01-contract.md");
    assert_eq!(card.todo.status, TodoStatus::Handoff);
    assert!(!card.todo.merged);
    assert_eq!(card.todo.claim_by, "sol_api / xbabe2");
    assert!(card.todo.body.contains(CARD));
    assert!(card.todo.body.contains(&format!(
        "/repos/jeryu/root/jopedime-todo/blob/{new}/tasks/"
    )));
    assert_eq!(
        read_todos("git", &dir, &old).unwrap()[0].todo.title,
        "outdated native title"
    );
    assert_eq!(
        run_git(
            &dir,
            &["show", &format!("{new}:tasks/T001-API-01-contract.md")]
        ),
        CARD.trim()
    );
}

#[test]
fn task_cards_followups_and_dependencies_use_original_ids() {
    let temp = tempfile::tempdir().unwrap();
    let dir = repo(temp.path());
    let head = commit(
        &dir,
        &[
            ("family.toml", FAMILY),
            ("tasks/T001-API-01-contract.md", CARD),
            (
                "tasks/API-01-F01-integration.md",
                "# API-01-F01 — Wire modules\n\nTier: COMPLEX. Depends on API-01 and contract-owner review.\n",
            ),
            (
                "index.json",
                r#"{"API-01":{"title":"Contract freeze","crit":true,"deps":"—"}}"#,
            ),
        ],
    );
    let cards = read_todos("git", &dir, &head).unwrap();
    assert_eq!(cards.len(), 2);
    let parent = cards
        .iter()
        .find(|card| card.todo.id == "jopedime-api-01")
        .unwrap();
    let child = cards
        .iter()
        .find(|card| card.todo.id == "jopedime-api-01-f01")
        .unwrap();
    assert_eq!(parent.todo.title, "[API-01] Contract freeze");
    assert_eq!(parent.todo.priority, 1);
    assert_eq!(child.todo.blocked_by, ["jopedime-api-01"]);
}

#[test]
fn task_cards_standalone_blocker_receipts_override_claimed_cards() {
    let temp = tempfile::tempdir().unwrap();
    let dir = repo(temp.path());
    let head = commit(
        &dir,
        &[
            ("family.toml", FAMILY),
            ("tasks/T001-API-01-contract.md", CARD),
            (
                "status/API-01.md",
                "# API-01 gate self-review\n\nBLOCKED: owner signature pending\n",
            ),
        ],
    );
    let card = read_todos("git", &dir, &head).unwrap().remove(0);
    assert_eq!(card.todo.status, TodoStatus::Blocked);
    assert!(card.todo.note.contains("owner signature pending"));
}

#[test]
fn task_cards_report_bad_inputs_instead_of_silently_losing_tasks() {
    for (path, input, expected) in [
        ("tasks/unrecognized.md", CARD, "unrecognized filename"),
        ("tasks/API-01-duplicate.md", CARD, "duplicate ID"),
        (
            "tasks/API-02-errors.md",
            "# API-02 — Errors\nDepends on GOV-99\n",
            "has no card",
        ),
        ("index.json", "{broken", "index.json"),
        ("tracking/observations.json", "{broken", "observations"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let dir = repo(temp.path());
        let head = commit(
            &dir,
            &[
                ("family.toml", FAMILY),
                ("tasks/T001-API-01-contract.md", CARD),
                (path, input),
            ],
        );
        let error = read_todos("git", &dir, &head).unwrap_err();
        assert!(error.contains(expected), "{path}: {error}");
    }
}

#[test]
fn task_cards_dated_pr_evidence_keeps_partial_delivery_separate() {
    for (state, expected) in [("handoff", TodoStatus::Handoff), ("done", TodoStatus::Done)] {
        let temp = tempfile::tempdir().unwrap();
        let dir = repo(temp.path());
        let observations = json!({"observed_at":"2026-10-30T12:00:00Z","tasks":[{
            "task":"API-01","status":state,"progress":"skeleton merged; acceptance separately recorded",
            "prs":[{"state":"MERGED","url":"https://github.com/example/product/pull/1","headRefOid":"abc"}]
        }]}).to_string();
        let head = commit(
            &dir,
            &[
                ("family.toml", FAMILY),
                ("tasks/T001-API-01-contract.md", CARD),
                ("tracking/observations.json", &observations),
            ],
        );
        let card = read_todos("git", &dir, &head).unwrap().remove(0);
        assert_eq!(card.todo.status, expected);
        assert_eq!(card.todo.merged, state == "done");
        assert!(card.todo.body.contains("2026-10-30T12:00:00Z"));
        assert!(
            card.todo
                .body
                .contains("Round-trip proof and OWNER approval")
        );
    }
}

#[test]
fn task_cards_reject_symlinks_and_oversized_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let dir = repo(temp.path());
    let huge = "x".repeat(MAX_BLOB + 1);
    let head = commit(
        &dir,
        &[
            ("family.toml", FAMILY),
            ("tasks/T001-API-01-contract.md", &huge),
        ],
    );
    assert!(
        read_todos("git", &dir, &head)
            .unwrap_err()
            .contains("budget")
    );
    std::fs::remove_file(dir.join("tasks/T001-API-01-contract.md")).unwrap();
    std::os::unix::fs::symlink("/outside/secret", dir.join("tasks/T001-API-01-contract.md"))
        .unwrap();
    let head = commit(&dir, &[]);
    assert!(
        read_todos("git", &dir, &head)
            .unwrap_err()
            .contains("symlink")
    );
}

#[test]
#[ignore = "requires an operator-supplied committed queue snapshot"]
fn task_cards_live_queue_snapshot_matches_existing_tracking_ids() {
    let path = std::env::var_os("JERYU_TASK_CARD_FIXTURE_REPO").expect("snapshot repository path");
    let path = Path::new(&path);
    let head = resolve("git", path, "HEAD").unwrap();
    let family = crate::web::shift::queue::show_file("git", path, &head, "family.toml").unwrap();
    let config = crate::web::shift::queue::parse_family_toml(&family, "jopedime").unwrap();
    let native = read_todos("git", path, &head).unwrap();
    let cards = read("git", path, &head, &config).unwrap();
    assert!(!cards.is_empty());
    let original: BTreeSet<_> = native.iter().map(|entry| &entry.todo.id).collect();
    let parsed: BTreeSet<_> = cards.iter().map(|entry| &entry.todo.id).collect();
    assert_eq!(
        original, parsed,
        "direct cards must preserve all published tracking IDs"
    );
    assert!(cards.iter().all(|card| card.path.starts_with("tasks/")));
    assert_eq!(resolve("git", path, "HEAD").unwrap(), head);
    let mut statuses = BTreeMap::new();
    for card in &cards {
        *statuses.entry(card.todo.status.as_str()).or_insert(0) += 1;
    }
    println!(
        "{} committed task cards, stable IDs and dependencies verified: {statuses:?}",
        cards.len()
    );
}

#[tokio::test]
async fn task_cards_routes_list_cards_and_refuse_actions_without_touching_sources() {
    let temp = tempfile::tempdir().unwrap();
    let (router, admin) = crate::web::pipeline::tests::shift_forge(temp.path());
    let work = temp.path().join("work-todo");
    let family = std::fs::read_to_string(work.join("family.toml"))
        .unwrap()
        .replacen("[family]", "[family]\ntask_cards = true", 1);
    commit(
        &work,
        &[
            ("family.toml", &family),
            ("tasks/T001-API-01-contract.md", CARD),
        ],
    );
    let bare = temp.path().join("jeryu/jeryu-todo.git");
    run_git(&work, &["push", "-q", bare.to_str().unwrap(), "queue"]);
    let before = resolve("git", &bare, "refs/heads/queue").unwrap();
    let response = router
        .clone()
        .oneshot(request(
            Method::GET,
            "/api/v1/shift/todos?family=jeryu",
            &admin,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = body_json(response).await;
    assert!(
        response["todos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|todo| todo["id"] == "jeryu-api-01")
    );
    for action in ["edit", "done", "release", "close", "park", "assign"] {
        let response = router
            .clone()
            .oneshot(request(
                Method::POST,
                "/api/v1/shift/todos/jeryu/jeryu-api-01/action",
                &admin,
                Some(json!({"action":action,"value":"alice"})),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT, "{action}");
        assert_eq!(body_json(response).await["code"], "external_task_read_only");
        assert_eq!(resolve("git", &bare, "refs/heads/queue").unwrap(), before);
    }
    let response = router
        .oneshot(request(
            Method::POST,
            "/api/v1/shift/todos/jeryu/jeryu-api-01/comments",
            &admin,
            Some(json!({"body":"External review evidence added"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let after = resolve("git", &bare, "refs/heads/queue").unwrap();
    assert_ne!(after, before);
    assert_eq!(
        run_git(&bare, &["rev-parse", &format!("{before}:tasks")]),
        run_git(&bare, &["rev-parse", &format!("{after}:tasks")])
    );
}
