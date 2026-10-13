//! The outcome projection: pipeline events in, one `outcome_ledger` row per
//! change out (`db/migrations/0019_outcome_ledger.sql`).
//!
//! Like the flow fold it runs in the same transaction as the event, reads no
//! clock, and every effect is a minimum or a set union, so the same events in
//! any number of posts give the same rows. A lot is a todo, or
//! `<owner>/<repo>#<pr>` for a pull request no todo carries; the key is the
//! flow lot key, and a pull-request lot is rewritten onto the todo that
//! adopts it.
//!
//! What each event contributes (vocabulary in `docs/pipeline-events.md`):
//! `todo.filed` the filing time, work kind and `detail.reworks`;
//! `todo.attempt_finished` with outcome `done` the agent/model arm that
//! reported done; `pr.merged`, `queue.landed` and `todo.merged` the merge and
//! its commit (and a revert, when the text names the commit it undoes);
//! `release.staged` and a successful `deploy.status` the release or deploy a
//! merged change shipped in; a `deploy.status` rollback the changes in the
//! rolled-back release; `incident.*` the incident id on the changes it names.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::web::flow::project::{EventView, for_each_event, lots_of_pr};

/// Kinds that mean a pull request or todo landed on the base branch.
const MERGED: [&str; 3] = ["pr.merged", "queue.landed", "todo.merged"];
const WORK_KINDS: [&str; 4] = ["feature", "fix", "rework", "ops"];
/// Labels that classify a change as operational work.
const OPS_LABELS: [&str; 6] = [
    "pin",
    "red-main",
    "red_main",
    "replacement",
    "revert",
    "chore",
];

/// One change as the ledger stores it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Change {
    pub lot_id: String,
    pub family: Option<String>,
    pub todo_id: Option<String>,
    pub work_kind: Option<String>,
    pub first_seen_ms: i64,
    pub filed_ms: Option<i64>,
    pub done_ms: Option<i64>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub experiment: Option<String>,
    pub reverted_ms: Option<i64>,
    pub reverted_by: Option<String>,
    pub rolled_back_ms: Option<i64>,
    pub reworks: Vec<String>,
    pub reworked_by: Vec<String>,
    pub incidents: Vec<String>,
    pub legs: Vec<Leg>,
}

/// One repository a change landed in.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Leg {
    pub repo: String,
    pub pr: Option<i64>,
    pub sha: Option<String>,
    pub merged_ms: Option<i64>,
    pub released_ms: Option<i64>,
    pub deployed_ms: Option<i64>,
    pub release: Option<String>,
}

fn min_into(slot: &mut Option<i64>, value: i64) {
    *slot = Some(slot.map_or(value, |old| old.min(value)));
}

fn union_into(set: &mut Vec<String>, value: &str) {
    if !value.is_empty() && !set.iter().any(|have| have == value) {
        set.push(value.to_string());
        set.sort();
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

/// Strings of `detail[key]`, whether it is one string or an array of them.
fn strings(detail: Option<&Value>, key: &str) -> Vec<String> {
    match detail.and_then(|d| d.get(key)) {
        Some(Value::String(one)) => vec![one.trim().to_string()],
        Some(Value::Array(many)) => many
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .collect(),
        _ => Vec::new(),
    }
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect()
}

/// A JSON string-list column. The ledger writes these itself, so a value that
/// does not parse is a corrupt row: it is reported as a conversion error on
/// that column rather than read as an empty list.
fn json_list(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Vec<String>> {
    let raw: String = row.get(index)?;
    serde_json::from_str(&raw).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(err))
    })
}

