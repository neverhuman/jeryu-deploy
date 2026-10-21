//! Review catch rate: what review actually caught, next to what it cost.
//!
//! Findings arrive as `review.finding` events (the agent reviewer posts its
//! own; the forge emits one when a human requests changes or leaves a line
//! comment). A finding is resolved by a `review.resolved` event whose
//! `outcome` is `caught` (the code changed before merge) or `not_acted`
//! (dismissed, or unchanged at merge). The forge writes the resolution when
//! the pull request merges ([`resolve_at_merge`]); a reviewer may write it
//! sooner, once a later head no longer shows the finding.
//!
//! A defect in a change that passed review is a `defect.linked` event on that
//! change (`detail.cause` is `revert`, `rework` or `incident`). It counts in
//! the denominator: catch rate = caught / (caught + escaped past review),
//! split by reviewer kind (`human`, `agent`). The forge links all three
//! causes itself: a revert names the change it undoes
//! ([`link_revert`]), a todo filed to redo another one names that todo's
//! changes, and an `incident.caused_by` names the changes it blames
//! ([`link_defect`]).
//! Contract: `docs/pipeline-events.md`, "Review catch rate".

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::WebState;
use super::pipeline::emit::{PullEvent, pull};
use super::pipeline::{Event, EventStore, EventsQuery, NewEvent, emit};
use jeryu_core::PullRequest;

pub(crate) const FINDING_KIND: &str = "review.finding";
pub(crate) const RESOLVED_KIND: &str = "review.resolved";
pub(crate) const DEFECT_KIND: &str = "defect.linked";
pub(crate) const HUMAN: &str = "human";
pub(crate) const AGENT: &str = "agent";

/// Kinds that start the review clock.
const REVIEW_STARTS: &[&str] = &["pr.review_requested", "pr.ready_for_review"];

/// A finding as the event log holds it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Finding {
    pub key: String,
    pub repo: String,
    pub pr: i64,
    pub sha: Option<String>,
    pub reviewer_kind: String,
    pub path: Option<String>,
}

/// The key a `review.resolved` names a finding by: its `event_id`, or its
/// sequence number for forge-emitted findings, which carry none.
pub(crate) fn finding_key(event: &Event) -> String {
    event
        .event_id
        .clone()
        .unwrap_or_else(|| format!("seq-{}", event.seq))
}

fn detail_str<'a>(event: &'a Event, key: &str) -> Option<&'a str> {
    event.detail.as_ref()?.get(key)?.as_str()
}

fn finding_of(event: &Event) -> Option<Finding> {
    if event.kind != FINDING_KIND {
        return None;
    }
    Some(Finding {
        key: finding_key(event),
        repo: event.repo.clone()?,
        pr: event.pr?,
        sha: event.sha.clone(),
        reviewer_kind: match detail_str(event, "reviewer_kind") {
            Some(AGENT) => AGENT,
            _ => HUMAN,
        }
        .to_string(),
        path: detail_str(event, "path")
            .filter(|p| !p.is_empty())
            .map(str::to_string),
    })
}

/// One finding a human left: a line comment, or the request-changes verdict
/// itself when it carries no comment.
pub(crate) struct HumanFinding {
    pub path: Option<String>,
    pub line: Option<u64>,
    pub body: Option<String>,
}

