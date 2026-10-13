//! The flow projection: pipeline events in, `flow_ledger` visits out.
//!
//! The projection is a fold over the event sequence. [`steps`] is the pure
//! half — what one event means for the route, decided from its kind and
//! outcome alone — and [`apply`] is the half that writes, opening, starting,
//! closing and reopening the one row per lot per station visit that
//! `db/migrations/0011_flow_ledger.sql` describes. `EventStore::insert` calls
//! it in the same transaction as the event, so an event is never in the log
//! without its visits; [`replay`] runs the same fold over every event the log
//! still holds, which is how the ledger is built on migration and rebuilt
//! after a projection bug.
//!
//! Nothing here reads the clock or the request: the same events in the same
//! order always give the same rows.

use rusqlite::{Connection, OptionalExtension, params};

use super::stations::Station;

/// What one event does to one station visit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The lot arrived at the station (a new visit, unless one is open).
    Enter(Station),
    /// The station's tool picked the lot up.
    Start(Station),
    /// The lot left forward, down the route.
    Leave(Station),
    /// The lot left backwards, to an earlier station (or round the same one),
    /// which it enters again as a new visit.
    Rework(Station, Station),
    /// The lot cannot move until a person acts.
    Hold(Station),
    /// A person acted and the lot can move again.
    Release(Station),
}

