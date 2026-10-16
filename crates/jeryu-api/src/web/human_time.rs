//! Human minutes per change: how much of a person's time a change cost, and
//! what that makes the engineering time behind each successful outcome.
//! Contract: `docs/pipeline-events.md`, "Human minutes per change".
//!
//! Minutes are charged to a *change lot* -- a todo id, or `<owner>/<repo>#<N>`
//! for a pull request no todo carries, exactly as `flow_ledger` spells a lot
//! -- and stored in `change_human_minutes` next to what the lot ended as in
//! `change_outcomes` (migration `0023_change_outcomes.sql`). Neither table is
//! ever pruned: the event log is telemetry, time people spent is a record.
//!
//! Four sources are counted, and each is labelled in the answer so a page can
//! say what is and is not counted:
//!
//! - `review`: a person submitting a review, an approval or a comment.
//!   Passive diff GETs, including background refreshes, write no marks.
//! - `commit`: a commit a person authored, pushed to a lot's branch.
//! - `session`: an interactive agent session a person drove, scoped to a todo
//!   or a pull request (`agent_presence`, with an `operator`).
//! - `triage`: a todo action a person took (release, block, rework filing,
//!   any other action on the todo).
//!
//! Each of those writes a *mark*: the moment a person did something. A mark
//! charges the gap since that person's previous mark on the same lot from the
//! same source, capped at [`IDLE_GAP_MS`]: a longer gap is not working time,
//! it is a person who went away and came back, so it opens a new session and
//! charges [`MARK_FLOOR_MS`] for the action itself. A mark that does not move
//! forward in time charges nothing, so a commit pushed twice is one commit.
//!
//! `actor` is stored so that two people on one lot are two sets of gaps, and
//! for nothing else: every read aggregates by family, repository and work
//! kind. There is no per-person read, by design -- this is the cost of the
//! system, not a ranking of people.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::WebState;
use super::strict_query::{StrictFields, StrictQuery, filter_one_of};
use super::workcells_support::{TypedError, typed_error};

pub(crate) const SCHEMA: &str = "jeryu.human_minutes/v1";
const DOCS: &str = "docs/pipeline-events.md";
/// Days before `until` the window starts when `since` is left out.
const DEFAULT_DAYS: i64 = 28;

/// The longest gap between two of one person's marks that is still working
/// time. Fifteen minutes: longer than reading a diff and thinking about it,
/// shorter than a meeting.
pub(crate) const IDLE_GAP_MS: i64 = 15 * 60 * 1000;
/// What one mark charges when no gap measures it: the first action of a
/// session, or the first action after an idle gap.
pub(crate) const MARK_FLOOR_MS: i64 = 60 * 1000;

/// Where a charge of minutes came from. The wire names are stable: a reader
/// labels its rows with them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Source {
    Review,
    Commit,
    Session,
    Triage,
}

impl Source {
    pub(crate) const ALL: [Self; 4] = [Self::Review, Self::Commit, Self::Session, Self::Triage];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Commit => "commit",
            Self::Session => "session",
            Self::Triage => "triage",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|source| source.as_str() == name)
    }

    /// What the answer says this source counts.
    fn label(self) -> &'static str {
        match self {
            Self::Review => "explicit review actions and comments by a person",
            Self::Commit => "commits a person authored on the change's branch",
            Self::Session => "interactive agent sessions a person drove for the change",
            Self::Triage => "todo actions a person took: release, block, rework filing",
        }
    }
}

/// What kind of work a change lot was, as `change_outcomes.work_kind`
/// spells it. A lot is a plain `change` until something says otherwise.
pub(crate) const REWORK: &str = "rework";
pub(crate) const PIN_BUMP: &str = "pin_bump";

/// One moment a person spent time on a change.
#[derive(Clone, Debug)]
pub(crate) struct Mark<'a> {
    pub source: Source,
    /// The login whose gaps are measured. Never reported.
    pub actor: &'a str,
    pub at_ms: i64,
    pub family: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub pr: Option<i64>,
    pub todo_id: Option<&'a str>,
}