/// `review.finding` for what a human reviewer just submitted: each file
/// comment is a finding; a request-changes with no file comment is one
/// finding on the pull request as a whole.
pub(crate) fn human_findings(
    state: &WebState,
    pr: &PullRequest,
    reviewer: &str,
    verdict: &str,
    comments: Vec<HumanFinding>,
) {
    // Automation owns its structured finding events; a route verdict must not
    // create a second finding or label the automation as a person.
    if !super::people::is_person(state, reviewer) {
        return;
    }
    let mut findings: Vec<HumanFinding> = comments
        .into_iter()
        .filter(|c| c.path.as_deref().is_some_and(|p| !p.is_empty()))
        .collect();
    if findings.is_empty() && verdict == "request_changes" {
        findings.push(HumanFinding {
            path: None,
            line: None,
            body: None,
        });
    }
    for finding in findings {
        let severity = if verdict == "request_changes" {
            "blocking"
        } else {
            "comment"
        };
        let place = finding.path.clone().unwrap_or_else(|| "the change".into());
        pull(
            state,
            pr,
            PullEvent {
                kind: FINDING_KIND,
                actor: reviewer,
                outcome: Some(severity),
                needs_human: false,
                summary: format!(
                    "{reviewer} left a {severity} finding on {}/{}#{} ({place})",
                    pr.owner, pr.repo, pr.number
                ),
                reason: finding
                    .body
                    .as_deref()
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .map(|b| b.chars().take(500).collect()),
                sha: None,
                detail: Some(json!({
                    "reviewer_kind": HUMAN,
                    "severity": severity,
                    "path": finding.path,
                    "line": finding.line,
                })),
            },
        );
    }
}

fn is_commit_oid(oid: &str) -> bool {
    (7..=64).contains(&oid.len()) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Paths changed between two commits, or `None` when the range cannot be read.
fn touched_paths(state: &WebState, pr: &PullRequest, from: &str, to: &str) -> Option<Vec<String>> {
    if !is_commit_oid(from) || !is_commit_oid(to) {
        return None;
    }
    let resolved = state.repo_manager.open_parts(&pr.owner, &pr.repo).ok()?;
    let bare = resolved.path.to_string_lossy().to_string();
    let out = std::process::Command::new(&state.repo_manager.config().git_bin)
        .args([
            "-C",
            &bare,
            "diff",
            "--name-only",
            "--end-of-options",
            from,
            to,
            "--",
        ])
        .output()
        .ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    })
}

/// How one finding ended: caught when a later commit touched its file (or,
/// for a finding about the change as a whole, moved the head), `not_acted`
/// otherwise.
pub(crate) fn verdict_for(
    finding: &Finding,
    head: &str,
    touched: &[String],
) -> (&'static str, &'static str) {
    let moved = finding.sha.as_deref().is_some_and(|sha| sha != head);
    match &finding.path {
        Some(path) if touched.iter().any(|t| t == path) => ("caught", "file_changed"),
        Some(_) => ("not_acted", "unchanged_at_merge"),
        None if moved => ("caught", "head_changed"),
        None => ("not_acted", "unchanged_at_merge"),
    }
}

/// Rows per page of [`pr_events`], and the most pages it walks: a pull
/// request with more findings than that resolves the newest
/// `PR_EVENT_PAGE * PR_EVENT_PAGES` of them, which is already far past what a
/// reviewer leaves, and the walk stays bounded.
const PR_EVENT_PAGE: i64 = 500;
const PR_EVENT_PAGES: usize = 20;

/// Every event of `kind` the log keeps for one pull request, newest first.
///
/// The store's default page is 100 rows (`pipeline::store`), so a single
/// query would silently resolve only the newest 100 findings of a pull
/// request the agent reviewer posted more on; this walks the pages.
fn pr_events(store: &EventStore, kind: &str, repo: &str, pr: u64) -> Result<Vec<Event>, String> {
    let number = i64::try_from(pr).map_err(|err| err.to_string())?;
    let mut out = Vec::new();
    let mut before: Option<i64> = None;
    for page_number in 0..PR_EVENT_PAGES {
        let page = store.query(&EventsQuery {
            kind: Some(kind.to_string()),
            repo: Some(repo.to_string()),
            pr: Some(number),
            before_seq: before,
            limit: Some(PR_EVENT_PAGE),
            ..Default::default()
        })?;
        let Some(last) = page.last() else { break };
        before = Some(last.seq);
        let full = i64::try_from(page.len()).unwrap_or(i64::MAX) >= PR_EVENT_PAGE;
        out.extend(page);
        if !full {
            break;
        }
        if page_number + 1 == PR_EVENT_PAGES {
            eprintln!(
                "jeryu-api review catch: {repo}#{pr}: more than {} {kind} events; older ones are left as they are",
                PR_EVENT_PAGE * i64::try_from(PR_EVENT_PAGES).unwrap_or(1)
            );
        }
    }
    Ok(out)
}

