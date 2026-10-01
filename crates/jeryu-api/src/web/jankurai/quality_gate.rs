//! The web console's quality-gate contract (`/api/v1/quality-gate/*`).
//!
//! Same stored scores and disputes as the `/api/v1/jankurai/*` routes, shaped
//! the way the Quality gate pages read them: a head is addressed by
//! `owner/name/sha`, and a finding by `<score_id>:<index>` into the stored
//! report's `findings` array, so a dispute can be filed from the finding alone.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::{Duration, Utc};
use jeryu_core::{AccountSummary, UserRole};
use serde::{Deserialize, Serialize};

use super::dimension_floor::{DimensionResult, dimension_result, median};
use super::disputes::{Dispute, NewDispute};
use super::{
    DEFAULT_DAYS, FindingDetail, RULE_DEFAULT_DAYS, RuleQuery, ScoredHead, api_error,
    collect_heads, dispute_invalid, failure_rules, matched_as, rounded, rule_known, rule_not_found,
    window_days,
};
use crate::web::WebState;

/// Bumped when a field changes meaning; the pages check it.
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WindowQuery {
    pub(crate) days: Option<i64>,
}

#[derive(Debug, Serialize)]
struct Day {
    day: String,
    passed: u32,
    failed: u32,
}

/// How one rule behaved over the window. Dimension-floor results are not
/// rule findings and are counted in [`DimensionSummary`] instead.
#[derive(Debug, Serialize)]
struct RuleSummary {
    rule: String,
    title: String,
    /// Findings summed over every scored head in the window: one problem left
    /// standing for ten pushes counts ten times. Kept for older readers; the
    /// same number as `findings_all_heads`.
    failures: u32,
    /// Repositories with any scored head in the window carrying the rule.
    repos: u32,
    /// Same as `failures`, named for what it counts.
    findings_all_heads: u32,
    /// Distinct findings on each repository's latest scored head in the
    /// window, summed over repositories: what is open now.
    latest_findings: u32,
    /// Repositories whose latest scored head carries the rule.
    latest_repos: u32,
    disputes: u32,
    dispute_rate: f64,
}

/// A scoring dimension below the floor on repositories' latest heads. These
/// are scores, not detections: the auditor files them under a rule id, but
/// no rule raised them.
#[derive(Debug, Serialize)]
struct DimensionSummary {
    dimension: String,
    /// Repositories whose latest scored head has the dimension below the floor.
    repos: u32,
    /// The median of those repositories' scores for the dimension.
    median_score: f64,
    /// The floor the auditor compared against (the highest seen).
    floor: u32,
    /// The rule id the auditor filed the results under, when it named one.
    attributed_rule: Option<String>,
}

#[derive(Debug, Serialize)]
struct RepoSummary {
    repo: String,
    heads_scored: u32,
    heads_failed: u32,
    fail_rate: f64,
    top_rule: Option<String>,
}

#[derive(Debug, Serialize)]
struct Overview {
    schema_version: u32,
    generated_at: String,
    window_days: i64,
    heads_scored: u32,
    heads_failed: u32,
    fail_rate: f64,
    disputes: u32,
    /// Repositories with a scored head in the window; the base of every
    /// `latest_*` count and of `dimensions_below_floor`.
    repos_scored: u32,
    rules: Vec<RuleSummary>,
    /// Dimension-floor results on the latest heads, kept out of `rules`.
    dimensions_below_floor: Vec<DimensionSummary>,
    repos: Vec<RepoSummary>,
    daily: Vec<Day>,
}

#[derive(Debug, Serialize)]
struct FlaggedHead {
    repo: String,
    sha: String,
    branch: String,
    scored_at: String,
    score: u32,
    threshold: u32,
    findings: u32,
    disputes: u32,
}

#[derive(Debug, Serialize)]
struct RuleDetail {
    schema_version: u32,
    rule: String,
    title: String,
    description: String,
    window_days: i64,
    heads: Vec<FlaggedHead>,
}