impl<'a> Mark<'a> {
    pub(crate) fn new(source: Source, actor: &'a str, at_ms: i64) -> Self {
        Self {
            source,
            actor,
            at_ms,
            family: None,
            repo: None,
            pr: None,
            todo_id: None,
        }
    }

    pub(crate) fn on_pull(mut self, repo: &'a str, pr: i64) -> Self {
        self.repo = Some(repo);
        self.pr = Some(pr);
        self
    }

    pub(crate) fn on_todo(mut self, todo_id: &'a str) -> Self {
        self.todo_id = Some(todo_id);
        self
    }

    pub(crate) fn in_family(mut self, family: Option<&'a str>) -> Self {
        self.family = family;
        self
    }
}

fn text(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// The lot a mark or an event belongs to: its todo when one is named, else
/// the lot the flow ledger already keeps the pull request under, else the
/// pull request itself.
fn lot_of(
    conn: &Connection,
    todo_id: Option<&str>,
    repo: Option<&str>,
    pr: Option<i64>,
) -> Result<Option<String>, String> {
    if let Some(todo) = text(todo_id) {
        return Ok(Some(todo.to_string()));
    }
    let (Some(repo), Some(pr)) = (text(repo), pr) else {
        return Ok(None);
    };
    let known: Option<String> = conn
        .query_row(
            "SELECT lot_id FROM flow_ledger WHERE repo = ?1 AND pr = ?2
              ORDER BY seq DESC LIMIT 1",
            params![repo, pr],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| err.to_string())?;
    Ok(Some(known.unwrap_or_else(|| pull_lot(repo, pr))))
}

/// How a pull request spells its own lot key.
pub(crate) fn pull_lot(repo: &str, pr: i64) -> String {
    format!("{repo}#{pr}")
}

/// Open or touch the lot's outcome row. Family and repository are filled in
/// the first time they are known and never overwritten: a lot keeps the
/// family it was worked in.
fn ensure_lot(
    conn: &Connection,
    lot: &str,
    family: Option<&str>,
    repo: Option<&str>,
    at_ms: i64,
) -> Result<(), String> {
    conn.execute(
        "INSERT OR IGNORE INTO change_outcomes (lot_id, family, repo, work_kind, first_ms, last_ms)
         VALUES (?1, ?2, ?3, 'change', ?4, ?4)",
        params![lot, text(family), text(repo), at_ms],
    )
    .map_err(|err| err.to_string())?;
    conn.execute(
        "UPDATE change_outcomes
            SET family = COALESCE(family, ?2), repo = COALESCE(repo, ?3),
                first_ms = MIN(first_ms, ?4), last_ms = MAX(last_ms, ?4)
          WHERE lot_id = ?1",
        params![lot, text(family), text(repo), at_ms],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

/// The UTC day a time falls on, as the ledger spells it.
fn day_of(at_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(at_ms)
        .unwrap_or_default()
        .format("%Y-%m-%d")
        .to_string()
}

/// What one mark charges: the gap since the actor's previous mark on this lot
/// from this source, when that gap is at most [`IDLE_GAP_MS`]. A longer gap,
/// or no previous mark at all, is the first action of a session and charges
/// [`MARK_FLOOR_MS`].
fn charged_ms(previous: Option<i64>, at_ms: i64) -> i64 {
    match previous {
        // A gap longer than the cap is not work: the person went away. The
        // mark opens a new session and charges the action itself.
        Some(previous) if at_ms > previous && at_ms - previous <= IDLE_GAP_MS => at_ms - previous,
        Some(previous) if at_ms <= previous => 0,
        _ => MARK_FLOOR_MS,
    }
}

/// Charge one mark to its lot. A mark that names no lot is nobody's time and
/// is dropped.
pub(crate) fn charge(conn: &Connection, mark: &Mark<'_>) -> Result<(), String> {
    let Some(actor) = text(Some(mark.actor)) else {
        return Ok(());
    };
    let Some(lot) = lot_of(conn, mark.todo_id, mark.repo, mark.pr)? else {
        return Ok(());
    };
    let tx = conn
        .unchecked_transaction()
        .map_err(|err| err.to_string())?;
    charge_lot(&tx, mark, actor, &lot)?;
    tx.commit().map_err(|err| err.to_string())
}

fn charge_lot(conn: &Connection, mark: &Mark<'_>, actor: &str, lot: &str) -> Result<(), String> {
    ensure_lot(conn, lot, mark.family, mark.repo, mark.at_ms)?;
    let previous: Option<i64> = conn
        .query_row(
            "SELECT MAX(last_ms) FROM change_human_minutes
              WHERE lot_id = ?1 AND source = ?2 AND actor = ?3",
            params![lot, mark.source.as_str(), actor],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if previous.is_some_and(|previous| previous >= mark.at_ms) {
        // Marks only ever move forward: a commit pushed a second time, or a
        // beat that repeats a time already charged, is not more time.
        return Ok(());
    }
    let minutes = charged_ms(previous, mark.at_ms) as f64 / 60_000.0;
    conn.execute(
        "INSERT INTO change_human_minutes (lot_id, source, actor, day, minutes, marks, last_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
         ON CONFLICT(lot_id, source, actor, day) DO UPDATE SET
           minutes = minutes + excluded.minutes,
           marks = marks + 1,
           last_ms = MAX(last_ms, excluded.last_ms)",
        params![
            lot,
            mark.source.as_str(),
            actor,
            day_of(mark.at_ms),
            minutes,
            mark.at_ms
        ],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

/// Charge one mark through the event store's connection, from a route.
pub(crate) fn record(state: &WebState, mark: &Mark<'_>) {
    if let Err(err) = state.events.charge_human_minutes(mark) {
        eprintln!("jeryu-api human minutes: {err}");
    }
}

/// Estimate a person's attention from explicit review actions and comments.
/// Read-only requests must never call this function.
///
/// Only a person's time is counted. The same routes carry the agent reviewer
/// and other automation, so the caller must be an active account the
/// instance knows to be a person ([`super::people::assignable_person`]).
pub(crate) fn pull_mark(state: &WebState, pr: &jeryu_core::PullRequest, login: &str) {
    let Some(login) = super::people::assignable_person(state, login) else {
        return;
    };
    let Ok(number) = i64::try_from(pr.number) else {
        return;
    };
    let repo = format!("{}/{}", pr.owner, pr.repo);
    let (family, _shift) =
        super::shift::shift_context(state, &pr.owner, &pr.repo, &pr.head.ref_name);
    record(
        state,
        &Mark::new(Source::Review, &login, Utc::now().timestamp_millis())
            .on_pull(&repo, number)
            .in_family(family.as_deref()),
    );
}

/// Charge a person's time triaging a todo: releasing, blocking, filing a
/// rework, any other action on it.
pub(crate) fn todo_mark(state: &WebState, family: &str, todo_id: &str, login: &str) {
    let Some(login) = super::people::assignable_person(state, login) else {
        return;
    };
    record(
        state,
        &Mark::new(Source::Triage, &login, Utc::now().timestamp_millis())
            .on_todo(todo_id)
            .in_family(Some(family)),
    );
}

/// At most this many commits of one push are read. A person does not hand
/// write more than this in one go, and the push path must stay cheap.
const MAX_COMMITS: usize = 200;

/// One commit of a push, as [`push_commits`] reads it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PushedCommit {
    /// The author's email address, which is what names the account.
    pub email: String,
    /// Author time in milliseconds.
    pub at_ms: i64,
}

/// Parse `git log --format=%ae%x1f%at` output.
fn parse_commits(out: &str) -> Vec<PushedCommit> {
    out.lines()
        .filter_map(|line| {
            let (email, at) = line.trim().split_once('\u{1f}')?;
            let at: i64 = at.trim().parse().ok()?;
            // The author date is whatever the committer wrote. A date past the
            // server's clock (or one that overflows) is no mark: it would park
            // the person's ledger in the future and drop every real mark after.
            let at_ms = at.checked_mul(1000)?;
            if at_ms > chrono::Utc::now().timestamp_millis() + 60_000 {
                return None;
            }
            let email = email.trim();
            (!email.is_empty()).then(|| PushedCommit {
                email: email.to_lowercase(),
                at_ms,
            })
        })
        .collect()
}

/// The commits one branch update added, oldest first. A new branch is read
/// against the repository's default branch, so a branch cut from main only
/// counts its own work.
fn push_commits(
    state: &WebState,
    owner: &str,
    name: &str,
    update: &crate::ci_bridge::RefUpdate,
) -> Vec<PushedCommit> {
    let Ok(opened) = state.repo_manager.open_parts(owner, name) else {
        return Vec::new();
    };
    let default = state
        .core
        .get_repository(owner, name)
        .map(|repo| repo.default_branch)
        .unwrap_or_else(|_| "main".to_string());
    let range = if update.old_oid == crate::ci_bridge::ZERO_OID {
        format!("refs/heads/{default}..{}", update.new_oid)
    } else {
        format!("{}..{}", update.old_oid, update.new_oid)
    };
    let max = MAX_COMMITS.to_string();
    let Ok(out) = super::shift::run_git(
        &state.repo_manager.config().git_bin,
        opened.path.as_path(),
        &[
            "log",
            "--max-count",
            &max,
            "--format=%ae%x1f%at",
            "--end-of-options",
            &range,
        ],
        &[],
        None,
    ) else {
        return Vec::new();
    };
    parse_commits(&String::from_utf8_lossy(&out))
}

/// Charge the commits a person authored on a lot's branch.
///
/// Runs on the detached post-push work, next to the gate signal: a push's
/// acknowledgement never waits for it. A commit counts when its author
/// address belongs to an account the instance knows to be a person, and when
/// the branch it landed on carries a pull request -- a branch nobody opened a
/// pull request for is not yet a change.
pub(crate) fn record_push(
    state: &WebState,
    owner: &str,
    repo: &str,
    updates: &[crate::ci_bridge::RefUpdate],
) {
    let name = repo.trim_end_matches(".git");
    let full_name = format!("{owner}/{name}");
    let Ok(pulls) = state.core.list_pull_requests(owner, name, None) else {
        return;
    };
    for update in updates {
        let Some(branch) = update.ref_name.strip_prefix("refs/heads/") else {
            continue;
        };
        let Some(number) = pulls
            .iter()
            .filter(|pull| pull.head.ref_name == branch && !pull.merged)
            .map(|pull| pull.number)
            .max()
            .and_then(|number| i64::try_from(number).ok())
        else {
            continue;
        };
        let (family, _shift) = super::shift::shift_context(state, owner, name, branch);
        let mut people: BTreeMap<String, Option<String>> = BTreeMap::new();
        for commit in push_commits(state, owner, name, update) {
            let login = people
                .entry(commit.email)
                .or_insert_with_key(|email| person_for_email(state, email))
                .clone();
            let Some(login) = login else { continue };
            record(
                state,
                &Mark::new(Source::Commit, &login, commit.at_ms)
                    .on_pull(&full_name, number)
                    .in_family(family.as_deref()),
            );
        }
    }
}

/// The account that proved this address, when it is a person's.
fn person_for_email(state: &WebState, email: &str) -> Option<String> {
    let login = state.account_email.verified_holder(email).ok()??;
    super::people::assignable_person(state, &login)
}

/// The `owner/name#number` a presence link spells, as a lot.
fn link_pull(link: &str) -> Option<(String, i64)> {
    let (repo, number) = link.rsplit_once('#')?;
    let number: i64 = number.trim().parse().ok()?;
    (!repo.trim().is_empty() && number > 0).then(|| (repo.trim().to_string(), number))
}

/// The todo a presence link spells. Reporters qualify a todo with its family
/// (`acme:20261012-1200-abc123`); the lot is the todo id alone, as the event
/// log and the flow ledger spell it.
fn link_todo(link: &str) -> Option<String> {
    let id = link.rsplit(':').next()?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Charge the wall-clock of an interactive agent session a person drove.
///
/// Only an `active` beat whose `operator` is an account the instance knows to
/// be a person, and which names the change it is working on, is time anyone
/// can attribute. Each beat charges the gap since that operator's previous
/// beat on the same lot, so a session that stops beating stops charging.
pub(crate) fn session_mark(state: &WebState, row: &super::agent_presence::PresenceRow) {
    if row.state != super::agent_presence::PresenceState::Active {
        return;
    }
    let Some(operator) = row
        .operator
        .as_deref()
        .and_then(|operator| super::people::assignable_person(state, operator))
    else {
        return;
    };
    let Some(links) = row.links.as_ref() else {
        return;
    };
    let todo = links.todo.as_deref().and_then(link_todo);
    let pull = links.pr.as_deref().and_then(link_pull);
    let family = row.families.first().map(String::as_str);
    // `last_active_ms` is self-reported; the beat's arrival is the server's
    // own clock, and no mark is later than that.
    let at_ms = row.last_active_ms.min(row.received_ms);
    let mut mark = Mark::new(Source::Session, &operator, at_ms).in_family(family);
    match (&todo, &pull) {
        (Some(todo), _) => mark = mark.on_todo(todo),
        (None, Some((repo, number))) => mark = mark.on_pull(repo, *number),
        (None, None) => return,
    }
    record(state, &mark);
}

/// The lots a report groups, and the minutes charged to each of them in the
/// window: what [`read`] answers.
type Ledgers = (BTreeMap<String, LotFacts>, Vec<(String, Source, f64)>);

/// What a stored event says about a change's outcome, folded in the same
/// transaction as the event itself (`EventStore::insert`).
///
/// `pr.merged` lands a change; `defect.linked` and `incident.caused_by` fail
/// one; a rework todo and an auto-pin bump say what kind of work their lot
/// is. Nothing here charges minutes: people's time arrives from the routes
/// they act on, not from the log.
pub(crate) fn apply(
    conn: &Connection,
    event: &super::pipeline::NewEvent,
    ts_ms: i64,
) -> Result<(), String> {
    let detail = event
        .detail
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    let string = |key: &str| {
        detail
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let lot = |conn: &Connection| {
        lot_of(
            conn,
            event.todo_id.as_deref(),
            event.repo.as_deref(),
            event.pr,
        )
    };
    match event.kind.as_str() {
        "pr.merged" => {
            if let Some(lot) = lot(conn)? {
                ensure_lot(
                    conn,
                    &lot,
                    event.family.as_deref(),
                    event.repo.as_deref(),
                    ts_ms,
                )?;
                conn.execute(
                    "UPDATE change_outcomes SET landed_ms = COALESCE(landed_ms, ?2)
                      WHERE lot_id = ?1",
                    params![lot, ts_ms],
                )
                .map_err(|err| err.to_string())?;
            }
        }
        "defect.linked" => {
            if let Some(lot) = lot(conn)? {
                ensure_lot(
                    conn,
                    &lot,
                    event.family.as_deref(),
                    event.repo.as_deref(),
                    ts_ms,
                )?;
                fail(conn, &lot, ts_ms)?;
            }
            // A rework names the todo that redoes the change: that todo's own
            // lot is rework, not a change of its own.
            if string("cause").as_deref() == Some("rework")
                && let Some(by) = string("by")
            {
                ensure_lot(conn, &by, event.family.as_deref(), None, ts_ms)?;
                set_work_kind(conn, &by, REWORK)?;
            }
        }
        super::incidents::CAUSED_BY => {
            for key in incident_changes(&detail) {
                ensure_lot(conn, &key, event.family.as_deref(), None, ts_ms)?;
                fail(conn, &key, ts_ms)?;
            }
        }
        "pin.bump_opened" => {
            if let Some(lot) = lot(conn)? {
                ensure_lot(
                    conn,
                    &lot,
                    event.family.as_deref(),
                    event.repo.as_deref(),
                    ts_ms,
                )?;
                set_work_kind(conn, &lot, PIN_BUMP)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The change lot keys an `incident.caused_by` names. The event route has
/// already validated them; a malformed one never reaches the fold.
fn incident_changes(detail: &serde_json::Value) -> Vec<String> {
    // The incident validator's own reading (trimmed, non-empty, bounded), so
    // the never-pruned ledgers see exactly the keys the event was checked for.
    super::incidents::changes(detail).unwrap_or_default()
}

fn fail(conn: &Connection, lot: &str, ts_ms: i64) -> Result<(), String> {
    conn.execute(
        "UPDATE change_outcomes SET failed_ms = COALESCE(failed_ms, ?2) WHERE lot_id = ?1",
        params![lot, ts_ms],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

fn set_work_kind(conn: &Connection, lot: &str, kind: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE change_outcomes SET work_kind = ?2 WHERE lot_id = ?1 AND work_kind = 'change'",
        params![lot, kind],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

/// The days a report covers, both ends included (UTC).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Window {
    pub since: NaiveDate,
    pub until: NaiveDate,
}

/// How the report groups minutes (`?by=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum By {
    Family,
    Repo,
    WorkKind,
}

impl By {
    pub(crate) const NAMES: &'static [&'static str] = &["family", "repo", "work_kind"];

    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "family" => Self::Family,
            "repo" => Self::Repo,
            "work_kind" => Self::WorkKind,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Family => "family",
            Self::Repo => "repo",
            Self::WorkKind => "work_kind",
        }
    }

    fn key(self, lot: &LotFacts) -> String {
        match self {
            Self::Family => lot.family.clone().unwrap_or_else(|| "(unknown)".into()),
            Self::Repo => lot.repo.clone().unwrap_or_else(|| "(none)".into()),
            Self::WorkKind => lot.work_kind.clone(),
        }
    }
}

/// `GET /api/v1/human-minutes` query.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct HumanMinutesQuery {
    /// Either spelling of a family key; the server canonicalises it.
    pub family: Option<String>,
    /// First day of the window, `YYYY-MM-DD` (UTC).
    pub since: Option<String>,
    /// Last day of the window, `YYYY-MM-DD` (UTC), included.
    pub until: Option<String>,
    /// How minutes are grouped: one of [`By::NAMES`].
    pub by: Option<String>,
}

impl StrictFields for HumanMinutesQuery {
    const KEYS: &'static [&'static str] = &["family", "since", "until", "by"];
    const REPAIR_HINT: &'static str =
        "send since and until as YYYY-MM-DD and by as one of the values the message names";

    fn check_values(&self) -> Result<(), String> {
        filter_one_of("by", self.by.as_ref(), By::NAMES)?;
        for (name, value) in [("since", &self.since), ("until", &self.until)] {
            day(name, value.as_deref())?;
        }
        Ok(())
    }
}

fn day(name: &str, value: Option<&str>) -> Result<Option<NaiveDate>, String> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| format!("{name}: {value:?} is not a day spelled YYYY-MM-DD")),
    }
}

/// The window a query names, with `today` filling what it leaves out.
fn window(query: &HumanMinutesQuery, today: NaiveDate) -> Result<Window, String> {
    let until = day("until", query.until.as_deref())?.unwrap_or(today);
    // A window that ends in the future has nothing more to say than one that
    // ends tomorrow, and bounding it keeps every date sum below in range.
    if until > today + Duration::days(1) {
        return Err(format!(
            "until ({until}) is after tomorrow ({})",
            today + Duration::days(1)
        ));
    }
    let since = match day("since", query.since.as_deref())? {
        Some(since) => since,
        None => until
            .checked_sub_signed(Duration::days(DEFAULT_DAYS))
            .ok_or_else(|| format!("until ({until}) is too early for the default window"))?,
    };
    if since > until {
        return Err(format!("since ({since}) is after until ({until})"));
    }
    Ok(Window { since, until })
}

/// What the report knows about one lot.
struct LotFacts {
    family: Option<String>,
    repo: Option<String>,
    work_kind: String,
    landed_ms: Option<i64>,
    failed: bool,
}

/// One group's minutes against the changes they bought.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct Row {
    pub minutes: f64,
    pub review_minutes: f64,
    pub commit_minutes: f64,
    pub session_minutes: f64,
    pub triage_minutes: f64,
    /// Changes that landed inside the window.
    pub changes: u64,
    pub successful_changes: u64,
    pub failed_changes: u64,
    /// The number the Scorecard shows: minutes divided by successful changes
    /// only. `null` while the group landed none.
    pub minutes_per_successful_change: Option<f64>,
}

impl Row {
    fn add(&mut self, source: Source, minutes: f64) {
        match source {
            Source::Review => self.review_minutes += minutes,
            Source::Commit => self.commit_minutes += minutes,
            Source::Session => self.session_minutes += minutes,
            Source::Triage => self.triage_minutes += minutes,
        }
        self.minutes += minutes;
    }

    fn finish(&mut self) {
        self.minutes = round(self.minutes);
        self.review_minutes = round(self.review_minutes);
        self.commit_minutes = round(self.commit_minutes);
        self.session_minutes = round(self.session_minutes);
        self.triage_minutes = round(self.triage_minutes);
        self.minutes_per_successful_change = (self.successful_changes > 0)
            .then(|| round(self.minutes / self.successful_changes as f64));
    }
}

fn round(minutes: f64) -> f64 {
    (minutes * 10.0).round() / 10.0
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct GroupRow {
    pub group: String,
    #[serde(flatten)]
    pub row: Row,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct SourceRow {
    pub source: &'static str,
    pub label: &'static str,
    pub minutes: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct HumanMinutesReport {
    pub schema_version: &'static str,
    pub generated_at: String,
    pub window: Window,
    pub family: Option<String>,
    pub by: &'static str,
    /// What each source counts, so a page can say where the minutes came
    /// from; the minutes are the window's, over every group.
    pub sources: Vec<SourceRow>,
    /// What these numbers deliberately leave out.
    pub not_counted: Vec<&'static str>,
    pub totals: Row,
    pub groups: Vec<GroupRow>,
}

/// What a read of the two tables answers, before it is grouped.
fn read(conn: &Connection, family: Option<&str>, window: Window) -> Result<Ledgers, String> {
    let mut lots: BTreeMap<String, LotFacts> = BTreeMap::new();
    let (since_ms, until_ms) = window_ms(window);
    let (since_day, until_day) = (window.since.to_string(), window.until.to_string());
    {
        // Only the lots the report can use: landed inside the window, or
        // charged minutes inside it. The ledgers are never pruned, so an
        // unwindowed read would grow with the life of the instance.
        let mut stmt = conn
            .prepare(
                "SELECT o.lot_id, o.family, o.repo, o.work_kind, o.landed_ms,
                        o.failed_ms IS NOT NULL
                          OR EXISTS (SELECT 1 FROM incident_causes c
                                      WHERE c.change_key = o.lot_id)
                   FROM change_outcomes o
                  WHERE (?1 IS NULL OR o.family = ?1)
                    AND ((o.landed_ms >= ?2 AND o.landed_ms < ?3)
                         OR o.lot_id IN (SELECT m.lot_id FROM change_human_minutes m
                                          WHERE m.day >= ?4 AND m.day <= ?5))
                  ORDER BY o.lot_id",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(
                params![family, since_ms, until_ms, since_day, until_day],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        LotFacts {
                            family: row.get(1)?,
                            repo: row.get(2)?,
                            work_kind: row.get(3)?,
                            landed_ms: row.get(4)?,
                            failed: row.get::<_, i64>(5)? != 0,
                        },
                    ))
                },
            )
            .map_err(|err| err.to_string())?;
        for row in rows {
            let (lot, facts) = row.map_err(|err| err.to_string())?;
            lots.insert(lot, facts);
        }
    }
    let mut minutes = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT m.lot_id, m.source, SUM(m.minutes)
                   FROM change_human_minutes m
                  WHERE m.day >= ?1 AND m.day <= ?2
                  GROUP BY m.lot_id, m.source
                  ORDER BY m.lot_id, m.source",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(
                params![window.since.to_string(), window.until.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f64>(2)?,
                    ))
                },
            )
            .map_err(|err| err.to_string())?;
        for row in rows {
            let (lot, source, charged) = row.map_err(|err| err.to_string())?;
            if let Some(source) = Source::parse(&source) {
                minutes.push((lot, source, charged));
            }
        }
    }
    Ok((lots, minutes))
}

