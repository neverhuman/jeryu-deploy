//! Who may take a todo on, and who may act on it once they have.
//!
//! A todo can carry an `assignee`: the forge login of the person working it.
//! todoq never claims a todo with an assignee, so assigning one takes it out
//! of the worker pool until it is unassigned or released.
//!
//! Administrators may run every todo action. Any other logged-in person may
//! run only these, and only on these terms:
//!
//! - `claim`: assign the todo to themselves, when nobody else has it.
//! - `assign` (`value`: a login): hand it to another person, when nobody has
//!   it or they are the one who has it.
//! - `unassign`: give back a todo they hold.
//! - `done`: finish a todo they hold.
//!
//! A bot, or an account marked as automation (see `web::people`), never takes
//! a todo on: the point of an assignee is that a person has it.

use serde_json::Value;

use super::todo_file::TodoFile;
use super::types::{TodoActionRequest, TodoStatus};

/// The actions that set or clear an assignee.
pub(crate) const ASSIGNMENT_ACTIONS: &[&str] = &["claim", "assign", "unassign"];

/// The caller as the assignment rules see them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Caller<'a> {
    pub login: &'a str,
    pub admin: bool,
    /// A person signed in as themselves: not a bot, not an automation account.
    pub person: bool,
}

/// Why a caller may not run an action; the handler answers 403 with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Denied(pub String);

/// The actions anyone but an administrator may run, on the terms below.
const PEOPLE_ACTIONS: &[&str] = &["claim", "assign", "unassign", "done"];

/// Whether `caller` could run `action` on any todo. Checked before the todo
/// is looked up, so a refusal says nothing about whether it exists.
pub(crate) fn may_attempt(action: &str, caller: Caller<'_>) -> Result<(), Denied> {
    if action == "claim" && !caller.person {
        return Err(Denied(
            "only a person can claim a todo; bots and automation accounts work through the queue"
                .to_string(),
        ));
    }
    if caller.admin || (caller.person && PEOPLE_ACTIONS.contains(&action)) {
        return Ok(());
    }
    if !caller.person {
        return Err(Denied(
            "only a person or an administrator can act on a todo".to_string(),
        ));
    }
    Err(Denied(format!(
        "{action} is for administrators; you can claim, assign, unassign, or finish a todo you hold"
    )))
}

/// Whether `caller` may run `action` on `todo` at all. State rules (a done
/// todo cannot be claimed, say) are `apply`'s business and answer 400.
pub(crate) fn authorize(todo: &TodoFile, action: &str, caller: Caller<'_>) -> Result<(), Denied> {
    let holds = !todo.assignee.is_empty() && todo.assignee == caller.login;
    let free = todo.assignee.is_empty();
    if action == "claim" && !caller.person {
        return Err(Denied(
            "only a person can claim a todo; bots and automation accounts work through the queue"
                .to_string(),
        ));
    }
    if caller.admin {
        return Ok(());
    }
    if !caller.person {
        return Err(Denied(
            "only a person or an administrator can act on a todo".to_string(),
        ));
    }
    let holder = || {
        if todo.assignee.is_empty() {
            "nobody".to_string()
        } else {
            todo.assignee.clone()
        }
    };
    match action {
        "claim" if free || holds => Ok(()),
        "claim" => Err(Denied(format!(
            "{} already has this todo; ask them or an administrator to hand it over",
            todo.assignee
        ))),
        "assign" if free || holds => Ok(()),
        "assign" => Err(Denied(format!(
            "only {} or an administrator can hand this todo to someone else",
            todo.assignee
        ))),
        "unassign" | "done" if holds => Ok(()),
        "unassign" | "done" => Err(Denied(format!(
            "only the person who has this todo can {action} it (it is held by {})",
            holder()
        ))),
        _ => Err(Denied(format!(
            "{action} is for administrators; you can claim, assign, unassign, or finish a todo you hold"
        ))),
    }
}

