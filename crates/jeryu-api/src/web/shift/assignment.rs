//! Who may take a todo on, and who may act on it once they have.
//!
//! A todo can carry an `assignee`: the forge login of the person working it.
//! todoq never claims a todo with an assignee, so assigning one takes it out
//! of the worker pool until it is unassigned or released.
//!
//! Administrators may run every todo action. Any other person may run only
//! these, only on a todo whose code they can write to (see `access.rs`: an
//! account that merely signed up, or cannot read a private family, gets
//! nothing), and only on these terms:
//!
//! - `claim`: assign the todo to themselves, when nobody else has it.
//! - `assign` (`value`: a login): hand it to another person who can also work
//!   on it, when nobody has it or they are the one who has it.
//! - `unassign`: give back a todo they hold.
//! - `done`: finish a todo they hold.
//!
//! A bot, or an account marked as automation (see `web::people`), never takes
//! a todo on: the point of an assignee is that a person has it.
//!
//! A person who is not an administrator takes on or finishes only `blocked`
//! and `handoff` todos (`PEOPLE_STATUSES`), which no worker claims, so the
//! rule holds whichever todoq version a worker host runs. todoq additionally
//! skips any todo with an assignee (`Todo.is_claimable`), which keeps an
//! administrator's assignment of open work out of the pool once it ships.

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

/// What the handler found out about the caller and the todo's holder.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Standing {
    /// The caller can write to the todo's code (see `access.rs`).
    pub works_here: bool,
    /// The stored assignee is still an active person. One who was
    /// deactivated or marked as automation holds nothing: the todo is free.
    pub holder_active: bool,
}

/// The statuses a person who is not an administrator may take on or finish:
/// work that stopped for a person. No worker claims these (todoq claims only
/// `open` and lapsed `claimed` todos), so a person and a worker never meet on
/// one even before todoq skips assigned todos; and a todo an administrator
/// parked or closed stays theirs, since `done` cannot be undone.
const PEOPLE_STATUSES: &[TodoStatus] = &[TodoStatus::Blocked, TodoStatus::Handoff];