/// The resolution step: on merge, every finding of the pull request that no
/// reviewer has resolved is `caught` or `not_acted`. Best-effort.
pub(crate) fn resolve_at_merge(state: &WebState, pr: &PullRequest) {
    let repo = format!("{}/{}", pr.owner, pr.repo);
    let finding_events = match pr_events(&state.events, FINDING_KIND, &repo, pr.number) {
        Ok(events) => events,
        Err(reason) => {
            eprintln!(
                "jeryu-api review catch: {repo}#{}: findings unavailable; leaving resolutions unchanged: {reason}",
                pr.number
            );
            return;
        }
    };
    let findings: Vec<Finding> = finding_events.iter().filter_map(finding_of).collect();
    if findings.is_empty() {
        return;
    }
    let resolution_events = match pr_events(&state.events, RESOLVED_KIND, &repo, pr.number) {
        Ok(events) => events,
        Err(reason) => {
            eprintln!(
                "jeryu-api review catch: {repo}#{}: prior resolutions unavailable; leaving resolutions unchanged: {reason}",
                pr.number
            );
            return;
        }
    };
    let done: BTreeSet<String> = resolution_events
        .iter()
        .filter_map(|e| detail_str(e, "finding").map(str::to_string))
        .collect();
    let mut diffs: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
    for finding in findings.iter().filter(|f| !done.contains(&f.key)) {
        let touched = match finding.sha.as_deref() {
            Some(sha) if sha != pr.head.sha => diffs
                .entry(sha.to_string())
                .or_insert_with(|| touched_paths(state, pr, sha, &pr.head.sha))
                .clone(),
            _ => Some(Vec::new()),
        };
        // An unreadable range leaves the finding open rather than guessing.
        let Some(touched) = touched else { continue };
        let (outcome, reason) = verdict_for(finding, &pr.head.sha, &touched);
        emit(
            state,
            NewEvent {
                // One resolution per finding: a merge emitted twice (a replay,
                // a restart) records the first verdict once instead of a
                // second row the catch report would count again.
                event_id: Some(resolution_id(&repo, pr.number, &finding.key)),
                actor: Some("forge".to_string()),
                repo: Some(repo.clone()),
                pr: i64::try_from(pr.number).ok(),
                sha: Some(pr.head.sha.clone()),
                outcome: Some(outcome.to_string()),
                detail: Some(json!({
                    "finding": finding.key,
                    "reviewer_kind": finding.reviewer_kind,
                    "reason": reason,
                })),
                ..NewEvent::forge(
                    RESOLVED_KIND,
                    format!("finding on {repo}#{} was {outcome} ({reason})", pr.number),
                )
            },
        );
    }
}

/// The `event_id` of the forge's merge-time resolution of one finding.
fn resolution_id(repo: &str, pr: u64, finding: &str) -> String {
    let digest = Sha256::digest(format!("{repo}\n{pr}\n{finding}").as_bytes());
    format!("resolved-{}", &hex::encode(digest)[..24])
}

/// Only a dedicated `Reverts #N` body line names a reverted change. Arbitrary
/// issue links in prose are not defect evidence. Bound parsing and emission.
fn reverted_pr_numbers(body: &str) -> BTreeSet<u64> {
    body.lines()
        .take(1_000)
        .filter_map(|line| {
            let number = line.trim().strip_prefix("Reverts #")?.trim_end_matches('.');
            if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            number.parse::<u64>().ok().filter(|number| *number > 0)
        })
        .take(50)
        .collect()
}