#[derive(Debug, Serialize)]
struct Finding {
    id: String,
    rule: String,
    title: String,
    path: String,
    line: i64,
    evidence: String,
    disputed: bool,
    dispute_reason: Option<String>,
    disputed_by: Option<String>,
    disputed_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct HeadDetail {
    schema_version: u32,
    repo: String,
    sha: String,
    branch: String,
    scored_at: String,
    score: u32,
    threshold: u32,
    passed: bool,
    /// Every cap the auditor applied to this head, explained.
    caps: Vec<AppliedCap>,
    findings: Vec<Finding>,
}

/// One applied cap: what it means and what clears it, so a red
/// `jankurai/proof` reads without the raw report.
#[derive(Debug, Serialize)]
struct AppliedCap {
    id: String,
    meaning: String,
    how_to_clear: String,
    findings: u32,
}

#[derive(Debug, Deserialize)]
struct DisputeRequest {
    reason: String,
}

fn rate(part: u32, whole: u32) -> f64 {
    if whole == 0 {
        0.0
    } else {
        rounded(f64::from(part) / f64::from(whole))
    }
}

fn count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// What one head carries, split: rule findings (each applied cap once, each
/// finding once, keyed so a repeated identical finding can be told apart from
/// a distinct one) and dimension-floor results.
struct HeadCarries {
    rules: Vec<(String, String)>,
    dimensions: Vec<(DimensionResult, String)>,
}

fn head_carries(head: &ScoredHead) -> HeadCarries {
    let mut rules: Vec<(String, String)> = head
        .score
        .caps_applied
        .iter()
        .map(|cap| (cap.clone(), format!("cap:{cap}")))
        .collect();
    let mut dimensions = Vec::new();
    for finding in head.findings() {
        if let Some(result) = dimension_result(head, &finding) {
            dimensions.push((result, finding.rule_id));
            continue;
        }
        let key = format!(
            "finding:{}|{}|{}|{}",
            finding.check_id.as_deref().unwrap_or_default(),
            finding.path.as_deref().unwrap_or_default(),
            finding.line.unwrap_or(0),
            finding.problem.as_deref().unwrap_or_default(),
        );
        rules.push((finding.rule_id, key));
    }
    HeadCarries { rules, dimensions }
}

/// Each repository's newest scored head among `heads`.
fn latest_heads(heads: &[ScoredHead]) -> Vec<&ScoredHead> {
    let mut latest: BTreeMap<&str, &ScoredHead> = BTreeMap::new();
    for head in heads {
        let slot = latest.entry(head.repo.as_str()).or_insert(head);
        if head.score.created_at > slot.score.created_at {
            *slot = head;
        }
    }
    latest.into_values().collect()
}

#[derive(Default)]
struct RuleTally {
    all_heads: u32,
    repos: BTreeSet<String>,
    latest_findings: u32,
    latest_repos: u32,
}

#[derive(Default)]
struct DimensionTally {
    /// Per repository, the lowest score seen on its latest head.
    scores: BTreeMap<String, u32>,
    floor: u32,
    rule: Option<String>,
}

/// Rule counts on the latest heads, and the dimensions below the floor there.
fn tally_latest(
    heads: &[ScoredHead],
    rules: &mut BTreeMap<String, RuleTally>,
) -> Vec<DimensionSummary> {
    let mut dimensions: BTreeMap<String, DimensionTally> = BTreeMap::new();
    for head in latest_heads(heads) {
        let carries = head_carries(head);
        let mut distinct: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (rule, key) in carries.rules {
            distinct.entry(rule).or_default().insert(key);
        }
        for (rule, keys) in distinct {
            let tally = rules.entry(rule).or_default();
            tally.latest_findings += count(keys.len());
            tally.latest_repos += 1;
        }
        for (result, rule) in carries.dimensions {
            let tally = dimensions.entry(result.dimension).or_default();
            let score = tally
                .scores
                .entry(head.repo.clone())
                .or_insert(result.score);
            *score = (*score).min(result.score);
            tally.floor = tally.floor.max(result.floor);
            if tally.rule.is_none() && rule != "unknown" {
                tally.rule = Some(rule);
            }
        }
    }
    let mut summaries: Vec<DimensionSummary> = dimensions
        .into_iter()
        .map(|(dimension, tally)| {
            let mut scores: Vec<u32> = tally.scores.into_values().collect();
            DimensionSummary {
                dimension,
                repos: count(scores.len()),
                median_score: median(&mut scores),
                floor: tally.floor,
                attributed_rule: tally.rule,
            }
        })
        .collect();
    // Most repositories first, then the lowest median, then by name.
    summaries.sort_by(|a, b| {
        b.repos
            .cmp(&a.repos)
            .then(a.median_score.total_cmp(&b.median_score))
            .then_with(|| a.dimension.cmp(&b.dimension))
    });
    summaries
}

/// GET /api/v1/quality-gate/overview?days=7|30
pub(crate) async fn overview(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<WindowQuery>,
) -> AxumResponse {
    let days = match window_days(query.days, DEFAULT_DAYS) {
        Ok(days) => days,
        Err(response) => return *response,
    };
    let now = Utc::now();
    let heads = collect_heads(&state, &account, None, Some(now - Duration::days(days)));
    let score_ids: BTreeSet<String> = heads.iter().map(|head| head.score.id.to_string()).collect();
    let disputes: Vec<Dispute> = state
        .disputes
        .list(None, None)
        .unwrap_or_default()
        .into_iter()
        .filter(|dispute| score_ids.contains(&dispute.score_id))
        .collect();

    let mut daily: BTreeMap<String, (u32, u32)> = (0..days)
        .map(|offset| {
            (
                (now - Duration::days(offset))
                    .format("%Y-%m-%d")
                    .to_string(),
                (0, 0),
            )
        })
        .collect();
    let mut repos: BTreeMap<String, (u32, u32, BTreeMap<String, u32>)> = BTreeMap::new();
    let mut rules: BTreeMap<String, RuleTally> = BTreeMap::new();
    let mut failed = 0;
    for head in &heads {
        let day = daily
            .entry(head.score.created_at.format("%Y-%m-%d").to_string())
            .or_insert((0, 0));
        let repo = repos.entry(head.repo.clone()).or_default();
        repo.0 += 1;
        if head.passed {
            day.0 += 1;
        } else {
            day.1 += 1;
            repo.1 += 1;
            failed += 1;
            for (rule, _) in failure_rules(head) {
                *repo.2.entry(rule).or_insert(0) += 1;
            }
        }
        for (rule, _) in head_carries(head).rules {
            let entry = rules.entry(rule).or_default();
            entry.all_heads += 1;
            entry.repos.insert(head.repo.clone());
        }
    }
    let dimensions_below_floor = tally_latest(&heads, &mut rules);
    let repos_scored = count(repos.len());
    let mut disputes_by_rule: BTreeMap<&str, u32> = BTreeMap::new();
    for dispute in &disputes {
        *disputes_by_rule
            .entry(dispute.rule_id.as_str())
            .or_insert(0) += 1;
        rules.entry(dispute.rule_id.clone()).or_default();
    }

    let scored = count(heads.len());
    Json(Overview {
        schema_version: SCHEMA_VERSION,
        generated_at: now.to_rfc3339(),
        window_days: days,
        heads_scored: scored,
        heads_failed: failed,
        fail_rate: rate(failed, scored),
        disputes: count(disputes.len()),
        repos_scored,
        rules: rules
            .into_iter()
            .map(|(rule, tally)| {
                let disputes = disputes_by_rule.get(rule.as_str()).copied().unwrap_or(0);
                RuleSummary {
                    title: rule.clone(),
                    rule,
                    failures: tally.all_heads,
                    repos: count(tally.repos.len()),
                    findings_all_heads: tally.all_heads,
                    latest_findings: tally.latest_findings,
                    latest_repos: tally.latest_repos,
                    disputes,
                    dispute_rate: rate(disputes, tally.all_heads),
                }
            })
            .collect(),
        dimensions_below_floor,
        repos: repos
            .into_iter()
            .map(|(repo, (scored, failed, failing_rules))| RepoSummary {
                repo,
                heads_scored: scored,
                heads_failed: failed,
                fail_rate: rate(failed, scored),
                // Most failures wins; ties go to the first rule by name.
                top_rule: failing_rules
                    .into_iter()
                    .rev()
                    .max_by_key(|(_, failures)| *failures)
                    .map(|(rule, _)| rule),
            })
            .collect(),
        daily: daily
            .into_iter()
            .map(|(day, (passed, failed))| Day {
                day,
                passed,
                failed,
            })
            .collect(),
    })
    .into_response()
}

/// GET /api/v1/quality-gate/rules/:rule?days=7|30
pub(crate) async fn rule(
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
    let disputes = state
        .disputes
        .list(None, Some(&rule_id))
        .unwrap_or_default();
    let flagged: Vec<FlaggedHead> = heads
        .iter()
        .filter_map(|head| {
            let (_, occurrences) = matched_as(head, &rule_id)?;
            let score_id = head.score.id.to_string();
            Some(FlaggedHead {
                repo: head.repo.clone(),
                sha: head.score.commit_sha.clone(),
                branch: head.score.branch.clone(),
                scored_at: head.score.created_at.to_rfc3339(),
                score: head.score.score.unwrap_or(0),
                threshold: head.floor,
                findings: occurrences,
                disputes: count(
                    disputes
                        .iter()
                        .filter(|dispute| dispute.score_id == score_id)
                        .count(),
                ),
            })
        })
        .collect();
    if flagged.is_empty() && disputes.is_empty() && !rule_known(&state, &account, &rule_id) {
        return rule_not_found();
    }
    let description = heads
        .iter()
        .flat_map(ScoredHead::findings)
        .find(|finding| finding.rule_id == rule_id)
        .and_then(|finding| finding.problem)
        .unwrap_or_default();
    Json(RuleDetail {
        schema_version: SCHEMA_VERSION,
        title: rule_id.clone(),
        rule: rule_id,
        description,
        window_days: days,
        heads: flagged,
    })
    .into_response()
}

/// GET /api/v1/quality-gate/heads/:owner/:name/:sha — the newest score of
/// that head, every finding on it.
pub(crate) async fn head(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath((owner, name, sha)): AxumPath<(String, String, String)>,
) -> AxumResponse {
    let repo = format!("{owner}/{name}");
    let Some(head) = collect_heads(&state, &account, Some(&repo), None)
        .into_iter()
        .filter(|head| head.repo == repo && head.score.commit_sha == sha)
        .max_by_key(|head| head.score.created_at)
    else {
        return head_not_found();
    };
    let score_id = head.score.id.to_string();
    let disputes = state
        .disputes
        .list(Some(&score_id), None)
        .unwrap_or_default();
    Json(HeadDetail {
        schema_version: SCHEMA_VERSION,
        repo: head.repo.clone(),
        sha: head.score.commit_sha.clone(),
        branch: head.score.branch.clone(),
        scored_at: head.score.created_at.to_rfc3339(),
        score: head.score.score.unwrap_or(0),
        threshold: head.floor,
        passed: head.passed,
        caps: applied_caps(&head),
        findings: head
            .findings()
            .into_iter()
            .enumerate()
            .map(|(index, finding)| finding_view(&score_id, index, finding, &disputes))
            .collect(),
    })
    .into_response()
}

fn applied_caps(head: &ScoredHead) -> Vec<AppliedCap> {
    let findings = head.findings();
    head.score
        .caps_applied
        .iter()
        .map(|cap| {
            let matching: Vec<_> = findings
                .iter()
                .filter(|finding| {
                    finding.rule_id == *cap || finding.check_id.as_deref() == Some(cap.as_str())
                })
                .collect();
            let (meaning, how_to_clear) = cap_explanation(cap, matching.len());
            let meaning = match matching
                .iter()
                .find_map(|finding| finding.problem.as_deref())
            {
                Some(problem) => format!("{meaning} On this head: {problem}"),
                None => meaning,
            };
            AppliedCap {
                id: cap.clone(),
                meaning,
                how_to_clear,
                findings: count(matching.len()),
            }
        })
        .collect()
}

/// What a cap means and how to clear it. Caps the gate sees often have their
/// own wording; any other cap gets the general rule, which is still exact:
/// the score is held under a ceiling until the next scored push no longer
/// applies the cap.
fn cap_explanation(cap: &str, findings: usize) -> (String, String) {
    let meaning = match cap {
        "dead-language" => "Product code or tests use words the auditor reads as marking \
                            dead or superseded code; the score is held under a ceiling \
                            while they remain; rename or remove them."
            .to_string(),
        "tool-failure" => "The auditor did not produce a valid report, so the proof fails \
                           closed."
            .to_string(),
        _ => format!(
            "The auditor applied the `{cap}` cap: the score is held under a ceiling while \
             what `{cap}` checks for remains, whatever the rest of the head scores."
        ),
    };
    let how_to_clear = if findings > 0 {
        format!(
            "Fix the {findings} `{cap}` finding(s) listed below (or dispute one that is \
             wrong) and push; the cap lifts when the next score no longer applies it."
        )
    } else {
        format!(
            "Remove what `{cap}` flags (run `jankurai diff-audit` locally to see it) and \
             push; the cap lifts when the next score no longer applies it."
        )
    };
    (meaning, how_to_clear)
}

/// POST /api/v1/quality-gate/findings/:id/dispute — admin-only, like the
/// `/api/v1/jankurai/disputes` filing it records into.
pub(crate) async fn dispute(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(finding_id): AxumPath<String>,
    body: Bytes,
) -> AxumResponse {
    if account.role != UserRole::Admin {
        return api_error(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "disputing a quality-gate finding requires global-admin access",
        );
    }
    let request: DisputeRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => return dispute_invalid(&format!("body failed to parse: {error}")),
    };
    if request.reason.trim().is_empty() {
        return dispute_invalid("reason must not be empty");
    }
    let Some((score_id, index)) = finding_id
        .rsplit_once(':')
        .and_then(|(score_id, index)| Some((score_id, index.parse::<usize>().ok()?)))
    else {
        return finding_not_found();
    };
    let Some(head) = collect_heads(&state, &account, None, None)
        .into_iter()
        .find(|head| head.score.id.to_string() == score_id)
    else {
        return finding_not_found();
    };
    let Some(finding) = head.findings().into_iter().nth(index) else {
        return finding_not_found();
    };
    let filed = state.disputes.insert(
        &NewDispute {
            score_id: score_id.to_string(),
            repo: head.repo.clone(),
            commit_sha: head.score.commit_sha.clone(),
            rule_id: finding.rule_id.clone(),
            path: finding.path.clone(),
            line: finding.line,
            reason: request.reason.trim().to_string(),
            author: account.login.clone(),
        },
        Utc::now().timestamp_millis(),
    );
    match filed {
        Ok(filed) => {
            let status = if filed.duplicate {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            let finding = finding_view(score_id, index, finding, &[filed.dispute]);
            (status, Json(serde_json::json!({ "finding": finding }))).into_response()
        }
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_failed",
            &format!("dispute could not be persisted: {error}"),
        ),
    }
}