/// What `kind` with `outcome` means for the route.
///
/// The mapping is the "Enters when / Starts when / Leaves when / Rework back
/// to" table of `docs/flow.md`, read off the event kinds in
/// `docs/pipeline-events.md`. An event that says nothing about the route
/// (`pr.skipped`, `gate.log` of a pass still running) maps to no step, and a
/// step for a station the lot is not at is dropped by [`apply`].
pub(crate) fn steps(kind: &str, outcome: Option<&str>) -> Vec<Step> {
    use Station::{Deploy, Gate, Merge, Pin, Queue, Review, Shift, Stage, Work};
    use Step::{Enter, Hold, Leave, Release, Rework, Start};
    let outcome = outcome.unwrap_or_default();
    match kind {
        "todo.filed" => vec![Enter(Queue)],
        // A claim is the whole of Queue: the lot waited for a worker slot and
        // the slot has it now.
        "todo.claimed" => vec![Enter(Queue), Start(Queue), Leave(Queue), Enter(Work)],
        "worker.stage" => vec![Enter(Work), Start(Work)],
        "todo.attempt_finished" => match outcome {
            "done" => vec![Leave(Work), Enter(Shift)],
            // A retried, rate-limited, killed or retargeted attempt puts the
            // todo back in the queue for another slot.
            "retry" | "killed" | "ratelimit" | "retarget" => vec![Rework(Work, Queue)],
            "blocked" | "handoff" | "needs_human" => vec![Hold(Work)],
            _ => Vec::new(),
        },
        // Releasing a held todo from the Work page lets it move again.
        "todo.action" => match outcome {
            "released" | "unblocked" => vec![Release(Work)],
            "blocked" | "parked" => vec![Hold(Work)],
            _ => Vec::new(),
        },
        // The shift's pull request is open: the work on that branch is now
        // waiting for a reviewer and for its required contexts. Gate is
        // entered here because nothing records a head joining the gate queue
        // (`docs/flow.md`, "Known gaps").
        "shift.pr_opened" | "pr.opened" | "pr.ready_for_review" => {
            vec![Leave(Shift), Enter(Review), Enter(Gate)]
        }
        "review.started" => vec![Enter(Review), Start(Review)],
        "review.finished" | "pr.review" | "pr.reviewed" => match outcome {
            "approve" | "approved" => vec![Leave(Review)],
            // A reviewer that holds, asks for changes or refuses to publish
            // sends the work back to the worker.
            "hold" | "request_changes" | "failed" | "publication_rejected" | "too_large" => {
                vec![Rework(Review, Work)]
            }
            _ => Vec::new(),
        },
        "pr.approved" => vec![Leave(Review)],
        "gate.started" => vec![Enter(Gate), Start(Gate)],
        "gate.finished" | "gate.log" => match outcome {
            "success" => vec![Leave(Gate)],
            "failure" | "error" | "timed_out" | "inputs_changed" => vec![Rework(Gate, Work)],
            _ => Vec::new(),
        },
        "queue.enqueued" => vec![Enter(Merge), Start(Merge)],
        // A rebuild sends the head back through the gate.
        "queue.building" => vec![Rework(Merge, Gate)],
        // A replay that failed or could not be replayed at all needs the
        // branch worked on again.
        "queue.failed" | "queue.refused" => vec![Rework(Merge, Work)],
        "queue.landed" | "pr.merged" | "todo.merged" => vec![Leave(Merge), Enter(Stage)],
        "pin.bump_opened" => vec![Enter(Pin), Start(Pin)],
        // A failed bump is a second visit to Pin, so the loop is countable.
        "pin.bump_failed" => vec![Rework(Pin, Pin)],
        // The staged release carries the work: the pin, if there was one,
        // reached it.
        "release.staged" => vec![Leave(Pin), Leave(Stage), Enter(Deploy)],
        "release.stage_failed" => vec![Rework(Stage, Stage)],
        "deploy.created" => vec![Enter(Deploy), Start(Deploy)],
        "deploy.status" => match outcome {
            "success" => vec![Leave(Deploy)],
            "failure" | "error" => vec![Rework(Deploy, Stage)],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The event fields the projection reads. Built from a `NewEvent` on the
/// insert path and from a stored row on the replay path, so both folds see
/// exactly the same thing.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EventView<'a> {
    pub seq: i64,
    /// Who posted the event and the id it gave it: together the key that
    /// makes the projection count an event once, however often it is posted.
    pub reporter: &'a str,
    pub event_id: Option<&'a str>,
    pub ts_ms: i64,
    pub kind: &'a str,
    pub outcome: Option<&'a str>,
    pub family: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub pr: Option<i64>,
    pub todo_id: Option<&'a str>,
    pub shift: Option<&'a str>,
    pub sha: Option<&'a str>,
    pub summary: Option<&'a str>,
    /// The producer's `detail` object, which the outcome ledger reads
    /// (`web/outcomes/project.rs`); the flow fold ignores it.
    pub detail: Option<&'a serde_json::Value>,
}

/// Project one event into `flow_ledger` on `conn`, which the caller has in a
/// transaction with the event's own insert.
pub(crate) fn apply(conn: &Connection, event: &EventView<'_>) -> Result<(), String> {
    let steps = steps(event.kind, event.outcome);
    if steps.is_empty() {
        return Ok(());
    }
    if !first_projection(conn, event)? {
        return Ok(());
    }
    for (lot, joined) in lots_for(conn, event)? {
        // A lot that joins an open pull request's new head leaves Shift and
        // waits for review of that head, as it would have had it been on the
        // branch when the pull request opened.
        if joined && !steps.contains(&Step::Leave(Station::Shift)) {
            for step in [Step::Leave(Station::Shift), Step::Enter(Station::Review)] {
                apply_step(conn, event, &lot, step)?;
            }
        }
        for step in &steps {
            apply_step(conn, event, &lot, *step)?;
        }
        // Merge is entered when the head is approved and green, which is two
        // events apart: whichever of Review and Gate finishes second opens it.
        if steps.iter().any(|step| {
            matches!(
                step,
                Step::Leave(Station::Review) | Step::Leave(Station::Gate)
            )
        }) && left_forward(conn, &lot.id, Station::Review)?
            && left_forward(conn, &lot.id, Station::Gate)?
            && open_visit(conn, &lot.id, Station::Merge)?.is_none()
            && !left_forward(conn, &lot.id, Station::Merge)?
        {
            enter(conn, event, &lot, Station::Merge)?;
        }
    }
    Ok(())
}

/// Whether this is the first time the event reaches the projection.
///
/// The event log refuses a repeated `(reporter, event_id)` only while it still
/// holds the first copy; after the 30-day prune the same event is stored again
/// under a new `seq`. `flow_ledger_events` is never pruned, so an event the
/// ledger has already folded in is skipped, the way `cost_ledger`'s unique key
/// counts a re-posted cost once. An event without an `event_id` cannot be
/// recognised again and is always projected.
fn first_projection(conn: &Connection, event: &EventView<'_>) -> Result<bool, String> {
    let Some(event_id) = event.event_id.filter(|id| !id.is_empty()) else {
        return Ok(true);
    };
    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO flow_ledger_events (reporter, event_id, seq)
             VALUES (?1, ?2, ?3)",
            params![event.reporter, event_id, event.seq],
        )
        .map_err(|err| err.to_string())?;
    Ok(inserted == 1)
}

/// Every event the log still holds, projected into an empty ledger, oldest
/// first. Answers how many events were read and how many visits they opened.
/// The caller records the replay so it happens exactly once.
pub(crate) fn replay(conn: &Connection) -> Result<(i64, i64), String> {
    let read = for_each_event(conn, |event| apply(conn, event))?;
    let visits = conn
        .query_row("SELECT COUNT(*) FROM flow_ledger", [], |row| row.get(0))
        .map_err(|err| err.to_string())?;
    Ok((read, visits))
}

/// Call `fold` with every event the log still holds, oldest first, and answer
/// how many there were. The one reader both projections replay from.
pub(crate) fn for_each_event(
    conn: &Connection,
    mut fold: impl FnMut(&EventView<'_>) -> Result<(), String>,
) -> Result<i64, String> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, ts_ms, kind, outcome, family, repo, pr, todo_id, shift,
                    reporter, event_id, sha, detail_json, summary
               FROM pipeline_events ORDER BY seq",
        )
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<String>>(13)?,
            ))
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    drop(stmt);
    let mut read = 0;
    for (
        seq,
        ts_ms,
        kind,
        outcome,
        family,
        repo,
        pr,
        todo_id,
        shift,
        reporter,
        event_id,
        sha,
        detail,
        summary,
    ) in &rows
    {
        read += 1;
        let detail: Option<serde_json::Value> = detail
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok());
        fold(&EventView {
            seq: *seq,
            reporter,
            event_id: event_id.as_deref(),
            ts_ms: *ts_ms,
            kind,
            outcome: outcome.as_deref(),
            family: family.as_deref(),
            repo: repo.as_deref(),
            pr: *pr,
            todo_id: todo_id.as_deref(),
            shift: shift.as_deref(),
            sha: sha.as_deref(),
            summary: summary.as_deref(),
            detail: detail.as_ref(),
        })?;
    }
    Ok(read)
}