/// The window as ledger times: midnight UTC of `since` up to, not including,
/// midnight UTC of the day after `until`.
fn window_ms(window: Window) -> (i64, i64) {
    let start = |day: NaiveDate| day.and_time(NaiveTime::MIN).and_utc().timestamp_millis();
    // `window` keeps `until` at most tomorrow, so the next day exists; the
    // fallback keeps this total so it can never panic under the store lock.
    let after = window.until.succ_opt().unwrap_or(window.until);
    (start(window.since), start(after))
}

/// Build the report from the two ledgers.
pub(crate) fn report(
    conn: &Connection,
    family: Option<String>,
    window: Window,
    by: By,
    now: DateTime<Utc>,
) -> Result<HumanMinutesReport, String> {
    let (lots, minutes) = read(conn, family.as_deref(), window)?;
    let (since_ms, until_ms) = window_ms(window);
    let mut groups: BTreeMap<String, Row> = BTreeMap::new();
    let mut totals = Row::default();
    let mut by_source: BTreeMap<Source, f64> = BTreeMap::new();
    for (lot, source, charged) in minutes {
        let Some(facts) = lots.get(&lot) else {
            // Minutes on a lot the family filter left out.
            continue;
        };
        groups
            .entry(by.key(facts))
            .or_default()
            .add(source, charged);
        totals.add(source, charged);
        *by_source.entry(source).or_default() += charged;
    }
    for facts in lots.values() {
        let landed = facts
            .landed_ms
            .is_some_and(|landed| landed >= since_ms && landed < until_ms);
        if !landed {
            continue;
        }
        let row = groups.entry(by.key(facts)).or_default();
        for row in [row, &mut totals] {
            row.changes += 1;
            if facts.failed {
                row.failed_changes += 1;
            } else {
                row.successful_changes += 1;
            }
        }
    }
    totals.finish();
    Ok(HumanMinutesReport {
        schema_version: SCHEMA,
        generated_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        window,
        family,
        by: by.as_str(),
        sources: Source::ALL
            .into_iter()
            .map(|source| SourceRow {
                source: source.as_str(),
                label: source.label(),
                minutes: round(by_source.get(&source).copied().unwrap_or_default()),
            })
            .collect(),
        not_counted: vec![
            "agent time and model spend, which GET /api/v1/yield reports in dollars",
            "time nobody recorded: work off the forge, and reading a page that is not a pull request's files",
            "anything per person: minutes are aggregated by family, repository and work kind only",
        ],
        groups: groups
            .into_iter()
            .map(|(group, mut row)| {
                row.finish();
                GroupRow { group, row }
            })
            .collect(),
        totals,
    })
}