/// A dedicated revert declaration links only an existing merged PR in this
/// repository. Repeated merge emission records the same stable defect once.
pub(crate) fn link_revert(state: &WebState, pr: &PullRequest) {
    if !pr
        .title
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("revert ")
    {
        return;
    }
    let repo = format!("{}/{}", pr.owner, pr.repo);
    for number in reverted_pr_numbers(pr.body.as_deref().unwrap_or_default()) {
        if number == pr.number
            || !state
                .core
                .get_pull_request(&pr.owner, &pr.repo, number)
                .is_ok_and(|target| target.merged)
        {
            continue;
        }
        let Ok(number) = i64::try_from(number) else {
            continue;
        };
        emit(
            state,
            NewEvent {
                event_id: Some(defect_id(
                    "revert",
                    &format!("{repo}#{}", pr.number),
                    &repo,
                    number,
                )),
                actor: Some("forge".to_string()),
                repo: Some(repo.clone()),
                pr: Some(number),
                outcome: Some("revert".to_string()),
                detail: Some(json!({"cause": "revert", "by_pr": pr.number})),
                ..NewEvent::forge(
                    DEFECT_KIND,
                    format!("{repo}#{number} was reverted by #{}", pr.number),
                )
            },
        );
    }
}

/// The pull request a change lot key names on its own: `<owner>/<repo>#<N>`.
fn pr_of_key(key: &str) -> Option<(String, i64)> {
    let (repo, number) = key.rsplit_once('#')?;
    let number = number.parse::<i64>().ok()?;
    repo.contains('/')
        .then(|| (repo.to_string(), number))
        .filter(|_| number > 0)
}

/// The changes a lot key names: the pull request it is, or every pull request
/// the flow ledger bound to that lot. A todo id is a lot too, and its work
/// landed as one change per repository, so a rework names them all.
fn changes_of(state: &WebState, key: &str) -> Vec<(String, i64)> {
    if let Some(one) = pr_of_key(key) {
        return vec![one];
    }
    state
        .events
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT repo, pr FROM flow_ledger
                      WHERE (lot_id = ?1 OR todo_id = ?1) AND repo IS NOT NULL AND pr IS NOT NULL
                      ORDER BY repo, pr",
                )
                .map_err(|err| err.to_string())?;
            let rows = stmt
                .query_map(rusqlite::params![key], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(|err| err.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|err| err.to_string())
        })
        .unwrap_or_default()
}

/// A stable `event_id`, so the same defect reported twice (a re-sent alert, a
/// second edit of the same rework link) links the change once.
fn defect_id(cause: &str, by: &str, repo: &str, pr: i64) -> String {
    let digest = Sha256::digest(format!("{cause}\n{by}\n{repo}\n{pr}").as_bytes());
    format!("defect-{cause}-{}", &hex::encode(digest)[..24])
}

/// `defect.linked` for every change `keys` name: what review let through.
/// `cause` is `rework` or `incident` and `by` is what found it (the redoing
/// todo, the incident), which also names the event so a repeat links once.
pub(crate) fn link_defect(state: &WebState, cause: &str, by: &str, keys: &[String]) {
    let changes: BTreeSet<(String, i64)> =
        keys.iter().flat_map(|key| changes_of(state, key)).collect();
    for (repo, pr) in changes {
        let summary = match cause {
            "rework" => format!("{repo}#{pr} was redone by todo {by}"),
            _ => format!("{repo}#{pr} caused incident {by}"),
        };
        emit(
            state,
            NewEvent {
                event_id: Some(defect_id(cause, by, &repo, pr)),
                actor: Some("forge".to_string()),
                repo: Some(repo),
                pr: Some(pr),
                outcome: Some(cause.to_string()),
                detail: Some(json!({"cause": cause, "by": by})),
                ..NewEvent::forge(DEFECT_KIND, summary)
            },
        );
    }
}

/// `incident.caused_by` blames changes for an incident: each is a defect that
/// review did not catch. Called for every stored incident event.
pub(crate) fn link_incident(state: &WebState, event: &Event) {
    if event.kind != super::incidents::CAUSED_BY {
        return;
    }
    let Some(detail) = event.detail.as_ref() else {
        return;
    };
    let Some(incident) = detail.get("incident_id").and_then(|id| id.as_str()) else {
        return;
    };
    let named = detail
        .get("change")
        .into_iter()
        .chain(detail.get("changes").into_iter().flat_map(|changes| {
            changes
                .as_array()
                .map(|items| items.iter().collect::<Vec<_>>())
                .unwrap_or_default()
        }))
        .filter_map(|key| key.as_str())
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    link_defect(state, "incident", incident, &named);
}

