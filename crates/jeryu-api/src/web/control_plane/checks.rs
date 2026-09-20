use std::collections::{BTreeMap, BTreeSet};

use jeryu_core::{CheckConclusion, CheckRun, CheckRunStatus, check_conclusion_wire_value};

use super::*;

pub(crate) fn summarize_checks(checks: &[CheckRun]) -> CheckSummary {
    let queued = checks
        .iter()
        .filter(|check| check.status == CheckRunStatus::Queued)
        .count();
    let running = checks
        .iter()
        .filter(|check| check.status == CheckRunStatus::InProgress)
        .count();
    let failing = checks.iter().filter(|check| failing_check(check)).count();
    let successful = checks
        .iter()
        .filter(|check| {
            check.status == CheckRunStatus::Completed
                && check.conclusion == Some(CheckConclusion::Success)
        })
        .count();
    CheckSummary {
        total: checks.len(),
        queued,
        running,
        failing,
        successful,
        missing: checks.is_empty(),
    }
}

pub(crate) fn check_state(check: &CheckRun) -> EvidenceState {
    match check.status {
        CheckRunStatus::Queued => EvidenceState::Queued,
        CheckRunStatus::InProgress => EvidenceState::Fresh,
        CheckRunStatus::Completed if failing_check(check) => EvidenceState::Failed,
        CheckRunStatus::Completed => EvidenceState::Fresh,
    }
}

pub(crate) fn failing_check(check: &CheckRun) -> bool {
    matches!(
        check.conclusion,
        Some(
            CheckConclusion::ActionRequired
                | CheckConclusion::Cancelled
                | CheckConclusion::Failure
                | CheckConclusion::TimedOut
        )
    )
}

pub(crate) fn check_status(status: &CheckRunStatus) -> &'static str {
    match status {
        CheckRunStatus::Queued => "queued",
        CheckRunStatus::InProgress => "in_progress",
        CheckRunStatus::Completed => "completed",
    }
}

pub(crate) fn check_conclusion(conclusion: &CheckConclusion) -> &'static str {
    match conclusion {
        CheckConclusion::ActionRequired => "action_required",
        CheckConclusion::Cancelled => "cancelled",
        CheckConclusion::Failure => "failure",
        CheckConclusion::Neutral => "neutral",
        CheckConclusion::Success => "success",
        CheckConclusion::Skipped => "skipped",
        CheckConclusion::Superseded => check_conclusion_wire_value(conclusion),
        CheckConclusion::TimedOut => "timed_out",
    }
}

/// Repeated failure shapes behind a failing-check backlog, largest first.
///
/// A three-digit `failingChecks` number is only actionable once you know
/// whether it is one lane failing on every head or many independent breaks, so
/// the summary carries the grouping rather than a bare count. Grouping is by
/// check name and conclusion, because that pair is what a repair targets.
pub(crate) fn failing_check_causes(checks: &[ControlCheckRun]) -> Vec<FailingCheckCause> {
    let failing: Vec<&ControlCheckRun> = checks
        .iter()
        .filter(|check| check.state == EvidenceState::Failed)
        .collect();
    let total = failing.len();
    if total == 0 {
        return Vec::new();
    }
    let mut grouped: BTreeMap<(&str, &str), (usize, BTreeSet<&str>)> = BTreeMap::new();
    for check in &failing {
        let conclusion = check.conclusion.as_deref().unwrap_or("unknown");
        let entry = grouped
            .entry((check.name.as_str(), conclusion))
            .or_insert_with(|| (0, BTreeSet::new()));
        entry.0 += 1;
        entry.1.insert(check.repo.as_str());
    }
    let mut causes: Vec<FailingCheckCause> = grouped
        .into_iter()
        .map(|((name, conclusion), (count, repos))| FailingCheckCause {
            name: name.to_string(),
            conclusion: conclusion.to_string(),
            count,
            share_percent: share_percent(count, total),
            repo_count: repos.len(),
            repos: repos
                .into_iter()
                .take(FAILING_CAUSE_REPO_LIMIT)
                .map(str::to_string)
                .collect(),
        })
        .collect();
    // Largest cause first; this sort is stable, so ties keep the BTreeMap's
    // name/conclusion order and two snapshots of the same forge state
    // serialize identically.
    causes.sort_by_key(|cause| std::cmp::Reverse(cause.count));
    causes.truncate(FAILING_CAUSE_LIMIT);
    causes
}

/// One evidence line per cause, phrased so an agent reading priorities can act
/// on it without a second request.
pub(crate) fn failing_cause_evidence(cause: &FailingCheckCause, total: usize) -> String {
    format!(
        "{} of {} ({}%) are {} -> {} across {} repo(s): {}",
        cause.count,
        total,
        cause.share_percent,
        cause.name,
        cause.conclusion,
        cause.repo_count,
        cause.repos.join(", ")
    )
}

fn share_percent(count: usize, total: usize) -> u32 {
    if total == 0 {
        return 0;
    }
    u32::try_from(count.saturating_mul(100) / total).unwrap_or(u32::MAX)
}