fn refuse(status: StatusCode, code: &str, reason: &str, hint: &str) -> AxumResponse {
    typed_error(TypedError {
        status,
        code,
        purpose: "report the human minutes each change cost",
        reason,
        common_fixes: &[
            "send since and until as YYYY-MM-DD with since not after until",
            "send by as one of family, repo, work_kind",
        ],
        docs_url: DOCS,
        repair_hint: hint,
        message: reason,
    })
}

/// `GET /api/v1/human-minutes`
pub(crate) async fn human_minutes(
    State(state): State<Arc<WebState>>,
    StrictQuery(query): StrictQuery<HumanMinutesQuery>,
) -> AxumResponse {
    let family = match super::family::filter(&state, query.family.as_deref()) {
        Ok(family) => family,
        Err(response) => return *response,
    };
    let now = Utc::now();
    let window = match window(&query, now.date_naive()) {
        Ok(window) => window,
        Err(reason) => {
            return refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                super::strict_query::CODE,
                &reason,
                "send a since that is not after until",
            );
        }
    };
    let by = query
        .by
        .as_deref()
        .map(str::trim)
        .filter(|by| !by.is_empty())
        .and_then(By::parse)
        .unwrap_or(By::Family);
    // The ledgers are SQLite reads under the event-store lock: off the async
    // runtime, as every sibling report does.
    let worker = state.clone();
    let read = tokio::task::spawn_blocking(move || {
        worker
            .events
            .with_conn(|conn| report(conn, family, window, by, now))
    })
    .await
    .map_err(|err| err.to_string())
    .and_then(|read| read);
    match read {
        Ok(report) => Json(report).into_response(),
        Err(reason) => refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "human_minutes_read_failed",
            &reason,
            "check <data_dir>/shift.sqlite",
        ),
    }
}
