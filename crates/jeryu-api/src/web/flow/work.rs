//! What an open ledger visit at Work is now, by the todo's recorded state.
//!
//! The ledger is a fold over events, and two gaps in older folds left Work
//! visits open that no event will ever close:
//!
//! - A slot going idle names the todo it was on (`worker.stage`, `record ->
//!   idle on <todo>`) right after `todo.attempt_finished` closed the visit,
//!   and older binaries read that beat as a fresh claim
//!   (`project::event_steps` no longer does).
//! - Landings projected before every upstream station was left on a landing
//!   kept Queue, Work and Shift open beside the lot's Stage visit.
//!
//! This read overlay, like [`super::closure`], never rewrites the ledger. It
//! answers from what the ledger and the queue record:
//!
//! - a visit upstream of Stage that a landing followed has left (the current
//!   fold would have closed it);
//! - a `done` todo has left Work: it sits at Shift or Review if it still
//!   waits on its shift pull request, and is off the line once merged;
//! - a `blocked`, `handoff` or `parked` todo is held at Work;
//! - an `open` todo, or a `claimed` one whose lease has lapsed, is waiting;
//! - a pull request no todo carries is not Work's: no worker claims it. Older
//!   binaries sent it to Work on a red gate, a held review or a refused queue
//!   replay; today's fold holds it where it stopped, so it is shown held at
//!   the station whose rework arc sent it, or left out when the lot already
//!   sits at another station.
//!
//! A todo the queue does not answer for keeps its recorded state.

use serde::Serialize;

use super::closure::OpenVisit;
use super::stations::Station;
use crate::web::shift::{ShiftTodo, TodoStatus};

/// Why a visit is no longer current.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    Landed,
    Done,
    PullRequestElsewhere,
    PullRequestNotWork,
}

/// What an open visit is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reading {
    /// As the ledger records it.
    Current,
    /// No longer at the station.
    Leaves(Why),
    /// Held at its station: a person must act.
    Held,
    /// Waiting at its station for a worker slot.
    Waiting,
    /// A pull request no todo carries, held at the station that stopped it.
    HeldAt(Station),
}

/// The visits the overlay changed, by why (`work_reconciled` of
/// `GET /api/v1/flow`). A visit already in the state it is read as is not
/// counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Reconciled {
    /// Open visits upstream of Stage, at any station, that a landing followed.
    pub landed_left: u32,
    /// Work visits of todos the queue records `done`.
    pub done_left: u32,
    /// Work visits of `blocked`, `handoff` or `parked` todos now held.
    pub blocked_held: u32,
    /// Work visits of `open` or lease-lapsed todos now waiting.
    pub open_waiting: u32,
    /// Work visits of pull requests no todo carries, held where they stopped.
    pub pull_requests_held_where_stopped: u32,
    /// Work visits of pull requests no todo carries that sit elsewhere too,
    /// or have no station to go back to.
    pub pull_requests_left: u32,
}

impl Reconciled {
    pub(crate) fn count(&mut self, reading: Reading) {
        let slot = match reading {
            Reading::Current => return,
            Reading::Leaves(Why::Landed) => &mut self.landed_left,
            Reading::Leaves(Why::Done) => &mut self.done_left,
            Reading::Leaves(Why::PullRequestElsewhere | Why::PullRequestNotWork) => {
                &mut self.pull_requests_left
            }
            Reading::Held => &mut self.blocked_held,
            Reading::Waiting => &mut self.open_waiting,
            Reading::HeldAt(_) => &mut self.pull_requests_held_where_stopped,
        };
        *slot += 1;
    }
}

