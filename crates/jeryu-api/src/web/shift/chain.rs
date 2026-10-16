//! Why a todo may not start yet: the dependency chain, in the words `/work`
//! shows beside the todo that waits.
//!
//! `blocked_by` names the todos whose work has to be on the base branch first.
//! A worker applies the same rule when it claims (todoq's `chain` module), so
//! the reason here is the reason a worker is not working it: a dependency
//! clears once it is `done` *and* `merged`, or when it landed on the very
//! shift branch the waiting todo is landing on, which is one review.
//!
//! Only a todo a worker could otherwise pick up carries reasons: `open` and
//! `claimed`. A todo that is done, blocked or parked has its own story, and
//! the attention inbox tells it.

use std::collections::HashMap;

use super::types::{ShiftTodo, TodoStatus};

/// What the chain needs to know about one dependency.
struct Dep<'a> {
    status: TodoStatus,
    merged: bool,
    shift: &'a str,
    claim_by: &'a str,
}

/// Fill `blocked_on` on one family's todos. Call it after the merged truth is
/// enriched, or a dependency that is on main still reads as unmerged.
pub(crate) fn annotate(todos: &mut [ShiftTodo]) {
    let deps: HashMap<String, Dep<'_>> = todos
        .iter()
        .map(|todo| {
            (
                todo.id.clone(),
                Dep {
                    status: todo.status,
                    merged: todo.merged,
                    shift: todo.shift.as_str(),
                    claim_by: todo.claim_by.as_str(),
                },
            )
        })
        .collect();
    let reasons: Vec<Vec<String>> = todos
        .iter()
        .map(|todo| match todo.status {
            TodoStatus::Open | TodoStatus::Claimed => todo
                .blocked_by
                .iter()
                .filter_map(|id| why(id, deps.get(id), &todo.shift))
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    for (todo, reasons) in todos.iter_mut().zip(reasons) {
        todo.blocked_on = reasons;
    }
}

/// One line for a dependency that has not landed; `None` when it has.
fn why(id: &str, dep: Option<&Dep<'_>>, waiting_shift: &str) -> Option<String> {
    let Some(dep) = dep else {
        return Some(format!(
            "blocked on {id}: no todo with that id is in the queue"
        ));
    };
    let same_shift = !dep.shift.is_empty() && dep.shift == waiting_shift;
    Some(match dep.status {
        TodoStatus::Done if dep.merged || same_shift => return None,
        TodoStatus::Done => {
            let shift = if dep.shift.is_empty() {
                "its branch"
            } else {
                dep.shift
            };
            format!("blocked on {id}: it is done on {shift} but not merged to the base branch yet")
        }
        TodoStatus::Blocked => format!("blocked on {id}: it is blocked and needs a person"),
        TodoStatus::Handoff => format!("blocked on {id}: it is handed to a person"),
        TodoStatus::Claimed => {
            let who = if dep.claim_by.is_empty() {
                "a worker"
            } else {
                dep.claim_by
            };
            format!("blocked on {id}: {who} is working on it")
        }
        TodoStatus::Open => format!("blocked on {id}: it is open and has not been worked yet"),
        TodoStatus::Parked => format!("blocked on {id}: it is parked"),
        TodoStatus::Closed => format!("blocked on {id}: it is closed"),
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::super::todo_file::TodoFile;
    use super::*;

    /// Front matter as todoq writes it, so the test reads the fields the
    /// server really parses rather than a hand-built struct.
    fn todo(id: &str, fields: &str) -> ShiftTodo {
        TodoFile::parse(&format!(
            "+++\nid = \"{id}\"\nfamily = \"jeryu\"\n{fields}+++\nwork\n"
        ))
        .expect("parse todo")
        .to_api(Utc::now())
    }

    #[test]
    fn a_dependency_that_is_done_but_unmerged_holds_the_todo() {
        let mut todos = vec![
            todo(
                "dep",
                "status = \"done\"\nshift = \"nightshift/2026-10-14\"\nmerged = false\n",
            ),
            todo(
                "after",
                "status = \"open\"\nblocked_by = [\"dep\"]\nshift = \"\"\n",
            ),
        ];
        annotate(&mut todos);
        assert_eq!(todos[0].blocked_on, Vec::<String>::new());
        assert_eq!(
            todos[1].blocked_on,
            vec![
                "blocked on dep: it is done on nightshift/2026-10-14 but not merged to the base \
                 branch yet"
            ]
        );
        todos[0].merged = true;
        annotate(&mut todos);
        assert_eq!(todos[1].blocked_on, Vec::<String>::new());
    }

    #[test]
    fn a_dependency_on_the_same_shift_is_one_review_and_does_not_hold() {
        let mut todos = vec![
            todo(
                "dep",
                "status = \"done\"\nshift = \"nightshift/2026-10-14\"\nmerged = false\n",
            ),
            todo(
                "after",
                "status = \"claimed\"\nblocked_by = [\"dep\"]\nshift = \"nightshift/2026-10-14\"\n",
            ),
        ];
        annotate(&mut todos);
        assert_eq!(todos[1].blocked_on, Vec::<String>::new());
    }

    #[test]
    fn every_other_state_says_why_and_names_the_id() {
        for (fields, expected) in [
            (
                "status = \"open\"\n",
                "blocked on dep: it is open and has not been worked yet",
            ),
            (
                "status = \"claimed\"\nclaim_by = \"node-0/w1\"\n",
                "blocked on dep: node-0/w1 is working on it",
            ),
            (
                "status = \"blocked\"\n",
                "blocked on dep: it is blocked and needs a person",
            ),
            (
                "status = \"handoff\"\n",
                "blocked on dep: it is handed to a person",
            ),
            ("status = \"parked\"\n", "blocked on dep: it is parked"),
            ("status = \"closed\"\n", "blocked on dep: it is closed"),
        ] {
            let mut todos = vec![
                todo("dep", fields),
                todo("after", "status = \"open\"\nblocked_by = [\"dep\"]\n"),
            ];
            annotate(&mut todos);
            assert_eq!(todos[1].blocked_on, vec![expected.to_string()], "{fields}");
        }
    }

    #[test]
    fn a_dependency_that_is_not_in_the_queue_is_said_not_ignored() {
        let mut todos = vec![todo(
            "after",
            "status = \"open\"\nblocked_by = [\"gone\"]\n",
        )];
        annotate(&mut todos);
        assert_eq!(
            todos[0].blocked_on,
            vec!["blocked on gone: no todo with that id is in the queue"]
        );
    }

    #[test]
    fn a_todo_that_is_not_a_workers_to_pick_up_carries_no_reasons() {
        let mut todos = vec![
            todo("dep", "status = \"open\"\n"),
            todo("after", "status = \"blocked\"\nblocked_by = [\"dep\"]\n"),
            todo("done", "status = \"done\"\nblocked_by = [\"dep\"]\n"),
        ];
        annotate(&mut todos);
        assert_eq!(todos[1].blocked_on, Vec::<String>::new());
        assert_eq!(todos[2].blocked_on, Vec::<String>::new());
    }

    #[tokio::test]
    async fn filtered_todos_route_reads_the_whole_family_when_phrasing_blocked_on() {
        use std::path::Path;

        use axum::http::{Method, StatusCode};
        use jeryu_core::{ForgeCore, UserRole};
        use tower::ServiceExt;

        use super::super::tests::{body_json, request, run_git};
        use crate::web::{WebState, app};

        let dir = tempfile::tempdir().unwrap();
        let owner = dir.path().join("acme");
        let work = dir.path().join("queue-source");
        std::fs::create_dir_all(&owner).unwrap();
        std::fs::create_dir_all(work.join("todos")).unwrap();
        std::fs::write(
            work.join("family.toml"),
            "[family]\nname = \"acme\"\nbase_branch = \"main\"\nlanding = \"shifts\"\nshift_tz = \"UTC\"\n[[repo]]\nname = \"widget\"\norder = 1\n",
        )
        .unwrap();
        std::fs::write(
            work.join("todos/20261001-000000-aaaaaa-dep.md"),
            "+++\nid = \"20261001-000000-aaaaaa\"\nfamily = \"acme\"\ntitle = \"Add endpoint\"\nrepos = [\"widget\"]\nstatus = \"done\"\nshift = \"nightshift/2026-10-15\"\nmerged = false\n+++\nAdd endpoint.\n",
        )
        .unwrap();
        std::fs::write(
            work.join("todos/20261001-000001-bbbbbb-after.md"),
            "+++\nid = \"20261001-000001-bbbbbb\"\nfamily = \"acme\"\ntitle = \"Call endpoint\"\nrepos = [\"widget\"]\nstatus = \"open\"\nblocked_by = [\"20261001-000000-aaaaaa\"]\n+++\nCall endpoint.\n",
        )
        .unwrap();
        run_git(&work, &["init", "-q"]);
        run_git(&work, &["checkout", "-q", "-b", "queue"]);
        run_git(&work, &["add", "."]);
        run_git(&work, &["commit", "-q", "-m", "seed"]);
        run_git(&owner, &["init", "-q", "--bare", "acme-todo.git"]);
        run_git(
            &work,
            &[
                "push",
                "-q",
                owner.join("acme-todo.git").to_str().unwrap(),
                "queue",
            ],
        );
        let core = ForgeCore::new();
        core.create_account("reader", "test-password", UserRole::User)
            .unwrap();
        let token = core
            .create_personal_access_token("reader", "test", None)
            .unwrap()
            .secret;
        let state = WebState::new_with_git_storage(core, dir.path().to_path_buf())
            .with_auth(true, false, false);
        let router = app(state, Path::new("/nonexistent-spa"));
        let response = router
            .oneshot(request(
                Method::GET,
                "/api/v1/shift/todos?family=acme&status=open",
                &token,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let todos = body["todos"].as_array().unwrap();
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0]["id"], "20261001-000001-bbbbbb");
        assert_eq!(
            todos[0]["blocked_on"],
            serde_json::json!([
                "blocked on 20261001-000000-aaaaaa: it is done on nightshift/2026-10-15 but not merged to the base branch yet"
            ])
        );
    }
}