/// Whether `caller` may run `action` on `todo` at all. State rules (a done
/// todo cannot be claimed, say) are `apply`'s business and answer 422.
pub(crate) fn authorize(
    todo: &TodoFile,
    action: &str,
    caller: Caller<'_>,
    standing: Standing,
) -> Result<(), Denied> {
    let held = !todo.assignee.is_empty() && standing.holder_active;
    let holds = held && todo.assignee == caller.login;
    let free = !held;
    let works_here = standing.works_here;
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
    if !works_here {
        return Err(Denied(
            "you need write access to this todo's code to take it on; ask an administrator \
             for a grant"
                .to_string(),
        ));
    }
    let holder = || {
        if !held {
            "nobody".to_string()
        } else {
            todo.assignee.clone()
        }
    };
    if matches!(action, "claim" | "assign" | "done") && !PEOPLE_STATUSES.contains(&todo.status) {
        return Err(Denied(format!(
            "a {} todo is not waiting on a person; only blocked or handed-off todos can be \
             taken on or finished here, the rest belong to the workers and administrators",
            todo.status.as_str()
        )));
    }
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

/// Apply `claim`, `assign` or `unassign`. `assignable` answers the canonical
/// login of an active person who can work on this todo, the only kind of
/// account it can go to, or `None`.
pub(crate) fn apply(
    todo: &mut TodoFile,
    request: &TodoActionRequest,
    caller: Caller<'_>,
    assignable: impl Fn(&str) -> Option<String>,
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
            let Some(login) = assignable(&target) else {
                return Err(format!(
                    "{target:?} is not an active person who can work on this todo; pick \
                     someone from GET /api/v1/shift/todos/{}/{}/people",
                    todo.family, todo.id
                ));
            };
            // The stored login is the account store's, never the request's
            // spelling of it, so the holder always matches their own login.
            todo.assignee = login;
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

    const HERE: Standing = Standing {
        works_here: true,
        holder_active: true,
    };
    const ELSEWHERE: Standing = Standing {
        works_here: false,
        holder_active: true,
    };

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
        assert!(authorize(&free, "claim", ADA, HERE).is_ok());
        let held = todo("bea", TodoStatus::Blocked);
        let denied = authorize(&held, "claim", ADA, HERE).expect_err("another user holds it");
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
                authorize(&mine, action, ADA, HERE).is_err(),
                "{action} must stay admin-only"
            );
        }
    }

    #[test]
    fn only_the_holder_finishes_unassigns_or_delegates() {
        let held = todo("ada", TodoStatus::Blocked);
        for action in ["done", "unassign", "assign"] {
            assert!(
                authorize(&held, action, ADA, HERE).is_ok(),
                "{action} by holder"
            );
            assert!(
                authorize(&held, action, BEA, HERE).is_err(),
                "{action} by another user must be forbidden"
            );
        }
        let free = todo("", TodoStatus::Blocked);
        assert!(
            authorize(&free, "done", ADA, HERE).is_err(),
            "done needs a holder"
        );
    }

    #[test]
    fn bots_and_automation_never_take_a_todo() {
        let free = todo("", TodoStatus::Open);
        assert!(authorize(&free, "claim", BUILD_BOT, HERE).is_err());
        assert!(authorize(&free, "claim", OTHER_USER_BOT, HERE).is_err());
        assert!(authorize(&free, "assign", OTHER_USER_BOT, HERE).is_err());
        // An administrator's automation keeps its admin actions.
        assert!(authorize(&free, "assign", BUILD_BOT, HERE).is_ok());
        assert!(authorize(&free, "release", BUILD_BOT, HERE).is_ok());
    }

    #[test]
    fn an_admin_may_reassign_a_todo_someone_else_holds() {
        let held = todo("bea", TodoStatus::Blocked);
        assert!(authorize(&held, "assign", ROOT, HERE).is_ok());
        assert!(authorize(&held, "unassign", ROOT, HERE).is_ok());
    }

    #[test]
    fn assign_goes_only_to_active_people() {
        let people = |login: &str| matches!(login, "ada" | "bea").then(|| login.to_string());
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
        let people = |login: &str| Some(login.to_string());
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
    fn a_person_without_write_access_to_the_code_gets_nothing() {
        // An account that only signed up, or cannot read a private family.
        let free = todo("", TodoStatus::Blocked);
        for action in ["claim", "assign", "done", "unassign"] {
            let denied = authorize(&free, action, ADA, ELSEWHERE).expect_err(action);
            assert!(denied.0.contains("write access"), "{denied:?}");
        }
        // Administrators are not limited by grants.
        assert!(authorize(&free, "claim", ROOT, ELSEWHERE).is_ok());
    }

    #[test]
    fn a_person_takes_on_or_finishes_only_work_that_stopped_for_a_person() {
        // Parked, closed, open and claimed todos belong to admins and workers;
        // done cannot be undone, so a person never finishes them.
        for status in [
            TodoStatus::Parked,
            TodoStatus::Closed,
            TodoStatus::Open,
            TodoStatus::Claimed,
        ] {
            let mine = todo("ada", status);
            let free = todo("", status);
            assert!(
                authorize(&free, "claim", ADA, HERE).is_err(),
                "claim {status:?}"
            );
            assert!(
                authorize(&free, "assign", ADA, HERE).is_err(),
                "assign {status:?}"
            );
            assert!(
                authorize(&mine, "done", ADA, HERE).is_err(),
                "done {status:?}"
            );
            // Giving a todo back is always allowed to its holder.
            assert!(
                authorize(&mine, "unassign", ADA, HERE).is_ok(),
                "unassign {status:?}"
            );
            assert!(
                authorize(&free, "claim", ROOT, HERE).is_ok(),
                "admin {status:?}"
            );
        }
        let handoff = todo("", TodoStatus::Handoff);
        assert!(authorize(&handoff, "claim", ADA, HERE).is_ok());
    }

    #[test]
    fn a_holder_who_is_no_longer_an_active_person_holds_nothing() {
        let stale = todo("gone", TodoStatus::Blocked);
        let lapsed = Standing {
            works_here: true,
            holder_active: false,
        };
        assert!(authorize(&stale, "claim", ADA, lapsed).is_ok());
        assert!(authorize(&stale, "claim", ADA, HERE).is_err());
    }

    #[test]
    fn the_stored_assignee_is_the_canonical_login() {
        let canonical =
            |login: &str| (login.eq_ignore_ascii_case("bea")).then(|| "bea".to_string());
        let mut free = todo("", TodoStatus::Blocked);
        apply(&mut free, &request("assign", Some(" BEA ")), ADA, canonical).expect("assign");
        assert_eq!(free.assignee, "bea");
    }

    #[test]
    fn finished_work_cannot_be_claimed() {
        for status in [TodoStatus::Done, TodoStatus::Closed] {
            let mut todo = todo("", status);
            assert!(
                apply(&mut todo, &request("claim", None), ADA, |l| Some(
                    l.to_string()
                ))
                .is_err()
            );
        }
    }
}