/// The reading of one open visit. `todo` is the queue's record for the lot
/// when it is a todo the queue answers for; `elsewhere` says the lot has
/// another open visit.
pub(crate) fn read(visit: &OpenVisit, todo: Option<&ShiftTodo>, elsewhere: bool) -> Reading {
    let upstream = matches!(
        visit.station.as_str(),
        "queue" | "work" | "shift" | "review" | "gate" | "merge"
    );
    if upstream && visit.landed {
        return Reading::Leaves(Why::Landed);
    }
    if visit.station != Station::Work.id() {
        return Reading::Current;
    }
    if !visit.todo {
        // Only a pull request is not Work's; a row that names neither a
        // todo nor a pull request keeps its recorded state.
        if visit.pr.is_none() {
            return Reading::Current;
        }
        if elsewhere {
            return Reading::Leaves(Why::PullRequestElsewhere);
        }
        return match visit.back_from.as_deref().and_then(Station::parse) {
            Some(station) if station != Station::Work => Reading::HeldAt(station),
            _ => Reading::Leaves(Why::PullRequestNotWork),
        };
    }
    let Some(todo) = todo else {
        return Reading::Current;
    };
    let waiting = || {
        if visit.started || visit.held {
            Reading::Waiting
        } else {
            Reading::Current
        }
    };
    match todo.status {
        TodoStatus::Done => Reading::Leaves(Why::Done),
        TodoStatus::Blocked | TodoStatus::Handoff | TodoStatus::Parked => {
            if visit.held {
                Reading::Current
            } else {
                Reading::Held
            }
        }
        TodoStatus::Open => waiting(),
        TodoStatus::Claimed if !todo.lease_live => waiting(),
        TodoStatus::Claimed | TodoStatus::Closed => Reading::Current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An open visit shaped like the live rows: a todo lot with no pull
    /// request, opened by a beat, started.
    fn at(station: &str) -> OpenVisit {
        OpenVisit {
            lot: "20261026-204743-068333".into(),
            station: station.into(),
            entered: 1_793_050_688_699,
            repo: None,
            pr: None,
            family: Some("acme".into()),
            started: true,
            held: false,
            todo: true,
            landed: false,
            back_from: None,
        }
    }

    fn todo(status: TodoStatus, merged: bool, lease_live: bool) -> ShiftTodo {
        let mut file = crate::web::shift::todo_file::TodoFile::new(
            "20261026-204743-068333".into(),
            "acme".into(),
            "proof-evidence lane".into(),
        );
        file.status = status;
        file.merged = merged;
        let now = chrono::Utc::now();
        if lease_live {
            file.claim_by = "operator/w1".into();
            file.lease_until = (now + chrono::Duration::hours(1)).to_rfc3339();
        }
        let api = file.to_api(now);
        assert_eq!(api.lease_live, lease_live);
        api
    }

    #[test]
    fn a_done_todo_leaves_work_merged_or_waiting_on_its_shift() {
        for merged in [true, false] {
            assert_eq!(
                read(
                    &at("work"),
                    Some(&todo(TodoStatus::Done, merged, false)),
                    true
                ),
                Reading::Leaves(Why::Done)
            );
        }
        // Held before it was finished: done still wins.
        let mut held = at("work");
        held.held = true;
        assert_eq!(
            read(&held, Some(&todo(TodoStatus::Done, false, false)), true),
            Reading::Leaves(Why::Done)
        );
        // A closed todo is the closure overlay's; its raw merged flag is no
        // proof the visit's work landed.
        assert_eq!(
            read(
                &at("work"),
                Some(&todo(TodoStatus::Closed, true, false)),
                false
            ),
            Reading::Current
        );
        // Done says nothing about Shift: the shift pull request moves it.
        assert_eq!(
            read(
                &at("shift"),
                Some(&todo(TodoStatus::Done, false, false)),
                true
            ),
            Reading::Current
        );
    }

    #[test]
    fn a_landing_that_followed_the_visit_closes_every_upstream_station() {
        for station in ["queue", "work", "shift", "review", "gate", "merge"] {
            let mut visit = at(station);
            visit.landed = true;
            assert_eq!(
                read(&visit, None, true),
                Reading::Leaves(Why::Landed),
                "{station}"
            );
        }
        let mut stage = at("stage");
        stage.landed = true;
        assert_eq!(read(&stage, None, true), Reading::Current);
    }

    #[test]
    fn blocked_is_held_and_open_is_waiting() {
        for status in [TodoStatus::Blocked, TodoStatus::Handoff, TodoStatus::Parked] {
            assert_eq!(
                read(&at("work"), Some(&todo(status, false, false)), true),
                Reading::Held
            );
        }
        let mut held = at("work");
        held.held = true;
        assert_eq!(
            read(&held, Some(&todo(TodoStatus::Blocked, false, false)), true),
            Reading::Current,
            "an already held visit is not counted again"
        );
        assert_eq!(
            read(
                &at("work"),
                Some(&todo(TodoStatus::Open, false, false)),
                true
            ),
            Reading::Waiting
        );
        assert_eq!(
            read(&held, Some(&todo(TodoStatus::Open, false, false)), true),
            Reading::Waiting,
            "a released todo waits again"
        );
        assert_eq!(
            read(
                &at("work"),
                Some(&todo(TodoStatus::Claimed, false, false)),
                true
            ),
            Reading::Waiting,
            "a lapsed lease is no claim"
        );
        assert_eq!(
            read(
                &at("work"),
                Some(&todo(TodoStatus::Claimed, false, true)),
                true
            ),
            Reading::Current,
            "a live lease is work in progress"
        );
        assert_eq!(read(&at("work"), None, true), Reading::Current);
    }

    #[test]
    fn a_pull_request_without_a_todo_is_held_where_it_stopped() {
        let mut visit = at("work");
        visit.lot = "acme/widget#5".into();
        visit.repo = Some("acme/widget".into());
        visit.pr = Some(5);
        visit.todo = false;
        visit.started = false;
        visit.back_from = Some("gate".into());
        assert_eq!(read(&visit, None, false), Reading::HeldAt(Station::Gate));
        assert_eq!(
            read(&visit, None, true),
            Reading::Leaves(Why::PullRequestElsewhere)
        );
        visit.back_from = None;
        assert_eq!(
            read(&visit, None, false),
            Reading::Leaves(Why::PullRequestNotWork)
        );
        // A row naming neither a todo nor a pull request is left as recorded.
        visit.pr = None;
        visit.repo = None;
        visit.back_from = Some("gate".into());
        assert_eq!(read(&visit, None, true), Reading::Current);
    }
}