/// Project one event into the outcome ledger on `conn`, which the caller has
/// in a transaction with the event's own insert.
pub(crate) fn apply(conn: &Connection, event: &EventView<'_>) -> Result<(), String> {
    let has_reworks = event.todo_id.is_some() && !strings(event.detail, "reworks").is_empty();
    let relevant = matches!(event.kind, "todo.filed" | "todo.attempt_finished")
        || MERGED.contains(&event.kind)
        || event.kind == "release.staged"
        || event.kind == "deploy.status"
        || event.kind == "pin.bump_opened"
        || event.kind.starts_with("incident.")
        || has_reworks;
    if !relevant || !first_projection(conn, event)? {
        return Ok(());
    }
    match event.kind {
        "release.staged" => return ship(conn, event, Ship::Released),
        "deploy.status" => return deploy_status(conn, event),
        k if k.starts_with("incident.") => return incident(conn, event),
        _ => {}
    }
    let lots = lots_for(conn, event)?;
    for id in lots {
        let mut change = load_or_new(conn, &id, || Change {
            lot_id: id.clone(),
            first_seen_ms: event.ts_ms,
            ..Change::default()
        })?;
        if change.family.is_none() {
            change.family = event.family.map(ToString::to_string);
        }
        if !id.contains('#') || event.todo_id == Some(id.as_str()) {
            change.todo_id = Some(id.clone()).filter(|_| !id.contains('#'));
        }
        change.first_seen_ms = change.first_seen_ms.min(event.ts_ms);
        let explicit = classify(event.detail);
        if let Some(kind) = explicit {
            change.work_kind.get_or_insert(kind);
        }
        match event.kind {
            "todo.filed" => min_into(&mut change.filed_ms, event.ts_ms),
            "todo.attempt_finished" if event.outcome == Some("done") => {
                min_into(&mut change.done_ms, event.ts_ms);
                let detail = event.detail;
                change.agent = text(detail.and_then(|d| d.get("agent")));
                change.model = text(detail.and_then(|d| d.get("model")));
                change.experiment = text(detail.and_then(|d| d.get("experiment")));
            }
            "pin.bump_opened" => {
                change.work_kind.get_or_insert("ops".to_string());
            }
            kind if MERGED.contains(&kind) => merge_leg(&mut change, event),
            _ => {}
        }
        let reworks = strings(event.detail, "reworks");
        for original in &reworks {
            if original != &id {
                union_into(&mut change.reworks, original);
            }
        }
        if !change.reworks.is_empty() {
            // A link to what it reworks makes the change rework, whatever
            // else it was labelled.
            change.work_kind = Some("rework".to_string());
        }
        save(conn, &change)?;
        if MERGED.contains(&event.kind) {
            record_sha(conn, &change, event)?;
            revert(conn, &mut change, event)?;
            save(conn, &change)?;
        }
        for original in reworks.iter().filter(|o| **o != id) {
            let mut base = load_or_new(conn, original, || Change {
                lot_id: original.clone(),
                todo_id: Some(original.clone()),
                family: event.family.map(ToString::to_string),
                first_seen_ms: event.ts_ms,
                ..Change::default()
            })?;
            union_into(&mut base.reworked_by, &id);
            save(conn, &base)?;
        }
    }
    Ok(())
}

/// Work kind from the event's `detail.kind` or `detail.labels`: one of
/// feature, fix, rework, ops. Pins, red-main repair, replacement PRs and
/// reverts count as ops.
fn classify(detail: Option<&Value>) -> Option<String> {
    let mut words = strings(detail, "kind");
    words.extend(strings(detail, "labels"));
    words.extend(strings(detail, "work_kind"));
    let words: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    for kind in WORK_KINDS {
        if words.iter().any(|w| w == kind) {
            return Some(kind.to_string());
        }
    }
    words
        .iter()
        .any(|w| OPS_LABELS.contains(&w.as_str()))
        .then(|| "ops".to_string())
}

fn merge_leg(change: &mut Change, event: &EventView<'_>) {
    let leg = leg_for(change, event.repo);
    min_into(&mut leg.merged_ms, event.ts_ms);
    if leg.pr.is_none() {
        leg.pr = event.pr;
    }
    if let Some(sha) = event.sha.filter(|s| !s.is_empty()) {
        leg.sha.get_or_insert_with(|| sha.to_string());
    }
}