/// A stored finding as the pages show it, with the newest dispute on it.
fn finding_view(
    score_id: &str,
    index: usize,
    finding: FindingDetail,
    disputes: &[Dispute],
) -> Finding {
    let dispute = disputes.iter().find(|dispute| {
        dispute.rule_id == finding.rule_id
            && dispute.path == finding.path
            && dispute.line == finding.line
    });
    Finding {
        id: format!("{score_id}:{index}"),
        title: finding
            .problem
            .clone()
            .unwrap_or_else(|| finding.rule_id.clone()),
        rule: finding.rule_id,
        path: finding.path.unwrap_or_default(),
        line: finding.line.unwrap_or(0),
        evidence: finding.evidence.join("\n"),
        disputed: dispute.is_some(),
        dispute_reason: dispute.map(|dispute| dispute.reason.clone()),
        disputed_by: dispute.map(|dispute| dispute.author.clone()),
        disputed_at: dispute.map(|dispute| dispute.created_at.clone()),
    }
}

fn head_not_found() -> AxumResponse {
    api_error(
        StatusCode::NOT_FOUND,
        "not_found",
        "no quality-gate score for that head",
    )
}

fn finding_not_found() -> AxumResponse {
    api_error(
        StatusCode::NOT_FOUND,
        "not_found",
        "quality-gate finding not found",
    )
}