/// One lot an event speaks about.
#[derive(Clone, Debug)]
struct Lot {
    id: String,
    repo: Option<String>,
    pr: Option<i64>,
    todo_id: Option<String>,
}

/// The lots one event moves.
///
/// A todo is its own lot, and a pull request that carries no todo is
/// `<owner>/<repo>#<pr>`. An event that names both joins them: the pull
/// request's visits are rewritten onto the todo's lot, which is the same
/// `Todo:` trailer join `GET /api/v1/trace` makes. A shift pull request names
/// no todo at all, so it moves every lot whose work landed on that shift
/// branch *of the same repository* (and family, when the event names one)
/// and is still at Shift. Shift branch names are shared by every repository
/// and family, so the branch alone is never the join.
/// Event kinds that bring a head to a shift pull request, and so may carry
/// the lots waiting at Shift onto it.
fn sweeps_shift(kind: &str) -> bool {
    matches!(
        kind,
        "shift.pr_opened" | "pr.opened" | "pr.ready_for_review" | "gate.started"
    )
}

/// Each lot an event moves, and whether this event swept it off Shift.
fn lots_for(conn: &Connection, event: &EventView<'_>) -> Result<Vec<(Lot, bool)>, String> {
    if let Some(todo_id) = event.todo_id.map(str::trim).filter(|id| !id.is_empty()) {
        let lot = Lot {
            id: todo_id.to_string(),
            repo: event.repo.map(ToString::to_string),
            pr: event.pr,
            todo_id: Some(todo_id.to_string()),
        };
        if let (Some(repo), Some(pr)) = (event.repo, event.pr) {
            adopt(conn, todo_id, repo, pr)?;
        }
        return Ok(vec![(lot, false)]);
    }
    if let (Some(repo), Some(pr)) = (event.repo, event.pr) {
        // A shift pull request is the work of the lots on its branch. Only an
        // event that brings a head (the pull request opening, or a head
        // joining the gate) sweeps the lots waiting at Shift onto it; a later
        // verdict on an older head must not move a lot that head never held.
        let mut swept = Vec::new();
        if sweeps_shift(event.kind)
            && let Some(shift) = event.shift.map(str::trim).filter(|s| !s.is_empty())
        {
            swept = open_at_shift(conn, shift, repo, event.family)?;
            for lot in &swept {
                bind_pr(conn, &lot.id, repo, pr)?;
            }
        }
        // Every lot the pull request already carries: one for an ordinary
        // pull request, every lot its shift swept for a shift pull request.
        // The sweep is added to them, never in place of them.
        let mut ids = lots_of_pr(conn, repo, pr)?;
        for lot in &swept {
            if !ids.contains(&lot.id) {
                ids.push(lot.id.clone());
            }
        }
        if ids.is_empty() {
            ids.push(format!("{repo}#{pr}"));
        }
        let mut lots = Vec::with_capacity(ids.len());
        for id in ids {
            let joined = swept.iter().any(|lot| lot.id == id);
            let todo_id = conn
                .query_row(
                    "SELECT todo_id FROM flow_ledger WHERE lot_id = ?1 AND todo_id IS NOT NULL
                      ORDER BY seq LIMIT 1",
                    params![id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|err| err.to_string())?;
            lots.push((
                Lot {
                    id,
                    repo: Some(repo.to_string()),
                    pr: Some(pr),
                    todo_id,
                },
                joined,
            ));
        }
        return Ok(lots);
    }
    Ok(Vec::new())
}

/// The lots the visits of `repo#pr` belong to, in the order they first
/// named it.
pub(crate) fn lots_of_pr(conn: &Connection, repo: &str, pr: i64) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT lot_id FROM flow_ledger WHERE repo = ?1 AND pr = ?2
              GROUP BY lot_id ORDER BY MIN(seq)",
        )
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map(params![repo, pr], |row| row.get::<_, String>(0))
        .map_err(|err| err.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())
}