/// The leg for `repo`: an event without a repository names the only leg the
/// change has, and a repository arriving later claims the unnamed leg.
fn leg_for<'a>(change: &'a mut Change, repo: Option<&str>) -> &'a mut Leg {
    let name = match repo.filter(|r| !r.is_empty()) {
        Some(repo) => repo.to_string(),
        None if change.legs.len() == 1 => change.legs[0].repo.clone(),
        None => String::new(),
    };
    if !name.is_empty()
        && !change.legs.iter().any(|l| l.repo == name)
        && let Some(unnamed) = change.legs.iter_mut().find(|l| l.repo.is_empty())
    {
        unnamed.repo = name.clone();
    }
    if !change.legs.iter().any(|l| l.repo == name) {
        change.legs.push(Leg {
            repo: name.clone(),
            ..Leg::default()
        });
        change.legs.sort_by(|a, b| a.repo.cmp(&b.repo));
    }
    change
        .legs
        .iter_mut()
        .find(|l| l.repo == name)
        .expect("leg just ensured")
}

fn record_sha(conn: &Connection, change: &Change, event: &EventView<'_>) -> Result<(), String> {
    let mut shas = strings(event.detail, "shas");
    shas.extend(strings(event.detail, "commits"));
    shas.extend(event.sha.map(ToString::to_string));
    for sha in shas {
        if sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
            conn.execute(
                "INSERT OR IGNORE INTO outcome_ledger_shas (sha, repo, lot_id) VALUES (?1, ?2, ?3)",
                params![sha.to_ascii_lowercase(), event.repo, change.lot_id],
            )
            .map_err(|err| err.to_string())?;
        }
    }
    Ok(())
}

/// Hex tokens of 7 to 40 characters with a digit in them: the shape of a
/// commit id in a subject or body.
fn hex_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| (7..=40).contains(&t.len()))
        .filter(|t| t.chars().all(|c| c.is_ascii_hexdigit()))
        .filter(|t| t.chars().any(|c| c.is_ascii_digit()))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The commit ids (or lot ids) an event says it reverts: a `This reverts
/// commit <sha>` trailer, a "Revert" subject naming a sha, or an explicit
/// `detail.reverts`.
pub(crate) fn reverted_refs(event: &EventView<'_>) -> Vec<String> {
    let mut sources: Vec<String> = event.summary.map(ToString::to_string).into_iter().collect();
    let detail = event.detail;
    for key in ["subject", "title", "message", "body", "commit_message"] {
        sources.extend(text(detail.and_then(|d| d.get(key))));
    }
    if let Some(Value::Array(commits)) = detail.and_then(|d| d.get("commits")) {
        for commit in commits {
            match commit {
                Value::String(s) => sources.push(s.clone()),
                Value::Object(_) => {
                    for key in ["subject", "message"] {
                        sources.extend(text(commit.get(key)));
                    }
                }
                _ => {}
            }
        }
    }
    let mut refs = strings(detail, "reverts");
    for source in &sources {
        let mut rest = source.as_str();
        while let Some(at) = rest.to_ascii_lowercase().find("this reverts commit ") {
            rest = &rest[at + "this reverts commit ".len()..];
            refs.extend(hex_tokens(
                rest.split_whitespace().next().unwrap_or_default(),
            ));
        }
        let first = source.lines().next().unwrap_or_default().trim_start();
        if first.to_ascii_lowercase().starts_with("revert") {
            refs.extend(hex_tokens(source));
        }
    }
    refs.sort();
    refs.dedup();
    refs
}

/// Mark the changes this event reverts: the lot whose commit a token names
/// is reverted by this change, once and only when the join is unique.
fn revert(conn: &Connection, change: &mut Change, event: &EventView<'_>) -> Result<(), String> {
    for token in reverted_refs(event) {
        let mut hits: Vec<String> = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT DISTINCT lot_id FROM outcome_ledger_shas WHERE sha LIKE ?1 || '%'")
                .map_err(|err| err.to_string())?;
            let rows = stmt
                .query_map(params![token], |row| row.get::<_, String>(0))
                .map_err(|err| err.to_string())?;
            for row in rows {
                hits.push(row.map_err(|err| err.to_string())?);
            }
        }
        if hits.is_empty() && load(conn, &token)?.is_some() {
            hits.push(token.clone());
        }
        hits.retain(|lot| *lot != change.lot_id);
        let [original] = hits.as_slice() else {
            continue;
        };
        let mut original = load(conn, original)?.expect("sha names a stored change");
        min_into(&mut original.reverted_ms, event.ts_ms);
        original
            .reverted_by
            .get_or_insert_with(|| change.lot_id.clone());
        save(conn, &original)?;
        change.work_kind.get_or_insert("ops".to_string());
    }
    Ok(())
}

