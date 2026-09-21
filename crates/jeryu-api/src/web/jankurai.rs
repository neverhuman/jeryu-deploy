//! Quality-gate visibility: how the `jankurai/proof` gate has behaved, before
//! anyone makes it required.
//!
//! Every route reads the scores the forge already stores (the same records
//! behind `GET /api/v1/repos/:id/jankurai-scores`, written by the push-side
//! `record_authoritative_jankurai_score_with`). Findings are not a separate
//! table: each score keeps the auditor's whole report JSON, so rule id, path,
//! line and evidence are parsed back out of it here.
//!
//! Reads need a login and are filtered to the repositories the caller can read.
//! Filing a dispute is admin-only; disputes live in `<data_dir>/shift.sqlite`
//! (`db/migrations/0003_jankurai_disputes.sql`).

mod disputes;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{DateTime, Duration, Utc};
use jeryu_core::{AccountSummary, JankuraiScore, PullRequest, Repository, UserRole};
use serde::{Deserialize, Serialize};

use super::{WebState, api_error};
use crate::ci_bridge::HOST_JANKURAI_MINIMUM_SCORE;
pub(crate) use disputes::DisputeStore;
use disputes::{Dispute, NewDispute};

/// Windows the overview accepts. Wider windows would scan every stored report
/// on every request; these two answer "is this gate ready to be required?".
const ALLOWED_DAYS: [i64; 2] = [7, 30];
const DEFAULT_DAYS: i64 = 7;
/// Default window of the per-rule drill-down (the overview's widest window).
const RULE_DEFAULT_DAYS: i64 = 30;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct OverviewQuery {
    pub(crate) days: Option<i64>,
    pub(crate) repo: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RuleQuery {
    pub(crate) days: Option<i64>,
    pub(crate) repo: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DisputeListQuery {
    pub(crate) score_id: Option<String>,
    pub(crate) rule_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct Counts {
    scored: u32,
    passed: u32,
    failed: u32,
    pass_rate: f64,
}

impl Counts {
    fn new(passed: u32, failed: u32) -> Self {
        let scored = passed + failed;
        Self {
            scored,
            passed,
            failed,
            pass_rate: if scored == 0 {
                0.0
            } else {
                rounded(f64::from(passed) / f64::from(scored))
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct DayBucket {
    date: String,
    #[serde(flatten)]
    counts: Counts,
}

#[derive(Debug, Serialize)]
struct RepoBucket {
    repo: String,
    #[serde(flatten)]
    counts: Counts,
    latest_score: Option<u32>,
    latest_decision: Option<String>,
}

#[derive(Debug, Serialize)]
struct RuleBucket {
    rule: String,
    /// `cap` (a score cap the auditor applied), `hard-finding` (a hard rule the
    /// head broke) or `tool-failure` (the audit produced no score at all).
    kind: String,
    failures: u32,
    repos_affected: u32,
    disputes: u32,
    /// Disputes per failure: how often an operator called this rule wrong.
    dispute_rate: f64,
}

#[derive(Debug, Serialize)]
struct DistributionBucket {
    bucket: String,
    count: u32,
}

#[derive(Debug, Serialize)]
struct BlockedHead {
    score_id: String,
    repo: String,
    branch: String,
    commit_sha: String,
    score: Option<u32>,
    floor: u32,
    decision: String,
    pull_request: Option<PullRequestLink>,
}

#[derive(Debug, Serialize)]
struct OverviewResponse {
    generated_at: String,
    days: i64,
    repo: Option<String>,
    totals: Counts,
    daily: Vec<DayBucket>,
    repos: Vec<RepoBucket>,
    failures_by_rule: Vec<RuleBucket>,
    score_distribution: Vec<DistributionBucket>,
    would_have_blocked: Vec<BlockedHead>,
}

#[derive(Debug, Serialize)]
struct PullRequestLink {
    number: u64,
    title: String,
    state: String,
    merged: bool,
    merged_at: Option<String>,
    url: String,
}

#[derive(Debug, Serialize)]
struct FlaggedHead {
    score_id: String,
    repo: String,
    branch: String,
    commit_sha: String,
    score: Option<u32>,
    floor: u32,
    caps_applied: Vec<String>,
    hard_findings: u32,
    decision: String,
    passed: bool,
    /// How the rule flagged this head: `cap`, `hard-finding` or `finding`.
    matched_as: String,
    occurrences: u32,
    created_at: String,
    pull_request: Option<PullRequestLink>,
}

#[derive(Debug, Serialize)]
struct RuleResponse {
    rule_id: String,
    days: i64,
    heads: Vec<FlaggedHead>,
    disputes: Vec<Dispute>,
}

#[derive(Debug, Serialize)]
struct FindingDetail {
    rule_id: String,
    check_id: Option<String>,
    severity: Option<String>,
    hardness: Option<String>,
    category: Option<String>,
    path: Option<String>,
    line: Option<i64>,
    problem: Option<String>,
    evidence: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ScoreDetailResponse {
    score_id: String,
    repo: String,
    branch: String,
    commit_sha: String,
    created_at: String,
    decision: String,
    passed: bool,
    score: Option<u32>,
    raw_score: Option<u32>,
    floor: u32,
    caps_applied: Vec<String>,
    hard_findings: u32,
    tool_exit: Option<i64>,
    /// False when the score carries no auditor report, so the empty finding
    /// list means "nothing was stored", not "nothing was found".
    report_stored: bool,
    findings: Vec<FindingDetail>,
    pull_request: Option<PullRequestLink>,
    disputes: Vec<Dispute>,
}

#[derive(Debug, Deserialize)]
struct DisputeRequest {
    score_id: String,
    rule_id: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<i64>,
    reason: String,
}

/// One stored score with everything the routes derive from it, so a report is
/// parsed once per request instead of once per view.
struct ScoredHead {
    repo: String,
    score: JankuraiScore,
    report: Option<serde_json::Value>,
    floor: u32,
    passed: bool,
}

impl ScoredHead {
    fn new(repo: &Repository, score: JankuraiScore) -> Self {
        let report = score
            .report_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        let floor = report
            .as_ref()
            .and_then(|report| report.pointer("/decision/minimum_score"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0)
            .max(HOST_JANKURAI_MINIMUM_SCORE);
        // The same strict verdict the push-side proof publishes: score at or
        // above the effective floor, no hard finding, no cap applied. A score
        // the auditor never produced (`tool-failed`) fails closed.
        let passed = score.decision == "scored"
            && score.score.is_some_and(|value| value >= floor)
            && score.hard_findings == 0
            && score.caps_applied.is_empty();
        Self {
            repo: repo.full_name.clone(),
            score,
            report,
            floor,
            passed,
        }
    }

    fn findings(&self) -> Vec<FindingDetail> {
        self.report
            .as_ref()
            .and_then(|report| report.get("findings"))
            .and_then(serde_json::Value::as_array)
            .map(|findings| findings.iter().map(finding_detail).collect())
            .unwrap_or_default()
    }

    /// Rule ids the auditor recorded as hard findings on this head.
    fn hard_rules(&self) -> Vec<String> {
        self.findings()
            .into_iter()
            .filter(|finding| finding.hardness.as_deref() == Some("hard"))
            .map(|finding| finding.rule_id)
            .collect()
    }
}

fn finding_detail(finding: &serde_json::Value) -> FindingDetail {
    let string = |key: &str| {
        finding
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    FindingDetail {
        rule_id: string("rule_id").unwrap_or_else(|| "unknown".to_string()),
        check_id: string("check_id"),
        severity: string("severity"),
        hardness: string("hardness"),
        category: string("category"),
        path: string("path"),
        line: finding.get("line").and_then(serde_json::Value::as_i64),
        problem: string("problem"),
        evidence: finding
            .get("evidence")
            .and_then(serde_json::Value::as_array)
            .map(|evidence| {
                evidence
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// GET /api/v1/jankurai/overview?days=7|30&repo= — how the gate has behaved.
pub(crate) async fn overview(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<OverviewQuery>,
) -> AxumResponse {
    let days = match window_days(query.days, DEFAULT_DAYS) {
        Ok(days) => days,
        Err(response) => return *response,
    };
    let now = Utc::now();
    let since = now - Duration::days(days);
    let heads = collect_heads(&state, &account, query.repo.as_deref(), Some(since));
    let disputes = state.disputes.list(None, None).unwrap_or_default();

    let mut daily: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    for offset in 0..days {
        daily.insert(
            (now - Duration::days(offset))
                .format("%Y-%m-%d")
                .to_string(),
            (0, 0),
        );
    }
    let mut per_repo: BTreeMap<String, (u32, u32, Option<&ScoredHead>)> = BTreeMap::new();
    let mut distribution: BTreeMap<u32, u32> = (0..=10).map(|bucket| (bucket, 0)).collect();
    let mut rules: BTreeMap<(String, String), (u32, BTreeSet<String>)> = BTreeMap::new();
    let mut passed = 0;
    let mut failed = 0;

    for head in &heads {
        let day = daily
            .entry(head.score.created_at.format("%Y-%m-%d").to_string())
            .or_insert((0, 0));
        let repo = per_repo
            .entry(head.repo.clone())
            .or_insert((0, 0, None::<&ScoredHead>));
        if head.passed {
            passed += 1;
            day.0 += 1;
            repo.0 += 1;
        } else {
            failed += 1;
            day.1 += 1;
            repo.1 += 1;
        }
        // Scores come back newest first per repo, so the first one seen wins.
        if repo.2.is_none() {
            repo.2 = Some(head);
        }
        if let Some(score) = head.score.score {
            *distribution.entry(score / 10).or_insert(0) += 1;
        }
        if head.passed {
            continue;
        }
        for (rule, kind) in failure_rules(head) {
            let entry = rules.entry((rule, kind)).or_insert((0, BTreeSet::new()));
            entry.0 += 1;
            entry.1.insert(head.repo.clone());
        }
    }

    let mut dispute_counts: BTreeMap<String, u32> = BTreeMap::new();
    for dispute in &disputes {
        *dispute_counts.entry(dispute.rule_id.clone()).or_insert(0) += 1;
    }

    let response = OverviewResponse {
        generated_at: now.to_rfc3339(),
        days,
        repo: query.repo.clone(),
        totals: Counts::new(passed, failed),
        daily: daily
            .into_iter()
            .map(|(date, (passed, failed))| DayBucket {
                date,
                counts: Counts::new(passed, failed),
            })
            .collect(),
        repos: per_repo
            .into_iter()
            .map(|(repo, (passed, failed, latest))| RepoBucket {
                repo,
                counts: Counts::new(passed, failed),
                latest_score: latest.and_then(|head| head.score.score),
                latest_decision: latest.map(|head| head.score.decision.clone()),
            })
            .collect(),
        failures_by_rule: rules
            .into_iter()
            .map(|((rule, kind), (failures, repos))| {
                let disputes = dispute_counts.get(&rule).copied().unwrap_or(0);
                RuleBucket {
                    rule,
                    kind,
                    failures,
                    repos_affected: u32::try_from(repos.len()).unwrap_or(u32::MAX),
                    disputes,
                    dispute_rate: if failures == 0 {
                        0.0
                    } else {
                        rounded(f64::from(disputes) / f64::from(failures))
                    },
                }
            })
            .collect(),
        score_distribution: distribution
            .into_iter()
            .map(|(bucket, count)| DistributionBucket {
                bucket: distribution_label(bucket),
                count,
            })
            .collect(),
        would_have_blocked: would_have_blocked(&state, &heads),
    };
    Json(response).into_response()
}

/// GET /api/v1/jankurai/rules/:rule_id — the heads one rule or cap flagged.
pub(crate) async fn rule_detail(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(rule_id): AxumPath<String>,
    Query(query): Query<RuleQuery>,
) -> AxumResponse {
    let days = match window_days(query.days, RULE_DEFAULT_DAYS) {
        Ok(days) => days,
        Err(response) => return *response,
    };
    let since = Utc::now() - Duration::days(days);
    let heads = collect_heads(&state, &account, query.repo.as_deref(), Some(since));
    let flagged = heads
        .iter()
        .filter_map(|head| {
            matched_as(head, &rule_id).map(|(matched_as, occurrences)| FlaggedHead {
                score_id: head.score.id.to_string(),
                repo: head.repo.clone(),
                branch: head.score.branch.clone(),
                commit_sha: head.score.commit_sha.clone(),
                score: head.score.score,
                floor: head.floor,
                caps_applied: head.score.caps_applied.clone(),
                hard_findings: head.score.hard_findings,
                decision: head.score.decision.clone(),
                passed: head.passed,
                matched_as,
                occurrences,
                created_at: head.score.created_at.to_rfc3339(),
                pull_request: pull_request_link(&state, &head.repo, &head.score.commit_sha),
            })
        })
        .collect::<Vec<_>>();
    let disputes = state
        .disputes
        .list(None, Some(&rule_id))
        .unwrap_or_default();
    // An empty window is only an answer for a rule jankurai has actually
    // reported: an id no visible score ever carried is not a resource.
    if flagged.is_empty() && disputes.is_empty() && !rule_known(&state, &account, &rule_id) {
        return rule_not_found();
    }
    Json(RuleResponse {
        rule_id: rule_id.clone(),
        days,
        heads: flagged,
        disputes,
    })
    .into_response()
}

/// GET /api/v1/jankurai/scores/:score_id — one score, every finding.
pub(crate) async fn score_detail(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(score_id): AxumPath<String>,
) -> AxumResponse {
    let Some(head) = find_head(&state, &account, &score_id) else {
        return not_found();
    };
    let report = head.report.as_ref();
    Json(ScoreDetailResponse {
        score_id: head.score.id.to_string(),
        repo: head.repo.clone(),
        branch: head.score.branch.clone(),
        commit_sha: head.score.commit_sha.clone(),
        created_at: head.score.created_at.to_rfc3339(),
        decision: head.score.decision.clone(),
        passed: head.passed,
        score: head.score.score,
        raw_score: report
            .and_then(|report| report.get("raw_score"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        floor: head.floor,
        caps_applied: head.score.caps_applied.clone(),
        hard_findings: head.score.hard_findings,
        tool_exit: report
            .and_then(|report| report.get("tool_exit"))
            .and_then(serde_json::Value::as_i64),
        report_stored: report.is_some_and(|report| report.get("findings").is_some()),
        findings: head.findings(),
        pull_request: pull_request_link(&state, &head.repo, &head.score.commit_sha),
        disputes: state
            .disputes
            .list(Some(&head.score.id.to_string()), None)
            .unwrap_or_default(),
    })
    .into_response()
}

/// GET /api/v1/jankurai/disputes[?score_id=&rule_id=] — filed disputes.
pub(crate) async fn dispute_list(
    State(state): State<Arc<WebState>>,
    Query(query): Query<DisputeListQuery>,
) -> AxumResponse {
    match state
        .disputes
        .list(query.score_id.as_deref(), query.rule_id.as_deref())
    {
        Ok(disputes) => Json(serde_json::json!({ "disputes": disputes })).into_response(),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("disputes could not be read: {error}"),
        ),
    }
}

/// POST /api/v1/jankurai/disputes — record that a finding looks wrong.
///
/// Admin-only: a dispute rate is the evidence the owner weighs before making
/// the gate required, so any logged-in user must not be able to move it.
pub(crate) async fn dispute_create(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    body: Bytes,
) -> AxumResponse {
    if account.role != UserRole::Admin {
        return api_error(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "filing a jankurai dispute requires global-admin access",
        );
    }
    let request: DisputeRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => return dispute_invalid(&format!("body failed to parse: {error}")),
    };
    if request.rule_id.trim().is_empty() || request.reason.trim().is_empty() {
        return dispute_invalid("rule_id and reason must not be empty");
    }
    let Some(head) = find_head(&state, &account, request.score_id.trim()) else {
        return not_found();
    };
    let dispute = NewDispute {
        score_id: head.score.id.to_string(),
        repo: head.repo.clone(),
        commit_sha: head.score.commit_sha.clone(),
        rule_id: request.rule_id.trim().to_string(),
        path: request
            .path
            .map(|path| path.trim().to_string())
            .filter(|path| !path.is_empty()),
        line: request.line,
        reason: request.reason.trim().to_string(),
        author: account.login.clone(),
    };
    match state
        .disputes
        .insert(&dispute, Utc::now().timestamp_millis())
    {
        Ok(filed) => {
            let status = if filed.duplicate {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            (status, Json(filed.dispute)).into_response()
        }
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("dispute could not be persisted: {error}"),
        ),
    }
}

fn dispute_invalid(reason: &str) -> AxumResponse {
    api_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_input",
        &format!("jankurai dispute failed validation: {reason}"),
    )
}

/// Whether any score the account can read, at any time, in any repo,
/// flagged `rule_id` as a cap or a finding.
fn rule_known(state: &WebState, account: &AccountSummary, rule_id: &str) -> bool {
    collect_heads(state, account, None, None)
        .iter()
        .any(|head| matched_as(head, rule_id).is_some())
}

fn rule_not_found() -> AxumResponse {
    api_error(
        StatusCode::NOT_FOUND,
        "not_found",
        "jankurai rule not found",
    )
}

fn not_found() -> AxumResponse {
    api_error(
        StatusCode::NOT_FOUND,
        "not_found",
        "jankurai score not found",
    )
}

/// The response is boxed because the error path is a whole HTTP response and
/// clippy refuses a `Result` whose `Err` dwarfs its `Ok` (as `repositories`
/// does for its own source reads).
fn window_days(requested: Option<i64>, default: i64) -> Result<i64, Box<AxumResponse>> {
    match requested {
        None => Ok(default),
        Some(days) if ALLOWED_DAYS.contains(&days) => Ok(days),
        Some(days) => Err(Box::new(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            &format!("days must be 7 or 30, got {days}"),
        ))),
    }
}

/// Every stored score the caller may read, newest first per repository.
fn collect_heads(
    state: &WebState,
    account: &AccountSummary,
    repo_filter: Option<&str>,
    since: Option<DateTime<Utc>>,
) -> Vec<ScoredHead> {
    let core = state.github.core();
    core.list_repositories(None)
        .into_iter()
        .filter(|repo| {
            account.role == UserRole::Admin
                || core.user_can_read_repo(&account.login, &repo.owner, &repo.name)
        })
        .filter(|repo| {
            repo_filter.is_none_or(|filter| {
                repo.full_name == filter || repo.name == filter || repo.id.to_string() == filter
            })
        })
        .flat_map(|repo| {
            core.list_jankurai_scores(&repo.owner, &repo.name, None, None)
                .unwrap_or_default()
                .into_iter()
                .filter(|score| since.is_none_or(|since| score.created_at >= since))
                .map(|score| ScoredHead::new(&repo, score))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn find_head(state: &WebState, account: &AccountSummary, score_id: &str) -> Option<ScoredHead> {
    collect_heads(state, account, None, None)
        .into_iter()
        .find(|head| head.score.id.to_string() == score_id)
}

/// Why this failing head failed: each applied cap, each hard rule it broke, or
/// the audit itself having produced no score.
fn failure_rules(head: &ScoredHead) -> Vec<(String, String)> {
    if head.score.decision != "scored" {
        return vec![(head.score.decision.clone(), "tool-failure".to_string())];
    }
    let mut rules: Vec<(String, String)> = head
        .score
        .caps_applied
        .iter()
        .map(|cap| (cap.clone(), "cap".to_string()))
        .collect();
    for rule in head.hard_rules() {
        rules.push((rule, "hard-finding".to_string()));
    }
    if rules.is_empty() {
        // Below the floor with no cap and no nameable hard rule.
        rules.push(("below-floor".to_string(), "score".to_string()));
    }
    rules
}

/// How `rule_id` flagged this head, and how many findings it accounts for.
fn matched_as(head: &ScoredHead, rule_id: &str) -> Option<(String, u32)> {
    if head.score.caps_applied.iter().any(|cap| cap == rule_id) {
        return Some(("cap".to_string(), 1));
    }
    let findings = head.findings();
    let occurrences = findings
        .iter()
        .filter(|finding| {
            finding.rule_id == rule_id
                || finding.check_id.as_deref().is_some_and(|id| id == rule_id)
        })
        .count();
    if occurrences == 0 {
        return None;
    }
    let hard = findings.iter().any(|finding| {
        finding.hardness.as_deref() == Some("hard")
            && (finding.rule_id == rule_id
                || finding.check_id.as_deref().is_some_and(|id| id == rule_id))
    });
    let matched = if hard { "hard-finding" } else { "finding" };
    Some((
        matched.to_string(),
        u32::try_from(occurrences).unwrap_or(u32::MAX),
    ))
}

/// Failing heads that merged anyway: what a required gate would have stopped.
fn would_have_blocked(state: &WebState, heads: &[ScoredHead]) -> Vec<BlockedHead> {
    heads
        .iter()
        .filter(|head| !head.passed)
        .filter_map(|head| {
            let pull_request = merged_pull_request(state, &head.repo, &head.score.commit_sha)?;
            Some(BlockedHead {
                score_id: head.score.id.to_string(),
                repo: head.repo.clone(),
                branch: head.score.branch.clone(),
                commit_sha: head.score.commit_sha.clone(),
                score: head.score.score,
                floor: head.floor,
                decision: head.score.decision.clone(),
                pull_request: Some(pull_request),
            })
        })
        .collect()
}

fn merged_pull_request(state: &WebState, repo: &str, sha: &str) -> Option<PullRequestLink> {
    pull_requests_for_head(state, repo, sha)
        .into_iter()
        .find(|pull| pull.merged)
        .map(|pull| pull_request_link_of(repo, &pull))
}

fn pull_request_link(state: &WebState, repo: &str, sha: &str) -> Option<PullRequestLink> {
    let pulls = pull_requests_for_head(state, repo, sha);
    pulls
        .iter()
        .find(|pull| pull.merged)
        .or_else(|| pulls.first())
        .map(|pull| pull_request_link_of(repo, pull))
}

/// Pull requests whose head commit is this scored head, newest first.
fn pull_requests_for_head(state: &WebState, repo: &str, sha: &str) -> Vec<PullRequest> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Vec::new();
    };
    let mut pulls: Vec<PullRequest> = state
        .github
        .core()
        .list_pull_requests(owner, name, None)
        .unwrap_or_default()
        .into_iter()
        .filter(|pull| pull.head.sha == sha)
        .collect();
    pulls.sort_by_key(|pull| std::cmp::Reverse(pull.number));
    pulls
}

fn pull_request_link_of(repo: &str, pull: &PullRequest) -> PullRequestLink {
    PullRequestLink {
        number: pull.number,
        title: pull.title.clone(),
        state: format!("{:?}", pull.state).to_lowercase(),
        merged: pull.merged,
        merged_at: pull.merged_at.map(|at| at.to_rfc3339()),
        url: format!("/api/v1/repos/{repo}/pulls/{}", pull.number),
    }
}

fn distribution_label(bucket: u32) -> String {
    if bucket >= 10 {
        "100".to_string()
    } else {
        format!("{}-{}", bucket * 10, bucket * 10 + 9)
    }
}

/// Rates are read by a human comparing repositories; two decimals is enough
/// and keeps the JSON stable across platforms.
fn rounded(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}
