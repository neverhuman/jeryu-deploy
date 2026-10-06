//! Family release boards: what every deliverable of a product family runs at
//! each of its stages, rendered by `/releases`.
//!
//! A board is assembled off the forge, by a collector on the build host
//! (`scripts/release-board/`), because most of what it reads cannot be seen
//! from here: fleet nodes, image registries, staged bundles, public download
//! manifests, todo queues. The collector PUTs one snapshot per family every
//! five minutes and again at the end of every release script, so the page
//! moves when a release lands rather than a tick later.
//!
//! Snapshots are held in memory, like runner heartbeats: a restarted forge has
//! no board until the next collector run, which is at most one tick away and is
//! triggered at once by a forge release. The shape is `jeryu.release_board.v1`,
//! documented in `docs/release-board.md`.
//!
//! Who may write: logins named in `JERYU_BOARD_REPORTERS` (comma-separated, no
//! default — a collector login is site configuration), or any forge admin. Reads are admin-only by path
//! (see `auth::admin_only_request`): a board names hosts, commands and pinned
//! commits of private repositories.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use jeryu_core::{AccountSummary, UserRole};
use jeryu_readmodel::contracts::WebEvent;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::web::WebState;
use crate::web::pipeline::PIPELINE_SCOPE;

pub(crate) const SCHEMA: &str = "jeryu.release_board.v1";
/// The whole request body, before parsing.
pub(crate) const MAX_BODY_BYTES: usize = 512 * 1024;
const MAX_STRING_CHARS: usize = 2000;
const MAX_LANES: usize = 32;
const MAX_STAGES: usize = 16;
const MAX_TARGETS: usize = 32;
const MAX_SHIPS: usize = 50;
const MAX_PROBLEMS: usize = 64;
const MAX_PIN_ROWS: usize = 200;
const MAX_PIN_COLUMNS: usize = 12;
const MAX_BOARD_COLUMNS: usize = 8;
const MAX_FAMILIES: usize = 64;
const MAX_TARGET_RUNNERS: usize = 64;
const MAX_RUNNER_ID_CHARS: usize = 200;
/// A collector clock may run a little ahead of the forge's; more than this is
/// a wrong clock, and a board from the future would never be replaced.
const MAX_CLOCK_SKEW_SECS: i64 = 300;
/// Pipeline websocket kind that tells open pages to refetch. Not written to
/// the event log: a collector posts every five minutes per family, and the
/// log is for things that happened, not for a page being refreshed.
pub(crate) const UPDATED_KIND: &str = "release_board.updated";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StageState {
    Ok,
    Warn,
    Bad,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum KnownBy {
    Reported,
    Host,
    Derived,
    Unverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Trigger {
    Timer,
    Release,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum WorkKey {
    Live,
    Merged,
    Stranded,
    Untraceable,
    Blocked,
    Open,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReleaseBoard {
    pub schema: String,
    /// Canonical family key (see `crate::web::family`). A collector may post
    /// either spelling; the stored board carries the canonical one.
    pub family: String,
    /// What a reader is shown for `family`. Set by the forge when it accepted
    /// the snapshot; ignored on input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family_label: Option<String>,
    pub observed_at: String,
    /// Set by the forge when it accepted the snapshot; ignored on input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<String>,
    pub summary: String,
    pub collector: Collector,
    /// The family's fixed stage columns, left to right. When present the page
    /// draws every lane on one grid and a column a lane skips as "not used";
    /// when empty each lane is its own track.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<Column>,
    pub lanes: Vec<Lane>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<Work>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pins: Option<Pins>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<Notes>,
    #[serde(default)]
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Collector {
    pub host: String,
    pub version: String,
    pub trigger: Trigger,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Column {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Lane {
    pub id: String,
    pub name: String,
    /// Neighbouring lanes with the same group sit together under its name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub source: String,
    pub owner_family: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Stage {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    pub state: StageState,
    pub status: String,
    pub known_by: KnownBy,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parallel: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub never_deployed: bool,
    /// One of the board's `columns`; several stages of a lane may share one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    #[serde(default)]
    pub targets: Vec<Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promote: Option<Promote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ships: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge: Option<ForgeBinding>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Target {
    pub name: String,
    pub running: Option<String>,
    pub state: StageState,
    /// Runners (by `runnerId`, as `GET /api/v1/control-plane/runners` lists
    /// them) that serve this target, so `/releases` and `/runners` can link to
    /// each other. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runners: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Promote {
    pub command: String,
    pub human_only: bool,
    pub automatic: bool,
}

/// Where the page can read this stage live: the environment a deploy reports
/// to on `POST /repos/{repo}/deployments`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ForgeBinding {
    pub repo: String,
    pub environment: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Work {
    pub total: u32,
    pub method: String,
    pub parts: Vec<WorkPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlinked: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WorkPart {
    pub key: WorkKey,
    pub label: String,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Pins {
    pub note: String,
    pub columns: Vec<String>,
    pub rows: Vec<PinRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PinRow {
    pub repo: String,
    pub cells: Vec<String>,
    pub behind: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Notes {
    pub title: String,
    pub items: Vec<String>,
    pub coverage: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Problem {
    pub source: String,
    pub message: String,
}

/// What a PUT answered: whether the snapshot is now the family's board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Accepted {
    pub family: String,
    pub family_label: String,
    pub observed_at: String,
    pub accepted_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignored: Option<&'static str>,
}

/// One row of `GET /api/v1/release-board`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct BoardSummary {
    pub family: String,
    pub family_label: String,
    pub observed_at: String,
    pub accepted_at: String,
    pub summary: String,
    pub collector: Collector,
    pub problem_count: usize,
}

#[derive(Debug, Clone)]
struct StoredBoard {
    board: ReleaseBoard,
    observed: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReleaseBoardStore {
    boards: Arc<Mutex<BTreeMap<String, StoredBoard>>>,
    reporters: Arc<Vec<String>>,
}

impl ReleaseBoardStore {
    pub(crate) fn from_env() -> Self {
        // No default: a reporter login is one installation's identity. Unset,
        // only forge admins may report.
        let configured = std::env::var("JERYU_BOARD_REPORTERS").unwrap_or_default();
        Self::with_reporters(configured.split(','))
    }

    pub(crate) fn with_reporters<'a>(logins: impl IntoIterator<Item = &'a str>) -> Self {
        let reporters = logins
            .into_iter()
            .map(str::trim)
            .filter(|login| !login.is_empty())
            .map(str::to_string)
            .collect();
        Self {
            boards: Arc::new(Mutex::new(BTreeMap::new())),
            reporters: Arc::new(reporters),
        }
    }

    /// A named reporter or any forge admin, the same rule as runner heartbeats.
    pub(crate) fn may_report(&self, login: &str, admin: bool) -> bool {
        admin || self.reporters.iter().any(|reporter| reporter == login)
    }

    /// Keep `board` as `family`'s board unless the stored one was observed
    /// later: a slow timer run finishing after a release push must not put the
    /// older picture back.
    pub(crate) fn put(
        &self,
        family: &str,
        mut board: ReleaseBoard,
        now: DateTime<Utc>,
    ) -> Result<Accepted, String> {
        // Either spelling names the same family; the board is stored, and
        // answered, under the canonical key.
        let family = &crate::web::family::canonical(family);
        board.family = crate::web::family::canonical(&board.family);
        let observed = validate(family, &board, now)?;
        let accepted_at = now.to_rfc3339_opts(SecondsFormat::Secs, true);
        let mut boards = self
            .boards
            .lock()
            .map_err(|_| "release board store is unavailable".to_string())?;
        if let Some(stored) = boards.get(family)
            && stored.observed > observed
        {
            return Ok(Accepted {
                family: family.to_string(),
                family_label: crate::web::family::label(family),
                observed_at: board.observed_at,
                accepted_at,
                ignored: Some("older than stored snapshot"),
            });
        }
        if !boards.contains_key(family) && boards.len() >= MAX_FAMILIES {
            return Err(format!("at most {MAX_FAMILIES} families may keep a board"));
        }
        board.accepted_at = Some(accepted_at.clone());
        board.family_label = Some(crate::web::family::label(family));
        let observed_at = board.observed_at.clone();
        boards.insert(family.to_string(), StoredBoard { board, observed });
        Ok(Accepted {
            family: family.to_string(),
            family_label: crate::web::family::label(family),
            observed_at,
            accepted_at,
            ignored: None,
        })
    }

    pub(crate) fn get(&self, family: &str) -> Option<ReleaseBoard> {
        let boards = self.boards.lock().ok()?;
        boards
            .get(&crate::web::family::canonical(family))
            .map(|stored| stored.board.clone())
    }

    /// Every stored board, by family. The attention inbox reads these to see
    /// what the page is showing (`pipeline::attention::board`).
    pub(crate) fn all(&self) -> Vec<ReleaseBoard> {
        let Ok(boards) = self.boards.lock() else {
            return Vec::new();
        };
        boards.values().map(|stored| stored.board.clone()).collect()
    }

    pub(crate) fn list(&self) -> Vec<BoardSummary> {
        let Ok(boards) = self.boards.lock() else {
            return Vec::new();
        };
        boards
            .values()
            .map(|stored| BoardSummary {
                family_label: crate::web::family::label(&stored.board.family),
                family: stored.board.family.clone(),
                observed_at: stored.board.observed_at.clone(),
                accepted_at: stored.board.accepted_at.clone().unwrap_or_default(),
                summary: stored.board.summary.clone(),
                collector: stored.board.collector.clone(),
                problem_count: stored.board.problems.len(),
            })
            .collect()
    }
}

pub(crate) fn valid_family(family: &str) -> bool {
    let bytes = family.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Check a snapshot against `jeryu.release_board.v1`. Returns when it was
/// observed, or the first thing wrong with it, naming the field.
pub(crate) fn validate(
    family: &str,
    board: &ReleaseBoard,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    if board.schema != SCHEMA {
        return Err(format!("schema must be {SCHEMA}, got {:?}", board.schema));
    }
    if !valid_family(family) {
        return Err(format!(
            "family {family:?} must be lowercase letters, digits and dashes, starting with a letter or digit"
        ));
    }
    if board.family != family {
        return Err(format!(
            "family in the body ({:?}) must match the path ({family:?})",
            board.family
        ));
    }
    let observed = DateTime::parse_from_rfc3339(&board.observed_at)
        .map_err(|_| {
            format!(
                "observed_at must be an RFC 3339 time, got {:?}",
                board.observed_at
            )
        })?
        .with_timezone(&Utc);
    if observed - now > Duration::seconds(MAX_CLOCK_SKEW_SECS) {
        return Err(format!(
            "observed_at {} is ahead of the forge's clock by more than {MAX_CLOCK_SKEW_SECS} seconds",
            board.observed_at
        ));
    }
    check_count("lanes", board.lanes.len(), 1, MAX_LANES)?;
    check_count("problems", board.problems.len(), 0, MAX_PROBLEMS)?;
    check_count("columns", board.columns.len(), 0, MAX_BOARD_COLUMNS)?;
    let mut column_ids = BTreeSet::new();
    for (c, column) in board.columns.iter().enumerate() {
        if !valid_id(&column.id) {
            return Err(format!("columns[{c}].id {:?} is not a valid id", column.id));
        }
        if !column_ids.insert(column.id.as_str()) {
            return Err(format!("columns[{c}].id {:?} is used twice", column.id));
        }
    }
    let mut lane_ids = BTreeSet::new();
    for (l, lane) in board.lanes.iter().enumerate() {
        if !valid_id(&lane.id) {
            return Err(format!("lanes[{l}].id {:?} is not a valid id", lane.id));
        }
        if !lane_ids.insert(lane.id.as_str()) {
            return Err(format!("lanes[{l}].id {:?} is used twice", lane.id));
        }
        if !valid_family(&lane.owner_family) {
            return Err(format!(
                "lanes[{l}].owner_family {:?} is not a family name",
                lane.owner_family
            ));
        }
        check_count(
            &format!("lanes[{l}].stages"),
            lane.stages.len(),
            1,
            MAX_STAGES,
        )?;
        let mut stage_ids = BTreeSet::new();
        for (s, stage) in lane.stages.iter().enumerate() {
            let at = format!("lanes[{l}].stages[{s}]");
            if !valid_id(&stage.id) {
                return Err(format!("{at}.id {:?} is not a valid id", stage.id));
            }
            if !stage_ids.insert(stage.id.as_str()) {
                return Err(format!("{at}.id {:?} is used twice in the lane", stage.id));
            }
            if let Some(column) = &stage.column
                && !column_ids.contains(column.as_str())
            {
                return Err(format!(
                    "{at}.column {column:?} is not one of the board's columns"
                ));
            }
            check_count(
                &format!("{at}.targets"),
                stage.targets.len(),
                0,
                MAX_TARGETS,
            )?;
            for (t, target) in stage.targets.iter().enumerate() {
                check_runners(&format!("{at}.targets[{t}]"), &target.runners)?;
            }
            if let Some(ships) = &stage.ships {
                check_count(&format!("{at}.ships"), ships.len(), 0, MAX_SHIPS)?;
            }
            if let Some(forge) = &stage.forge
                && forge
                    .repo
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .count()
                    != 2
            {
                return Err(format!(
                    "{at}.forge.repo {:?} must be owner/name",
                    forge.repo
                ));
            }
        }
    }
    if let Some(work) = &board.work {
        check_count("work.parts", work.parts.len(), 1, 6)?;
        let counted: u64 = work.parts.iter().map(|part| u64::from(part.count)).sum();
        if counted > u64::from(work.total) {
            return Err(format!(
                "work.parts add up to {counted}, more than work.total {}",
                work.total
            ));
        }
    }
    if let Some(pins) = &board.pins {
        check_count("pins.columns", pins.columns.len(), 1, MAX_PIN_COLUMNS)?;
        check_count("pins.rows", pins.rows.len(), 0, MAX_PIN_ROWS)?;
        for (r, row) in pins.rows.iter().enumerate() {
            check_count(
                &format!("pins.rows[{r}].cells"),
                row.cells.len(),
                0,
                MAX_PIN_COLUMNS,
            )?;
        }
    }
    if let Some(notes) = &board.notes {
        check_count("notes.items", notes.items.len(), 0, MAX_SHIPS)?;
    }
    let value = serde_json::to_value(board).map_err(|error| error.to_string())?;
    check_strings(&value, "board")?;
    Ok(observed)
}

fn check_runners(at: &str, runners: &[String]) -> Result<(), String> {
    check_count(
        &format!("{at}.runners"),
        runners.len(),
        0,
        MAX_TARGET_RUNNERS,
    )?;
    let mut seen = BTreeSet::new();
    for (r, runner) in runners.iter().enumerate() {
        let chars = runner.chars().count();
        if chars == 0 || chars > MAX_RUNNER_ID_CHARS {
            return Err(format!(
                "{at}.runners[{r}] is {chars} characters; allowed 1 to {MAX_RUNNER_ID_CHARS}"
            ));
        }
        if runner.chars().any(char::is_control) {
            return Err(format!(
                "{at}.runners[{r}] {runner:?} contains a control character"
            ));
        }
        if !seen.insert(runner.as_str()) {
            return Err(format!(
                "{at}.runners[{r}] {runner:?} is listed twice in the target"
            ));
        }
    }
    Ok(())
}

fn check_count(field: &str, count: usize, min: usize, max: usize) -> Result<(), String> {
    if count < min || count > max {
        return Err(format!(
            "{field} has {count} entries; allowed {min} to {max}"
        ));
    }
    Ok(())
}

fn check_strings(value: &Value, at: &str) -> Result<(), String> {
    match value {
        Value::String(text) if text.chars().count() > MAX_STRING_CHARS => Err(format!(
            "{at} is {} characters; at most {MAX_STRING_CHARS}",
            text.chars().count()
        )),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .try_for_each(|(i, item)| check_strings(item, &format!("{at}[{i}]"))),
        Value::Object(fields) => fields
            .iter()
            .try_for_each(|(key, item)| check_strings(item, &format!("{at}.{key}"))),
        _ => Ok(()),
    }
}

fn refusal(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "code": code, "message": message.into() })),
    )
        .into_response()
}

/// `PUT /api/v1/release-board/{family}`: a collector replaces a family's
/// board. Open to the named reporters and to forge admins.
pub(crate) async fn put_board(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Path(family): Path<String>,
    body: Bytes,
) -> Response {
    let admin = account.role == UserRole::Admin;
    if !state.release_boards.may_report(&account.login, admin) {
        return refusal(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "this account may not report release boards: report as a forge admin or a JERYU_BOARD_REPORTERS login",
        );
    }
    if body.len() > MAX_BODY_BYTES {
        return refusal(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!(
                "a release board is at most {MAX_BODY_BYTES} bytes; this one is {}",
                body.len()
            ),
        );
    }
    let board: ReleaseBoard = match serde_json::from_slice(&body) {
        Ok(board) => board,
        Err(error) => {
            return refusal(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                format!("not a {SCHEMA} snapshot: {error}"),
            );
        }
    };
    match state.release_boards.put(&family, board, Utc::now()) {
        Ok(accepted) => {
            if accepted.ignored.is_none() {
                publish_updated(&state, &accepted);
            }
            Json(accepted).into_response()
        }
        Err(reason) => refusal(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", reason),
    }
}

/// `GET /api/v1/release-board`: every family that has a board, newest facts
/// only. Admin-only by path.
pub(crate) async fn list_boards(State(state): State<Arc<WebState>>) -> Json<Value> {
    Json(json!({ "boards": state.release_boards.list() }))
}

/// `GET /api/v1/release-board/{family}`. Admin-only by path.
pub(crate) async fn get_board(
    State(state): State<Arc<WebState>>,
    Path(family): Path<String>,
) -> Response {
    match state.release_boards.get(&family) {
        Some(board) => Json(board).into_response(),
        // A family the forge knows nothing about is a typo in the request.
        None if !crate::web::family::known(&state)
            .contains(&crate::web::family::canonical(&family)) =>
        {
            crate::web::family::unknown(&state, &family)
        }
        None => refusal(
            StatusCode::NOT_FOUND,
            "not_found",
            format!(
                "no release board for {family:?}: the collector posts one every five minutes and after every release"
            ),
        ),
    }
}

fn publish_updated(state: &WebState, accepted: &Accepted) {
    let family = accepted.family.clone();
    let summary = format!("release board for {family} updated");
    let payload = json!({ "family": family, "observed_at": accepted.observed_at });
    state.ws.publish(PIPELINE_SCOPE, move |seq| WebEvent {
        seq,
        timestamp: crate::web::server_time(),
        scope: PIPELINE_SCOPE.to_string(),
        kind: UPDATED_KIND.to_string(),
        entity: family,
        summary,
        payload,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../../docs/release-board.example.json");

    fn board() -> ReleaseBoard {
        serde_json::from_str(FIXTURE).expect("the documented example parses")
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("test time")
            .with_timezone(&Utc)
    }

    #[test]
    fn the_documented_example_is_a_valid_board() {
        let board = board();
        let observed = validate(&board.family, &board, at("2026-09-28T16:00:00Z")).expect("valid");
        assert_eq!(observed, at("2026-09-28T15:40:00Z"));
    }

    #[test]
    fn the_example_round_trips_without_losing_fields() {
        let board = board();
        let again: ReleaseBoard =
            serde_json::from_value(serde_json::to_value(&board).expect("serialize"))
                .expect("parse");
        assert_eq!(again, board);
    }

    #[test]
    fn the_path_family_must_match_the_body() {
        let board = board();
        let error = validate("jeryu", &board, at("2026-09-28T16:00:00Z")).expect_err("mismatch");
        assert!(error.contains("must match the path"), "{error}");
    }

    #[test]
    fn a_board_from_the_future_is_refused() {
        let board = board();
        let error =
            validate(&board.family, &board, at("2026-09-28T15:00:00Z")).expect_err("future");
        assert!(error.contains("ahead of the forge's clock"), "{error}");
    }

    #[test]
    fn a_wrong_schema_is_named() {
        let mut board = board();
        board.schema = "jeryu.release_board.v0".into();
        let error =
            validate(&board.family, &board, at("2026-09-28T16:00:00Z")).expect_err("schema");
        assert!(error.contains("schema must be"), "{error}");
    }

    #[test]
    fn duplicate_stage_ids_name_the_stage() {
        let mut board = board();
        let first = board.lanes[0].stages[0].clone();
        board.lanes[0].stages.push(first);
        let error = validate(&board.family, &board, at("2026-09-28T16:00:00Z")).expect_err("dup");
        assert!(error.contains("used twice"), "{error}");
    }

    fn with_columns(mut board: ReleaseBoard) -> ReleaseBoard {
        board.columns = ["main", "dev", "stage", "prod"]
            .into_iter()
            .map(|id| Column {
                id: id.into(),
                name: id.into(),
            })
            .collect();
        board
    }

    #[test]
    fn columns_groups_and_stage_columns_are_kept() {
        let mut board = with_columns(board());
        board.lanes[0].group = Some("Apps".into());
        board.lanes[0].stages[0].column = Some("main".into());
        validate(&board.family, &board, at("2026-09-28T16:00:00Z")).expect("valid");
        let again: ReleaseBoard =
            serde_json::from_value(serde_json::to_value(&board).expect("serialize"))
                .expect("parse");
        assert_eq!(again, board);
        assert_eq!(again.columns.len(), 4);
        assert_eq!(again.lanes[0].stages[0].column.as_deref(), Some("main"));
    }

    #[test]
    fn a_stage_column_must_be_declared() {
        let mut board = with_columns(board());
        board.lanes[0].stages[0].column = Some("qa".into());
        let error =
            validate(&board.family, &board, at("2026-09-28T16:00:00Z")).expect_err("column");
        assert!(error.contains("lanes[0].stages[0].column"), "{error}");
        let mut board = self::board();
        board.lanes[0].stages[0].column = Some("main".into());
        let error = validate(&board.family, &board, at("2026-09-28T16:00:00Z"))
            .expect_err("no columns declared");
        assert!(error.contains("not one of the board's columns"), "{error}");
    }

    #[test]
    fn column_ids_are_unique() {
        let mut board = with_columns(board());
        board.columns[1].id = "main".into();
        let error = validate(&board.family, &board, at("2026-09-28T16:00:00Z")).expect_err("dup");
        assert!(error.contains("columns[1].id"), "{error}");
    }

    #[test]
    fn work_parts_cannot_exceed_the_total() {
        let mut board = board();
        let work = board.work.as_mut().expect("example has work");
        work.total = 1;
        let error = validate(&board.family, &board, at("2026-09-28T16:00:00Z")).expect_err("sum");
        assert!(error.contains("more than work.total"), "{error}");
    }

    #[test]
    fn a_long_string_is_refused_with_its_path() {
        let mut board = board();
        board.lanes[0].stages[0].status = "x".repeat(MAX_STRING_CHARS + 1);
        let error = validate(&board.family, &board, at("2026-09-28T16:00:00Z")).expect_err("long");
        assert!(error.contains("board.lanes[0].stages[0].status"), "{error}");
    }

    #[test]
    fn an_older_snapshot_does_not_replace_a_newer_one() {
        let store = ReleaseBoardStore::with_reporters(["ci-bot"]);
        let now = at("2026-09-28T16:00:00Z");
        let newer = board();
        let mut older = board();
        older.observed_at = "2026-09-28T15:30:00Z".into();
        older.summary = "older".into();
        assert!(
            store
                .put("acme", newer, now)
                .expect("newer")
                .ignored
                .is_none()
        );
        let answer = store.put("acme", older, now).expect("older answers");
        assert_eq!(answer.ignored, Some("older than stored snapshot"));
        assert_ne!(store.get("acme").expect("stored").summary, "older");
    }

    #[test]
    fn the_list_names_each_family_once_with_its_problem_count() {
        let store = ReleaseBoardStore::with_reporters(["ci-bot"]);
        let now = at("2026-09-28T16:00:00Z");
        let mut board = board();
        board.problems.push(Problem {
            source: "node-b".into(),
            message: "ssh timed out".into(),
        });
        store.put("acme", board.clone(), now).expect("first");
        store.put("acme", board, now).expect("second");
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].family, "acme");
        assert_eq!(list[0].problem_count, 1);
        assert!(!list[0].accepted_at.is_empty());
    }

    #[test]
    fn reporters_are_the_named_logins_or_admins() {
        let store = ReleaseBoardStore::with_reporters(" ci-bot , ,review-bot".split(','));
        assert!(store.may_report("ci-bot", false));
        assert!(store.may_report("review-bot", false));
        assert!(!store.may_report("jordan", false));
        assert!(store.may_report("jordan", true));
    }

    #[test]
    fn family_names_are_url_safe() {
        assert!(valid_family("acme"));
        assert!(valid_family("initech"));
        assert!(!valid_family("-initech"));
        assert!(!valid_family("Initech"));
        assert!(!valid_family("initech/web"));
        assert!(!valid_family(""));
    }
}