/// Rewrite the visits of a pull-request-only lot onto the todo that carries
/// it, and give the todo's own visits the pull request.
fn adopt(conn: &Connection, todo_id: &str, repo: &str, pr: i64) -> Result<(), String> {
    let pr_lot = format!("{repo}#{pr}");
    if pr_lot != todo_id {
        conn.execute(
            "UPDATE flow_ledger SET lot_id = ?1, todo_id = ?1 WHERE lot_id = ?2",
            params![todo_id, pr_lot],
        )
        .map_err(|err| err.to_string())?;
    }
    bind_pr(conn, todo_id, repo, pr)
}

/// Name the pull request on every visit of a lot that has none yet. A visit
/// that already names another repository keeps it: a pull request never
/// relabels work recorded against a different repository.
fn bind_pr(conn: &Connection, lot_id: &str, repo: &str, pr: i64) -> Result<(), String> {
    conn.execute(
        "UPDATE flow_ledger SET repo = ?2, pr = ?3
          WHERE lot_id = ?1 AND pr IS NULL AND (repo IS NULL OR repo = ?2)",
        params![lot_id, repo, pr],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

/// The lots of `repo` whose work landed on `shift` and is still waiting at
/// Shift. A lot that recorded no repository is never swept: the branch name
/// alone cannot say which repository's pull request carries it. When the
/// event names a family the lot must be of that family too.
fn open_at_shift(
    conn: &Connection,
    shift: &str,
    repo: &str,
    family: Option<&str>,
) -> Result<Vec<Lot>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT lot_id, repo, pr, todo_id FROM flow_ledger
              WHERE shift = ?1 AND station = 'shift' AND left_ms IS NULL
                AND repo = ?2 AND (?3 IS NULL OR family = ?3)
              ORDER BY seq",
        )
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map(params![shift, repo, family], |row| {
            Ok(Lot {
                id: row.get(0)?,
                repo: row.get(1)?,
                pr: row.get(2)?,
                todo_id: row.get(3)?,
            })
        })
        .map_err(|err| err.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())
}