enum Ship {
    Released,
    Deployed,
}

/// A release or deploy carries every merged change of its repository (and of
/// each repository in `detail.carries`, for a deploy repo that pins others)
/// that was on the base branch when it happened.
fn ship(conn: &Connection, event: &EventView<'_>, how: Ship) -> Result<(), String> {
    let mut repos: Vec<String> = event.repo.map(ToString::to_string).into_iter().collect();
    repos.extend(strings(event.detail, "carries"));
    let release = text(event.detail.and_then(|d| d.get("release")))
        .or_else(|| text(event.detail.and_then(|d| d.get("tag"))))
        .or_else(|| event.sha.map(ToString::to_string));
    let column = match how {
        Ship::Released => "released_ms",
        Ship::Deployed => "deployed_ms",
    };
    for repo in repos {
        conn.execute(
            &format!(
                "UPDATE outcome_ledger_repos SET {column} = ?2, release = COALESCE(release, ?3)
                  WHERE repo = ?1 AND merged_ms IS NOT NULL AND merged_ms <= ?2
                    AND {column} IS NULL
                    AND (?4 IS NULL OR lot_id IN
                         (SELECT lot_id FROM outcome_ledger WHERE family = ?4))"
            ),
            params![repo, event.ts_ms, release, event.family],
        )
        .map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn deploy_status(conn: &Connection, event: &EventView<'_>) -> Result<(), String> {
    let rolled_back = matches!(
        event.outcome,
        Some("rollback" | "rolled_back" | "rollback_success")
    ) || event
        .detail
        .and_then(|d| d.get("rollback"))
        .and_then(Value::as_bool)
        == Some(true);
    if rolled_back {
        return rollback(conn, event);
    }
    if event.outcome == Some("success") {
        return ship(conn, event, Ship::Deployed);
    }
    Ok(())
}

/// A rollback undoes the changes of one release: the one `detail.rolled_back`
/// names, else `detail.release`, else the newest release the repository
/// deployed.
fn rollback(conn: &Connection, event: &EventView<'_>) -> Result<(), String> {
    let Some(repo) = event.repo else {
        return Ok(());
    };
    let target = text(event.detail.and_then(|d| d.get("rolled_back")))
        .or_else(|| text(event.detail.and_then(|d| d.get("release"))))
        .or_else(|| {
            conn.query_row(
                "SELECT release FROM outcome_ledger_repos
                  WHERE repo = ?1 AND deployed_ms IS NOT NULL AND deployed_ms <= ?2
                  ORDER BY deployed_ms DESC LIMIT 1",
                params![repo, event.ts_ms],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten()
        });
    let Some(target) = target else {
        return Ok(());
    };
    conn.execute(
        "UPDATE outcome_ledger SET rolled_back_ms = MIN(COALESCE(rolled_back_ms, ?3), ?3)
          WHERE lot_id IN (SELECT lot_id FROM outcome_ledger_repos
                            WHERE repo = ?1 AND release = ?2 AND deployed_ms IS NOT NULL)",
        params![repo, target, event.ts_ms],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

/// An incident event names the changes it is traced to by todo, by commit, or
/// by the lots of a pull request; an incident that names none is not guessed.
fn incident(conn: &Connection, event: &EventView<'_>) -> Result<(), String> {
    let detail = event.detail;
    let id = ["incident_id", "incident", "id"]
        .iter()
        .find_map(|key| text(detail.and_then(|d| d.get(*key))));
    let Some(id) = id else {
        return Ok(());
    };
    let mut lots: Vec<String> = event.todo_id.map(ToString::to_string).into_iter().collect();
    lots.extend(strings(detail, "todo_ids"));
    let mut shas = strings(detail, "shas");
    shas.extend(strings(detail, "sha"));
    shas.extend(strings(detail, "cause_sha"));
    shas.extend(event.sha.map(ToString::to_string));
    for sha in shas {
        let hit = conn
            .query_row(
                "SELECT lot_id FROM outcome_ledger_shas WHERE sha LIKE ?1 || '%' LIMIT 1",
                params![sha.to_ascii_lowercase()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|err| err.to_string())?;
        lots.extend(hit);
    }
    if let (Some(repo), Some(pr)) = (event.repo, event.pr) {
        lots.extend(lots_of_pr(conn, repo, pr)?);
    }
    lots.sort();
    lots.dedup();
    for lot in lots {
        let mut change = load_or_new(conn, &lot, || Change {
            lot_id: lot.clone(),
            todo_id: Some(lot.clone()).filter(|l| !l.contains('#')),
            family: event.family.map(ToString::to_string),
            first_seen_ms: event.ts_ms,
            ..Change::default()
        })?;
        union_into(&mut change.incidents, &id);
        save(conn, &change)?;
    }
    Ok(())
}

/// The lots an event speaks about, with a pull-request lot adopted by the
/// todo that carries it.
fn lots_for(conn: &Connection, event: &EventView<'_>) -> Result<Vec<String>, String> {
    if let Some(todo) = event.todo_id.map(str::trim).filter(|t| !t.is_empty()) {
        if let (Some(repo), Some(pr)) = (event.repo, event.pr) {
            adopt(conn, todo, &format!("{repo}#{pr}"))?;
        }
        return Ok(vec![todo.to_string()]);
    }
    let (Some(repo), Some(pr)) = (event.repo, event.pr) else {
        return Ok(Vec::new());
    };
    let mut lots = lots_of_pr(conn, repo, pr)?;
    if lots.is_empty() {
        let mut stmt = conn
            .prepare("SELECT lot_id FROM outcome_ledger_repos WHERE repo = ?1 AND pr = ?2")
            .map_err(|err| err.to_string())?;
        lots = stmt
            .query_map(params![repo, pr], |row| row.get::<_, String>(0))
            .map_err(|err| err.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|err| err.to_string())?;
    }
    if lots.is_empty() {
        lots.push(format!("{repo}#{pr}"));
    }
    Ok(lots)
}

/// Fold the row of a pull-request lot into the todo's, then drop it.
fn adopt(conn: &Connection, todo: &str, pr_lot: &str) -> Result<(), String> {
    if todo == pr_lot {
        return Ok(());
    }
    let Some(from) = load(conn, pr_lot)? else {
        return Ok(());
    };
    let mut into = load_or_new(conn, todo, || Change {
        lot_id: todo.to_string(),
        first_seen_ms: from.first_seen_ms,
        ..Change::default()
    })?;
    into.todo_id = Some(todo.to_string());
    into.family = into.family.or(from.family);
    into.work_kind = into.work_kind.or(from.work_kind);
    into.first_seen_ms = into.first_seen_ms.min(from.first_seen_ms);
    for (slot, value) in [
        (&mut into.filed_ms, from.filed_ms),
        (&mut into.done_ms, from.done_ms),
        (&mut into.reverted_ms, from.reverted_ms),
        (&mut into.rolled_back_ms, from.rolled_back_ms),
    ] {
        if let Some(value) = value {
            min_into(slot, value);
        }
    }
    into.reverted_by = into.reverted_by.or(from.reverted_by);
    into.agent = into.agent.or(from.agent);
    into.model = into.model.or(from.model);
    into.experiment = into.experiment.or(from.experiment);
    for list in [
        (&mut into.reworks, &from.reworks),
        (&mut into.reworked_by, &from.reworked_by),
        (&mut into.incidents, &from.incidents),
    ] {
        for value in list.1 {
            union_into(list.0, value);
        }
    }
    for leg in from.legs {
        match into.legs.iter_mut().find(|l| l.repo == leg.repo) {
            Some(have) => {
                have.pr = have.pr.or(leg.pr);
                have.sha = have.sha.clone().or(leg.sha);
                have.release = have.release.clone().or(leg.release);
                for (slot, value) in [
                    (&mut have.merged_ms, leg.merged_ms),
                    (&mut have.released_ms, leg.released_ms),
                    (&mut have.deployed_ms, leg.deployed_ms),
                ] {
                    if let Some(value) = value {
                        min_into(slot, value);
                    }
                }
            }
            None => into.legs.push(leg),
        }
    }
    into.legs.sort_by(|a, b| a.repo.cmp(&b.repo));
    conn.execute(
        "DELETE FROM outcome_ledger_repos WHERE lot_id = ?1",
        params![pr_lot],
    )
    .map_err(|err| err.to_string())?;
    conn.execute(
        "DELETE FROM outcome_ledger WHERE lot_id = ?1",
        params![pr_lot],
    )
    .map_err(|err| err.to_string())?;
    conn.execute(
        "UPDATE outcome_ledger_shas SET lot_id = ?1 WHERE lot_id = ?2",
        params![todo, pr_lot],
    )
    .map_err(|err| err.to_string())?;
    // Other changes that pointed at the pull-request lot now point at the todo.
    conn.execute(
        "UPDATE outcome_ledger SET reverted_by = ?1 WHERE reverted_by = ?2",
        params![todo, pr_lot],
    )
    .map_err(|err| err.to_string())?;
    save(conn, &into)
}

/// First time this event reaches the outcome ledger; an event without an id
/// is always projected, which is safe because every effect is idempotent.
fn first_projection(conn: &Connection, event: &EventView<'_>) -> Result<bool, String> {
    let Some(event_id) = event.event_id.filter(|id| !id.is_empty()) else {
        return Ok(true);
    };
    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO outcome_ledger_events (reporter, event_id, seq)
             VALUES (?1, ?2, ?3)",
            params![event.reporter, event_id, event.seq],
        )
        .map_err(|err| err.to_string())?;
    Ok(inserted == 1)
}

/// The stored change for `lot_id`, or the one `new` describes when the ledger
/// has no row for it yet: a change's first event creates its row.
fn load_or_new(
    conn: &Connection,
    lot_id: &str,
    new: impl FnOnce() -> Change,
) -> Result<Change, String> {
    match load(conn, lot_id)? {
        Some(change) => Ok(change),
        None => Ok(new()),
    }
}

fn load(conn: &Connection, lot_id: &str) -> Result<Option<Change>, String> {
    let change = conn
        .query_row(
            "SELECT lot_id, family, todo_id, work_kind, first_seen_ms, filed_ms, done_ms,
                    agent, model, experiment, reverted_ms, reverted_by, rolled_back_ms,
                    reworks, reworked_by, incidents
               FROM outcome_ledger WHERE lot_id = ?1",
            params![lot_id],
            change_from_row,
        )
        .optional()
        .map_err(|err| err.to_string())?;
    let Some(mut change) = change else {
        return Ok(None);
    };
    change.legs = legs_of(conn, lot_id)?;
    Ok(Some(change))
}

fn change_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Change> {
    Ok(Change {
        lot_id: row.get(0)?,
        family: row.get(1)?,
        todo_id: row.get(2)?,
        work_kind: row.get(3)?,
        first_seen_ms: row.get(4)?,
        filed_ms: row.get(5)?,
        done_ms: row.get(6)?,
        agent: row.get(7)?,
        model: row.get(8)?,
        experiment: row.get(9)?,
        reverted_ms: row.get(10)?,
        reverted_by: row.get(11)?,
        rolled_back_ms: row.get(12)?,
        reworks: json_list(row, 13)?,
        reworked_by: json_list(row, 14)?,
        incidents: json_list(row, 15)?,
        legs: Vec::new(),
    })
}

fn legs_of(conn: &Connection, lot_id: &str) -> Result<Vec<Leg>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT repo, pr, sha, merged_ms, released_ms, deployed_ms, release
               FROM outcome_ledger_repos WHERE lot_id = ?1 ORDER BY repo",
        )
        .map_err(|err| err.to_string())?;
    stmt.query_map(params![lot_id], |row| {
        Ok(Leg {
            repo: row.get(0)?,
            pr: row.get(1)?,
            sha: row.get(2)?,
            merged_ms: row.get(3)?,
            released_ms: row.get(4)?,
            deployed_ms: row.get(5)?,
            release: row.get(6)?,
        })
    })
    .map_err(|err| err.to_string())?
    .collect::<Result<_, _>>()
    .map_err(|err| err.to_string())
}

fn save(conn: &Connection, change: &Change) -> Result<(), String> {
    let list = |v: &Vec<String>| serde_json::to_string(v).map_err(|err| err.to_string());
    conn.execute(
        "INSERT INTO outcome_ledger (lot_id, family, todo_id, work_kind, first_seen_ms, filed_ms,
             done_ms, agent, model, experiment, reverted_ms, reverted_by, rolled_back_ms,
             reworks, reworked_by, incidents)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT (lot_id) DO UPDATE SET
             family = excluded.family, todo_id = excluded.todo_id,
             work_kind = excluded.work_kind, first_seen_ms = excluded.first_seen_ms,
             filed_ms = excluded.filed_ms, done_ms = excluded.done_ms,
             agent = excluded.agent, model = excluded.model, experiment = excluded.experiment,
             reverted_ms = excluded.reverted_ms, reverted_by = excluded.reverted_by,
             rolled_back_ms = excluded.rolled_back_ms, reworks = excluded.reworks,
             reworked_by = excluded.reworked_by, incidents = excluded.incidents",
        params![
            change.lot_id,
            change.family,
            change.todo_id,
            change.work_kind,
            change.first_seen_ms,
            change.filed_ms,
            change.done_ms,
            change.agent,
            change.model,
            change.experiment,
            change.reverted_ms,
            change.reverted_by,
            change.rolled_back_ms,
            list(&change.reworks)?,
            list(&change.reworked_by)?,
            list(&change.incidents)?,
        ],
    )
    .map_err(|err| err.to_string())?;
    for leg in &change.legs {
        conn.execute(
            "INSERT INTO outcome_ledger_repos (lot_id, repo, pr, sha, merged_ms, released_ms,
                 deployed_ms, release)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (lot_id, repo) DO UPDATE SET
                 pr = excluded.pr, sha = excluded.sha, merged_ms = excluded.merged_ms,
                 released_ms = excluded.released_ms, deployed_ms = excluded.deployed_ms,
                 release = excluded.release",
            params![
                change.lot_id,
                leg.repo,
                leg.pr,
                leg.sha,
                leg.merged_ms,
                leg.released_ms,
                leg.deployed_ms,
                leg.release,
            ],
        )
        .map_err(|err| err.to_string())?;
    }
    // A leg renamed from the unnamed one must not leave the old row behind.
    if change.legs.iter().all(|l| !l.repo.is_empty()) {
        conn.execute(
            "DELETE FROM outcome_ledger_repos WHERE lot_id = ?1 AND repo = ''",
            params![change.lot_id],
        )
        .map_err(|err| err.to_string())?;
    }
    Ok(())
}

/// Every change the ledger holds for `family` and, when named, `repo` (any
/// repository of a multi-repository change).
pub(crate) fn read(
    conn: &Connection,
    family: Option<&str>,
    repo: Option<&str>,
) -> Result<Vec<Change>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT lot_id FROM outcome_ledger o
              WHERE (?1 IS NULL OR o.family = ?1)
                AND (?2 IS NULL OR EXISTS (SELECT 1 FROM outcome_ledger_repos r
                                            WHERE r.lot_id = o.lot_id AND r.repo = ?2))
              ORDER BY o.lot_id",
        )
        .map_err(|err| err.to_string())?;
    let ids: Vec<String> = stmt
        .query_map(params![family, repo], |row| row.get(0))
        .map_err(|err| err.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|err| err.to_string())?;
    drop(stmt);
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.extend(load(conn, &id)?);
    }
    Ok(out)
}

/// Every retained event projected into an empty ledger, oldest first. Answers
/// how many events were read and how many changes they produced.
pub(crate) fn replay(conn: &Connection) -> Result<(i64, i64), String> {
    let read = for_each_event(conn, |event| apply(conn, event))?;
    let changes = conn
        .query_row("SELECT COUNT(*) FROM outcome_ledger", [], |row| row.get(0))
        .map_err(|err| err.to_string())?;
    Ok((read, changes))
}