/// Apply `claim`, `assign` or `unassign`. `is_person` says whether a login is
/// an active person on this forge, the only kind of account a todo can go to.
pub(crate) fn apply(
    todo: &mut TodoFile,
    request: &TodoActionRequest,
    caller: Caller<'_>,
    is_person: impl Fn(&str) -> bool,
) -> Result<(), String> {
    match request.action.as_str() {
        "claim" | "assign" => {
            if matches!(todo.status, TodoStatus::Done | TodoStatus::Closed) {
                return Err(format!(
                    "cannot {} a {} todo",
                    request.action,
                    todo.status.as_str()
                ));
            }
            let target = if request.action == "claim" {
                caller.login.to_string()
            } else {
                request
                    .value
                    .as_ref()
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|login| !login.is_empty())
                    .ok_or("assign needs value: the login of the person to give it to")?
                    .to_string()
            };
            if !is_person(&target) {
                return Err(format!(
                    "{target:?} is not an active person on this forge; pick someone from \
                     GET /api/v1/people"
                ));
            }
            todo.assignee = target;
        }
        "unassign" => {
            if todo.assignee.is_empty() {
                return Err("nobody has this todo".to_string());
            }
            todo.assignee.clear();
        }
        other => return Err(format!("{other} is not an assignment action")),
    }
    if let Some(note) = request
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        todo.note = note.to_string();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(assignee: &str, status: TodoStatus) -> TodoFile {
        let mut todo = TodoFile::new("20260101-000000-aaaaaa".into(), "acme".into(), "t".into());
        todo.assignee = assignee.to_string();
        todo.status = status;
        todo
    }

    fn request(action: &str, value: Option<&str>) -> TodoActionRequest {
        serde_json::from_value(serde_json::json!({ "action": action, "value": value }))
            .expect("request")
    }

    const ADA: Caller<'static> = Caller {
        login: "ada",
        admin: false,
        person: true,
    };
    const BEA: Caller<'static> = Caller {
        login: "bea",
        admin: false,
        person: true,
    };
    const ROOT: Caller<'static> = Caller {
        login: "root",
        admin: true,
        person: true,
    };
    const BUILD_BOT: Caller<'static> = Caller {
        login: "build-bot",
        admin: true,
        person: false,
    };
    const OTHER_USER_BOT: Caller<'static> = Caller {
        login: "ada",
        admin: false,
        person: false,
    };

    #[test]
    fn a_person_claims_a_free_todo_but_not_one_another_user_holds() {
        let free = todo("", TodoStatus::Blocked);
        assert!(authorize(&free, "claim", ADA).is_ok());
        let held = todo("bea", TodoStatus::Blocked);
        let denied = authorize(&held, "claim", ADA).expect_err("another user holds it");
        assert!(denied.0.contains("bea already has"), "{denied:?}");
    }

    #[test]
    fn a_non_admin_is_forbidden_every_admin_action() {
        let mine = todo("ada", TodoStatus::Blocked);
        for action in [
            "release", "block", "close", "park", "edit", "priority", "mode",
        ] {
            assert!(
                may_attempt(action, ADA).is_err(),
                "{action} must be refused before the lookup"
            );
            assert!(
                authorize(&mine, action, ADA).is_err(),
                "{action} must stay admin-only"
            );
        }
    }

    #[test]
    fn only_the_holder_finishes_unassigns_or_delegates() {
        let held = todo("ada", TodoStatus::Blocked);
        for action in ["done", "unassign", "assign"] {
            assert!(authorize(&held, action, ADA).is_ok(), "{action} by holder");
            assert!(
                authorize(&held, action, BEA).is_err(),
                "{action} by another user must be forbidden"
            );
        }
        let free = todo("", TodoStatus::Blocked);
        assert!(
            authorize(&free, "done", ADA).is_err(),
            "done needs a holder"
        );
    }

    #[test]
    fn bots_and_automation_never_take_a_todo() {
        let free = todo("", TodoStatus::Open);
        assert!(authorize(&free, "claim", BUILD_BOT).is_err());
        assert!(authorize(&free, "claim", OTHER_USER_BOT).is_err());
        assert!(authorize(&free, "assign", OTHER_USER_BOT).is_err());
        // An administrator's automation keeps its admin actions.
        assert!(authorize(&free, "assign", BUILD_BOT).is_ok());
        assert!(authorize(&free, "release", BUILD_BOT).is_ok());
    }

    #[test]
    fn an_admin_may_reassign_a_todo_someone_else_holds() {
        let held = todo("bea", TodoStatus::Blocked);
        assert!(authorize(&held, "assign", ROOT).is_ok());
        assert!(authorize(&held, "unassign", ROOT).is_ok());
    }

    #[test]
    fn assign_goes_only_to_active_people() {
        let people = |login: &str| matches!(login, "ada" | "bea");
        let mut free = todo("", TodoStatus::Blocked);
        apply(&mut free, &request("assign", Some("bea")), ADA, people).expect("assign");
        assert_eq!(free.assignee, "bea");
        assert_eq!(
            free.status,
            TodoStatus::Blocked,
            "assigning keeps the status"
        );

        let mut other = todo("", TodoStatus::Blocked);
        let err = apply(
            &mut other,
            &request("assign", Some("gate-bot")),
            ADA,
            people,
        )
        .expect_err("automation is not a person");
        assert!(err.contains("not an active person"), "{err}");
        let err = apply(&mut other, &request("assign", None), ADA, people)
            .expect_err("assign needs a login");
        assert!(err.contains("assign needs value"), "{err}");
    }

    #[test]
    fn claim_sets_the_caller_and_unassign_clears_it() {
        let people = |_: &str| true;
        let mut todo = todo("", TodoStatus::Blocked);
        apply(&mut todo, &request("claim", None), ADA, people).expect("claim");
        assert_eq!(todo.assignee, "ada");
        apply(&mut todo, &request("unassign", None), ADA, people).expect("unassign");
        assert!(todo.assignee.is_empty());
        let err = apply(&mut todo, &request("unassign", None), ADA, people)
            .expect_err("nothing to unassign");
        assert!(err.contains("nobody"), "{err}");
    }

    #[test]
    fn finished_work_cannot_be_claimed() {
        for status in [TodoStatus::Done, TodoStatus::Closed] {
            let mut todo = todo("", status);
            assert!(apply(&mut todo, &request("claim", None), ADA, |_| true).is_err());
        }
    }
}