/// One reviewer kind's row.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct ReviewerCatch {
    pub findings: u64,
    pub caught: u64,
    pub not_acted: u64,
    pub open: u64,
    /// Merged changes this reviewer kind passed that a defect was linked to.
    pub escaped: u64,
    /// `caught / (caught + escaped)`; absent when both are zero.
    pub catch_rate: Option<f64>,
    pub reviews_timed: u64,
    pub review_seconds_mean: Option<f64>,
    pub review_seconds_median: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct ReviewCatch {
    pub human: ReviewerCatch,
    pub agent: ReviewerCatch,
    pub total: ReviewerCatch,
}

fn ts(event: &Event) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&event.ts)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Record account kind at review time; unreadable classification stays unknown.
pub(crate) fn reviewer_kind(state: &WebState, login: &str) -> Option<&'static str> {
    super::people::kind_of(state, login)
        .ok()
        .map(|kind| match kind.kind {
            super::people::AccountKind::Human => HUMAN,
            super::people::AccountKind::Service => AGENT,
        })
}

/// The reviewer kind of a review event, if it is one.
fn review_kind(event: &Event) -> Option<&'static str> {
    match event.kind.as_str() {
        "pr.reviewed" | "review.finished" => Some(AGENT),
        "pr.review" | "pr.approved" if event.source == "forge" => {
            match detail_str(event, "reviewer_kind") {
                Some(HUMAN) => Some(HUMAN),
                Some(AGENT) => Some(AGENT),
                _ => None,
            }
        }
        _ => None,
    }
}

fn passed(event: &Event) -> bool {
    matches!(
        (event.kind.as_str(), event.outcome.as_deref()),
        (
            "pr.review" | "pr.reviewed" | "pr.approved",
            Some("approve" | "approved")
        )
    )
}

fn finish(row: &mut ReviewerCatch, mut seconds: Vec<f64>) {
    let denominator = row.caught + row.escaped;
    row.catch_rate = (denominator > 0).then(|| row.caught as f64 / denominator as f64);
    row.reviews_timed = seconds.len() as u64;
    if !seconds.is_empty() {
        seconds.sort_by(f64::total_cmp);
        row.review_seconds_mean = Some(seconds.iter().sum::<f64>() / seconds.len() as f64);
        let mid = seconds.len() / 2;
        row.review_seconds_median = Some(if seconds.len() % 2 == 1 {
            seconds[mid]
        } else {
            (seconds[mid - 1] + seconds[mid]) / 2.0
        });
    }
}