fn apply_step(
    conn: &Connection,
    event: &EventView<'_>,
    lot: &Lot,
    step: Step,
) -> Result<(), String> {
    match step {
        Step::Enter(station) => enter(conn, event, lot, station),
        Step::Start(station) => {
            if open_visit(conn, &lot.id, station)?.is_none() {
                enter(conn, event, lot, station)?;
            }
            conn.execute(
                "UPDATE flow_ledger SET started_ms = ?3, source_seq_last = ?4
                  WHERE lot_id = ?1 AND station = ?2 AND left_ms IS NULL
                    AND started_ms IS NULL",
                params![lot.id, station.id(), event.ts_ms, event.seq],
            )
            .map_err(|err| err.to_string())?;
            Ok(())
        }
        Step::Leave(station) => close(conn, event, lot, station, "forward", None),
        Step::Rework(station, back_to) => {
            // Nothing to send back if the lot was never here.
            if open_visit(conn, &lot.id, station)?.is_none() {
                return Ok(());
            }
            close(conn, event, lot, station, "rework", Some(back_to))?;
            enter(conn, event, lot, back_to)
        }
        Step::Hold(station) => {
            if open_visit(conn, &lot.id, station)?.is_none() {
                enter(conn, event, lot, station)?;
            }
            conn.execute(
                "UPDATE flow_ledger SET direction = 'hold', outcome = ?3, source_seq_last = ?4
                  WHERE lot_id = ?1 AND station = ?2 AND left_ms IS NULL",
                params![lot.id, station.id(), event.outcome, event.seq],
            )
            .map_err(|err| err.to_string())?;
            Ok(())
        }
        Step::Release(station) => {
            conn.execute(
                "UPDATE flow_ledger SET direction = 'open', source_seq_last = ?3
                  WHERE lot_id = ?1 AND station = ?2 AND left_ms IS NULL
                    AND direction = 'hold'",
                params![lot.id, station.id(), event.seq],
            )
            .map_err(|err| err.to_string())?;
            Ok(())
        }
    }
}

/// Open a visit, unless the lot is already at the station.
fn enter(
    conn: &Connection,
    event: &EventView<'_>,
    lot: &Lot,
    station: Station,
) -> Result<(), String> {
    if open_visit(conn, &lot.id, station)?.is_some() {
        return Ok(());
    }
    let family = match event.family {
        Some(family) => Some(family.to_string()),
        None => known_family(conn, &lot.id)?,
    };
    conn.execute(
        "INSERT INTO flow_ledger (lot_id, family, repo, pr, todo_id, shift, station,
             entered_ms, started_ms, direction, source_seq_first, source_seq_last)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?10, 'open', ?9, ?9)",
        params![
            lot.id,
            family,
            lot.repo,
            lot.pr,
            lot.todo_id,
            event.shift,
            station.id(),
            event.ts_ms,
            event.seq,
            station.starts_on_arrival().then_some(event.ts_ms),
        ],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

/// The family an earlier visit of the lot recorded, for an event that names
/// none (`pr.merged` on a pull request the forge did not tag).
fn known_family(conn: &Connection, lot_id: &str) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT family FROM flow_ledger WHERE lot_id = ?1 AND family IS NOT NULL
          ORDER BY seq LIMIT 1",
        params![lot_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|err| err.to_string())
}

/// Close the open visit, if there is one. A visit that never started is left
/// without a `started_ms`: a process time nobody measured is not a zero.
fn close(
    conn: &Connection,
    event: &EventView<'_>,
    lot: &Lot,
    station: Station,
    direction: &str,
    back_to: Option<Station>,
) -> Result<(), String> {
    conn.execute(
        "UPDATE flow_ledger SET left_ms = ?3, direction = ?4, rework_to = ?5,
             outcome = COALESCE(?6, outcome), source_seq_last = ?7
          WHERE lot_id = ?1 AND station = ?2 AND left_ms IS NULL",
        params![
            lot.id,
            station.id(),
            event.ts_ms,
            direction,
            back_to.map(Station::id),
            event.outcome,
            event.seq,
        ],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

/// The open visit of a lot at a station: its row id and whether it started.
fn open_visit(
    conn: &Connection,
    lot_id: &str,
    station: Station,
) -> Result<Option<(i64, Option<i64>)>, String> {
    conn.query_row(
        "SELECT seq, started_ms FROM flow_ledger
          WHERE lot_id = ?1 AND station = ?2 AND left_ms IS NULL ORDER BY seq DESC LIMIT 1",
        params![lot_id, station.id()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(|err| err.to_string())
}

/// The lot's newest visit to the station left forward.
fn left_forward(conn: &Connection, lot_id: &str, station: Station) -> Result<bool, String> {
    let direction = conn
        .query_row(
            "SELECT direction FROM flow_ledger WHERE lot_id = ?1 AND station = ?2
              ORDER BY seq DESC LIMIT 1",
            params![lot_id, station.id()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|err| err.to_string())?;
    Ok(direction.as_deref() == Some("forward"))
}