/// The catch report over `events` (any mix of kinds; others are ignored).
/// Findings and review times are counted for events inside `[since, until)`
/// when given; an escape counts when its defect link is in the window.
pub(crate) fn report(
    events: &[Event],
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> ReviewCatch {
    let inside = |event: &Event| match (window, ts(event)) {
        (Some((since, until)), Some(t)) => t >= since && t < until,
        _ => true,
    };
    // Index 0 is human, 1 is agent; both fold into `total` at the end.
    let row = |kind: &str| usize::from(kind == AGENT);
    let mut rows = [ReviewerCatch::default(), ReviewerCatch::default()];

    let mut resolution: BTreeMap<&str, &str> = BTreeMap::new();
    for event in events.iter().filter(|e| e.kind == RESOLVED_KIND) {
        if let (Some(key), Some(outcome)) = (detail_str(event, "finding"), event.outcome.as_deref())
            && matches!(outcome, "caught" | "not_acted")
        {
            resolution.entry(key).or_insert(outcome);
        }
    }
    for event in events.iter().filter(|e| inside(e)) {
        let Some(finding) = finding_of(event) else {
            continue;
        };
        let r = &mut rows[row(&finding.reviewer_kind)];
        r.findings += 1;
        match resolution.get(finding.key.as_str()) {
            Some(&"caught") => r.caught += 1,
            Some(_) => r.not_acted += 1,
            None => r.open += 1,
        }
    }

    // Escapes: a defect on a change a reviewer kind passed.
    let mut passes: BTreeMap<(String, i64), BTreeSet<&'static str>> = BTreeMap::new();
    for event in events.iter().filter(|e| passed(e)) {
        if let (Some(repo), Some(pr), Some(kind)) = (&event.repo, event.pr, review_kind(event)) {
            passes.entry((repo.clone(), pr)).or_default().insert(kind);
        }
    }
    let mut escaped_prs = BTreeSet::new();
    for event in events.iter().filter(|e| e.kind == DEFECT_KIND && inside(e)) {
        let (Some(repo), Some(pr)) = (&event.repo, event.pr) else {
            continue;
        };
        if !escaped_prs.insert((repo.clone(), pr)) {
            continue;
        }
        for kind in passes.get(&(repo.clone(), pr)).into_iter().flatten() {
            rows[row(kind)].escaped += 1;
        }
    }

    // Review time: from the review being asked for to the first review event
    // of each kind that follows.
    let mut started: BTreeMap<(String, i64), DateTime<Utc>> = BTreeMap::new();
    for event in events
        .iter()
        .filter(|e| REVIEW_STARTS.contains(&e.kind.as_str()))
    {
        if let (Some(repo), Some(pr), Some(t)) = (&event.repo, event.pr, ts(event)) {
            let slot = started.entry((repo.clone(), pr)).or_insert(t);
            *slot = (*slot).min(t);
        }
    }
    let mut firsts: BTreeMap<(String, i64, &'static str), DateTime<Utc>> = BTreeMap::new();
    for event in events.iter().filter(|e| inside(e)) {
        let (Some(repo), Some(pr), Some(kind), Some(t)) =
            (&event.repo, event.pr, review_kind(event), ts(event))
        else {
            continue;
        };
        let Some(start) = started.get(&(repo.clone(), pr)) else {
            continue;
        };
        if t < *start {
            continue;
        }
        let slot = firsts.entry((repo.clone(), pr, kind)).or_insert(t);
        *slot = (*slot).min(t);
    }
    let mut seconds: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
    for ((repo, pr, kind), t) in &firsts {
        let start = started[&(repo.clone(), *pr)];
        seconds[usize::from(*kind == AGENT)].push((*t - start).num_seconds() as f64);
    }

    let mut total = ReviewerCatch::default();
    let mut all_seconds = Vec::new();
    for (i, row) in rows.iter_mut().enumerate() {
        total.findings += row.findings;
        total.caught += row.caught;
        total.not_acted += row.not_acted;
        total.open += row.open;
        total.escaped += row.escaped;
        all_seconds.extend(seconds[i].iter().copied());
        finish(row, std::mem::take(&mut seconds[i]));
    }
    finish(&mut total, all_seconds);
    let [human, agent] = rows;
    ReviewCatch {
        human,
        agent,
        total,
    }
}

/// Every event of `kinds` the log keeps, for [`report`].
pub(crate) fn read_events(state: &WebState, family: Option<&str>) -> Result<Vec<Event>, String> {
    let mut out = Vec::new();
    let kinds = [
        FINDING_KIND,
        RESOLVED_KIND,
        DEFECT_KIND,
        "pr.review",
        "pr.approved",
        "pr.reviewed",
        "review.finished",
        "pr.ready_for_review",
        "pr.review_requested",
    ];
    for kind in kinds {
        let mut before: Option<i64> = None;
        loop {
            let query = EventsQuery {
                kind: Some(kind.to_string()),
                family: family.map(str::to_string),
                before_seq: before,
                limit: Some(500),
                ..Default::default()
            };
            let page = state.events.query(&query)?;
            let Some(last) = page.last() else { break };
            before = Some(last.seq);
            let full = page.len() >= 500;
            out.extend(page);
            if !full {
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
