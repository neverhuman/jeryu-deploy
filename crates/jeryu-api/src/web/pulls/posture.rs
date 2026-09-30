//! Required-check, review, and merge-passport posture for pull requests.

use super::*;

pub(super) fn passport_hash(
    state: &WebState,
    pr: &PullRequest,
    status: MergePassportStatus,
    blockers: &[MergePassportBlocker],
    review: &ReviewPosture,
    required_contexts: &[RequiredContextPosture],
) -> String {
    let core = state.github.core();
    let branch_protection = core
        .get_branch_protection(&pr.owner, &pr.repo, &pr.base.ref_name)
        .ok()
        .map(|rule| {
            let mut required_status_checks = rule.required_status_checks;
            required_status_checks.sort();
            required_status_checks.dedup();
            json!({
                "required_status_checks": required_status_checks,
                "required_approving_review_count": rule.required_approving_review_count,
                "enforce_admins": rule.enforce_admins,
                "required_linear_history": rule.required_linear_history,
                "require_signed_commits": rule.require_signed_commits,
                "require_jankurai_proof": rule.require_jankurai_proof,
            })
        });
    let blocker_codes = blockers
        .iter()
        .map(|blocker| blocker.code.as_str())
        .collect::<Vec<_>>();
    let required_context_states = required_contexts
        .iter()
        .map(|context| {
            json!({
                "name": context.name,
                "state": context.state.wire_name(),
            })
        })
        .collect::<Vec<_>>();
    let fingerprint = json!({
        "head_sha": pr.head.sha,
        "base_sha": pr.base.sha,
        "draft": pr.draft,
        "mergeable": pr.mergeable,
        "mergeable_state": pr.mergeable_state,
        "status": status,
        "blocker_codes": blocker_codes,
        "review": review,
        "branch_protection": branch_protection,
        "required_contexts": required_context_states,
    });
    format!(
        "passport:{}",
        hex::encode(Sha256::digest(fingerprint.to_string().as_bytes()))
    )
}

pub(super) fn passport(
    summary: &PullRequestSummary,
    pr: &PullRequest,
    required_contexts: &[RequiredContextPosture],
) -> MergePassport {
    let status = if summary.mergeable.can_merge {
        MergePassportStatus::Pass
    } else {
        MergePassportStatus::Blocked
    };
    MergePassport {
        status,
        head_sha: summary.head_sha.clone(),
        blockers: passport_blockers(required_contexts, &summary.review, pr),
        evaluated_at: server_time(),
    }
}

pub(super) fn passport_blockers(
    required_contexts: &[RequiredContextPosture],
    review: &ReviewPosture,
    pr: &PullRequest,
) -> Vec<MergePassportBlocker> {
    // A merged or closed pull request has no merge question left. Judging
    // its historical head against today's rules listed "missing" checks and
    // "the merge gate is blocked" on work that landed fine. The status type
    // has only pass and blocked (jeryu-readmodel), so the verdict stays
    // blocked; its single blocker says why in plain words, and clients tell a
    // finished pull request from `summary.state`.
    match web_pr_state(pr) {
        WebPullRequestState::Merged => {
            return vec![blocker(
                "passport_blocked_mergeability",
                "This pull request is already merged; there is nothing left to merge.",
                Some("merged"),
            )];
        }
        WebPullRequestState::Closed => {
            return vec![blocker(
                "passport_blocked_mergeability",
                "This pull request is closed; reopen it before it can merge.",
                Some("closed"),
            )];
        }
        WebPullRequestState::Open => {}
    }
    let mut blockers = Vec::new();
    if pr.draft {
        // `details` carries the route that clears this blocker, so the panel
        // showing it can also offer the button instead of only stating a rule.
        blockers.push(blocker(
            "passport_blocked_draft",
            "Draft pull requests cannot be merged: mark it ready for review.",
            Some(&format!("POST {}", super::ready_route(pr))),
        ));
    }
    for context in required_contexts {
        let (code, message) = match context.state {
            RequiredContextState::Missing => (
                "passport_blocked_checks_missing",
                format!(
                    "Required context `{}` has not run on this head.",
                    context.name
                ),
            ),
            RequiredContextState::Failing => (
                "passport_blocked_checks",
                format!("Required context `{}` is failing.", context.name),
            ),
            RequiredContextState::Pending => (
                "passport_blocked_pending_checks",
                format!("Required context `{}` is queued or running.", context.name),
            ),
            RequiredContextState::Passing => continue,
        };
        blockers.push(blocker(
            code,
            &message,
            Some(context.details.as_deref().unwrap_or(&context.name)),
        ));
    }
    if review.approvals < review.required_approvals {
        blockers.push(blocker(
            "passport_blocked_approvals",
            "Required approver count not satisfied.",
            None,
        ));
    }
    if review.changes_requested > 0 {
        blockers.push(blocker(
            "passport_blocked_changes_requested",
            "A reviewer requested changes on the current head.",
            None,
        ));
    }
    if review.unresolved_threads > 0 {
        blockers.push(blocker(
            "passport_blocked_threads",
            "Review threads are unresolved.",
            None,
        ));
    }
    if !pr.mergeable && blockers.is_empty() {
        blockers.push(blocker(
            "passport_blocked_mergeability",
            "The authoritative forge merge gate is blocked.",
            Some(&pr.mergeable_state),
        ));
    }
    blockers
}

fn blocker(code: &str, message: &str, details: Option<&str>) -> MergePassportBlocker {
    MergePassportBlocker {
        code: code.to_string(),
        message: message.to_string(),
        details: details.map(ToString::to_string),
    }
}

pub(super) fn required_contexts(state: &WebState, pr: &PullRequest) -> Vec<RequiredContextPosture> {
    required_contexts_with_enforcement(state, pr, audit_gate_enforced_for(&pr.owner, &pr.repo))
}

pub(super) fn required_contexts_with_enforcement(
    state: &WebState,
    pr: &PullRequest,
    audit_enforce_merge: bool,
) -> Vec<RequiredContextPosture> {
    let core = state.github.core();
    let mut names = BTreeSet::new();
    match core.get_branch_protection(&pr.owner, &pr.repo, &pr.base.ref_name) {
        Ok(rule) => {
            names.extend(rule.required_status_checks);
            if rule.require_jankurai_proof {
                names.insert("jankurai/proof".to_string());
            }
        }
        Err(ForgeError::NotFound(_)) => {}
        Err(_) => {
            return vec![RequiredContextPosture {
                name: "branch-protection/readback".to_string(),
                state: RequiredContextState::Failing,
                details: Some("could not read the exact base-branch protection rule".to_string()),
            }];
        }
    }

    if audit_enforce_merge {
        names.insert("jankurai/proof".to_string());
    }

    match core.evaluate_pull_request(&pr.owner, &pr.repo, pr.number, Some(&pr.head.sha)) {
        Ok(evaluation)
            if evaluation
                .blockers
                .iter()
                .any(|blocker| matches!(blocker, MergeBlocker::JankuraiProofRequired)) =>
        {
            names.insert("jankurai/proof".to_string());
        }
        Ok(_) => {}
        Err(_) => {
            return vec![RequiredContextPosture {
                name: "branch-protection/evaluation".to_string(),
                state: RequiredContextState::Failing,
                details: Some("could not evaluate the exact pull-request head".to_string()),
            }];
        }
    }

    let statuses = match core.combined_status(&pr.owner, &pr.repo, &pr.head.sha) {
        Ok(statuses) => statuses.statuses,
        Err(_) => {
            return vec![RequiredContextPosture {
                name: "required-context/status-readback".to_string(),
                state: RequiredContextState::Failing,
                details: Some("could not read exact-head commit statuses".to_string()),
            }];
        }
    };
    let check_runs = match core.list_check_runs(&pr.owner, &pr.repo, Some(&pr.head.sha)) {
        Ok(runs) => runs.check_runs,
        Err(_) => {
            return vec![RequiredContextPosture {
                name: "required-context/check-readback".to_string(),
                state: RequiredContextState::Failing,
                details: Some("could not read exact-head check runs".to_string()),
            }];
        }
    };

    names
        .into_iter()
        .map(|name| {
            let status = statuses
                .iter()
                .filter(|status| status.context == name)
                .max_by_key(|status| status.updated_at);
            if let Some(status) = status {
                let state = match status.state {
                    CommitStatusState::Success => RequiredContextState::Passing,
                    CommitStatusState::Pending => RequiredContextState::Pending,
                    CommitStatusState::Error | CommitStatusState::Failure => {
                        RequiredContextState::Failing
                    }
                };
                return RequiredContextPosture {
                    name,
                    state,
                    details: status
                        .target_url
                        .clone()
                        .or_else(|| status.description.clone()),
                };
            }

            let check = check_runs
                .iter()
                .filter(|check| check.name == name)
                .max_by_key(|check| check.completed_at.unwrap_or(check.started_at));
            match check {
                Some(check) => RequiredContextPosture {
                    name,
                    state: match check.status {
                        CheckRunStatus::Queued | CheckRunStatus::InProgress => {
                            RequiredContextState::Pending
                        }
                        CheckRunStatus::Completed
                            if check.conclusion == Some(CheckConclusion::Success) =>
                        {
                            RequiredContextState::Passing
                        }
                        CheckRunStatus::Completed => RequiredContextState::Failing,
                    },
                    details: check
                        .details_url
                        .clone()
                        .or_else(|| check.output.as_ref().map(|output| output.summary.clone())),
                },
                None => RequiredContextPosture {
                    details: Some(format!("required context `{name}` is missing")),
                    name,
                    state: RequiredContextState::Missing,
                },
            }
        })
        .collect()
}

fn audit_merge_enforced() -> bool {
    audit_merge_enforced_value(std::env::var("JERYU_AUDIT_ENFORCE_MERGE").ok().as_deref())
}

pub(in crate::web) fn audit_merge_enforced_value(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Which repositories the `jankurai/proof` gate is on for, when it is not on
/// family-wide: `owner/name` entries separated by commas or whitespace, and
/// `owner/*` for a whole owner. The rollout is per repo on purpose — a repo
/// whose main does not clear the floor yet would otherwise have every one of
/// its pull requests blocked from the moment the gate went on.
const AUDIT_GATE_REPOS_ENV: &str = "JERYU_AUDIT_GATE_REPOS";

/// Whether a failing or missing `jankurai/proof` blocks approval and merge for
/// this repository. `JERYU_AUDIT_ENFORCE_MERGE` turns it on family-wide; until
/// then each repo opts in by name.
pub(super) fn audit_gate_enforced_for(owner: &str, repo: &str) -> bool {
    audit_merge_enforced()
        || audit_gate_repo_listed(
            std::env::var(AUDIT_GATE_REPOS_ENV).ok().as_deref(),
            owner,
            repo,
        )
}

pub(in crate::web) fn audit_gate_repo_listed(value: Option<&str>, owner: &str, repo: &str) -> bool {
    let full = format!("{owner}/{repo}");
    let whole_owner = format!("{owner}/*");
    value
        .unwrap_or_default()
        .split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| entry.eq_ignore_ascii_case(&full) || entry.eq_ignore_ascii_case(&whole_owner))
}

/// Why an approval or a merge is refused while this repository is under the
/// gate: what the exact head's `jankurai/proof` says, in its own words.
pub(super) struct JankuraiGateVerdict {
    pub(super) state: RequiredContextState,
    pub(super) reason: String,
    pub(super) details_url: Option<String>,
}

impl JankuraiGateVerdict {
    /// How the check's state reads in a sentence about the refusal.
    pub(super) fn state_phrase(&self) -> &'static str {
        match self.state {
            RequiredContextState::Missing => "has not run on this head",
            RequiredContextState::Pending => "is still running on this head",
            RequiredContextState::Failing => "fails on this head",
            RequiredContextState::Passing => "passes on this head",
        }
    }
}

/// The exact head's `jankurai/proof` verdict when it stands in the way.
/// `None` when the gate is off for this repository, or the proof passed.
///
/// A scorer failure is not a pass: the push records a `tool-failed` score and
/// publishes a failing check whose title is the reason, and that reason is what
/// blocks here.
pub(super) fn jankurai_gate_blocker(
    state: &WebState,
    pr: &PullRequest,
) -> Option<JankuraiGateVerdict> {
    jankurai_gate_blocker_with(state, pr, audit_gate_enforced_for(&pr.owner, &pr.repo))
}

pub(super) fn jankurai_gate_blocker_with(
    state: &WebState,
    pr: &PullRequest,
    enforced: bool,
) -> Option<JankuraiGateVerdict> {
    if !enforced {
        return None;
    }
    let runs = state
        .github
        .core()
        .list_check_runs(&pr.owner, &pr.repo, Some(&pr.head.sha))
        .map(|list| latest_check_runs_by_name(list.check_runs))
        .unwrap_or_default();
    let Some(run) = runs.into_iter().find(|run| run.name == JANKURAI_PROOF) else {
        return Some(JankuraiGateVerdict {
            state: RequiredContextState::Missing,
            reason: format!(
                "no `{JANKURAI_PROOF}` has been published for {}; nothing has scored this head",
                &pr.head.sha
            ),
            details_url: None,
        });
    };
    let details_url = run.details_url.clone();
    let reason = run
        .output
        .as_ref()
        .map(|output| output.title.clone())
        .unwrap_or_else(|| format!("`{JANKURAI_PROOF}` published no verdict text"));
    match (run.status, run.conclusion) {
        (CheckRunStatus::Completed, Some(CheckConclusion::Success)) => None,
        (CheckRunStatus::Completed, _) => Some(JankuraiGateVerdict {
            state: RequiredContextState::Failing,
            reason,
            details_url,
        }),
        _ => Some(JankuraiGateVerdict {
            state: RequiredContextState::Pending,
            reason: format!("`{JANKURAI_PROOF}` is queued or running on this head"),
            details_url,
        }),
    }
}

pub(super) fn checks_for_pr(state: &WebState, pr: &PullRequest) -> PullRequestChecks {
    let core = state.github.core();
    let runs = match core.list_check_runs(&pr.owner, &pr.repo, Some(&pr.head.sha)) {
        Ok(list) => latest_check_runs_by_name(list.check_runs),
        Err(_) => Vec::new(),
    };
    let statuses = match core.combined_status(&pr.owner, &pr.repo, &pr.head.sha) {
        Ok(combined) => latest_statuses_by_context(combined.statuses),
        Err(_) => Vec::new(),
    };
    let required = required_context_names(state, pr);
    let mut passing = 0;
    let mut failing = 0;
    let mut pending = 0;
    let mut skipped = 0;
    let mut count = |bucket: &str| match bucket {
        "success" => passing += 1,
        "failure" => failing += 1,
        "pending" => pending += 1,
        "skipped" => skipped += 1,
        _ => {}
    };
    let mut checks = Vec::with_capacity(runs.len() + statuses.len());
    for run in &runs {
        count(check_bucket(run));
        let is_required = required.contains(&run.name);
        checks.push(PullRequestCheck {
            id: run.id.to_string(),
            name: run.name.clone(),
            kind: "check_run",
            status: check_status(run).to_string(),
            conclusion: run.conclusion.as_ref().map(conclusion),
            details_url: run.details_url.clone(),
            title: run.output.as_ref().map(|output| output.title.clone()),
            description: run.output.as_ref().map(|output| output.summary.clone()),
            details_text: run.output.as_ref().and_then(|output| output.text.clone()),
            web_url_label: check_web_url(pr, &run.name, run.details_url.as_deref())
                .map(|_| report_link_label(&run.name)),
            web_url: check_web_url(pr, &run.name, run.details_url.as_deref()),
            required: is_required,
            advisory: (!is_required).then(|| check_advisory(pr, &run.name)),
            started_at: Some(run.started_at.to_rfc3339()),
            completed_at: run.completed_at.map(|at| at.to_rfc3339()),
        });
    }
    for status in &statuses {
        let bucket = status_bucket(&status.state);
        count(bucket);
        let is_required = required.contains(&status.context);
        checks.push(PullRequestCheck {
            id: status.id.to_string(),
            name: status.context.clone(),
            kind: "status",
            status: bucket.to_string(),
            conclusion: None,
            details_url: status.target_url.clone(),
            title: None,
            description: status.description.clone(),
            details_text: None,
            web_url_label: status.target_url.as_ref().map(|_| "View log"),
            web_url: status.target_url.clone(),
            required: is_required,
            advisory: (!is_required).then(|| check_advisory(pr, &status.context)),
            started_at: Some(status.created_at.to_rfc3339()),
            completed_at: Some(status.updated_at.to_rfc3339()),
        });
    }
    PullRequestChecks {
        total: u32::try_from(checks.len()).unwrap_or(u32::MAX),
        passing,
        failing,
        pending,
        skipped,
        checks,
    }
}

/// The contexts the base branch waits for: its protection rule's required
/// checks, plus `jankurai/proof` when the rule or the merge enforcement asks.
fn required_context_names(state: &WebState, pr: &PullRequest) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if let Ok(rule) =
        state
            .github
            .core()
            .get_branch_protection(&pr.owner, &pr.repo, &pr.base.ref_name)
    {
        names.extend(rule.required_status_checks);
        if rule.require_jankurai_proof {
            names.insert(JANKURAI_PROOF.to_string());
        }
    }
    if audit_gate_enforced_for(&pr.owner, &pr.repo) {
        names.insert(JANKURAI_PROOF.to_string());
    }
    names
}

const JANKURAI_PROOF: &str = "jankurai/proof";

/// Why a check that is not required does not block the merge.
fn check_advisory(pr: &PullRequest, name: &str) -> CheckAdvisory {
    if name == JANKURAI_PROOF {
        return CheckAdvisory {
            label: "advisory - gate not enabled here".to_string(),
            reason: "jankurai/proof is a pre-approval gate in the repositories it has \
                     been rolled out to; this repository is not one of them yet, so its \
                     verdict is reported and does not block."
                .to_string(),
            url: Some("/quality-gate".to_string()),
        };
    }
    CheckAdvisory {
        label: "advisory - not in branch protection".to_string(),
        reason: format!(
            "The protection rule of `{}` does not list `{name}` as a required check.",
            pr.base.ref_name
        ),
        url: None,
    }
}

/// The human page behind a check row. `jankurai/proof` always has one (its
/// Quality gate head view); any other check links its `details_url` when that
/// is a web page rather than an `/api/` route.
fn check_web_url(pr: &PullRequest, name: &str, details_url: Option<&str>) -> Option<String> {
    if name == JANKURAI_PROOF {
        return Some(format!(
            "/quality-gate/heads/{}/{}/{}",
            pr.owner, pr.repo, pr.head.sha
        ));
    }
    details_url
        .filter(|url| crate::github::check_runs::details_url_problem(url).is_none())
        .map(str::to_string)
}

/// What the link on a check row says. `jankurai/proof` links its audit report;
/// every other check links whatever log its runner published.
fn report_link_label(name: &str) -> &'static str {
    if name == JANKURAI_PROOF {
        "View report"
    } else {
        "View log"
    }
}

fn latest_statuses_by_context(statuses: Vec<CommitStatus>) -> Vec<CommitStatus> {
    let mut latest = BTreeMap::<String, CommitStatus>::new();
    for status in statuses {
        match latest.entry(status.context.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(status);
            }
            std::collections::btree_map::Entry::Occupied(mut entry)
                if status.updated_at >= entry.get().updated_at =>
            {
                entry.insert(status);
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    }
    latest.into_values().collect()
}

fn status_bucket(state: &CommitStatusState) -> &'static str {
    match state {
        CommitStatusState::Success => "success",
        CommitStatusState::Pending => "pending",
        CommitStatusState::Error | CommitStatusState::Failure => "failure",
    }
}

fn latest_check_runs_by_name(runs: Vec<CheckRun>) -> Vec<CheckRun> {
    let mut latest = BTreeMap::<String, CheckRun>::new();
    for run in runs {
        match latest.entry(run.name.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(run);
            }
            std::collections::btree_map::Entry::Occupied(mut entry)
                if run.completed_at.unwrap_or(run.started_at)
                    >= entry.get().completed_at.unwrap_or(entry.get().started_at) =>
            {
                entry.insert(run);
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    }
    latest.into_values().collect()
}

fn check_bucket(run: &CheckRun) -> &'static str {
    match run.status {
        CheckRunStatus::Queued | CheckRunStatus::InProgress => "pending",
        CheckRunStatus::Completed => match run.conclusion {
            Some(CheckConclusion::Success) => "success",
            Some(CheckConclusion::Skipped | CheckConclusion::Neutral) => "skipped",
            _ => "failure",
        },
    }
}

fn check_status(run: &CheckRun) -> &'static str {
    match run.status {
        CheckRunStatus::Queued => "queued",
        CheckRunStatus::InProgress => "running",
        CheckRunStatus::Completed => match run.conclusion {
            Some(CheckConclusion::Success) => "success",
            Some(CheckConclusion::Skipped) => "skipped",
            Some(CheckConclusion::Cancelled) => "cancelled",
            Some(CheckConclusion::Neutral) => "neutral",
            _ => "failure",
        },
    }
}

fn conclusion(value: &CheckConclusion) -> String {
    match value {
        CheckConclusion::ActionRequired => "action_required",
        CheckConclusion::Cancelled => "cancelled",
        CheckConclusion::Failure => "failure",
        CheckConclusion::Neutral => "neutral",
        CheckConclusion::Success => "success",
        CheckConclusion::Skipped => "skipped",
        CheckConclusion::Superseded => check_conclusion_wire_value(&CheckConclusion::Superseded),
        CheckConclusion::TimedOut => "timed_out",
    }
    .to_string()
}

pub(super) fn review_posture(state: &WebState, pr: &PullRequest) -> ReviewPosture {
    let reviews = state
        .github
        .core()
        .list_reviews(&pr.owner, &pr.repo, pr.number)
        .unwrap_or_default();
    let comments = state
        .github
        .core()
        .list_review_comments(&pr.owner, &pr.repo, pr.number)
        .unwrap_or_default();
    let required_approvals = state
        .github
        .core()
        .get_branch_protection(&pr.owner, &pr.repo, &pr.base.ref_name)
        .map(|rule| u32::try_from(rule.required_approving_review_count).unwrap_or(u32::MAX))
        .unwrap_or(0);
    let effective = effective_reviews_for_head(&reviews, &pr.head.sha);
    ReviewPosture {
        required_approvals,
        approvals: effective
            .iter()
            .filter(|review| review.state == ReviewState::Approved)
            .count() as u32,
        changes_requested: effective
            .iter()
            .filter(|review| review.state == ReviewState::ChangesRequested)
            .count() as u32,
        unresolved_threads: comments.len() as u32,
        user_review_state: None,
    }
}

pub(super) fn reviews_for_pr(state: &WebState, pr: &PullRequest) -> Vec<PullRequestReview> {
    let reviews = state
        .github
        .core()
        .list_reviews(&pr.owner, &pr.repo, pr.number)
        .unwrap_or_default();
    let effective_ids = effective_reviews_for_head(&reviews, &pr.head.sha)
        .into_iter()
        .map(|review| review.id)
        .collect::<BTreeSet<_>>();
    reviews
        .into_iter()
        .map(|review| {
            let effective = effective_ids.contains(&review.id);
            PullRequestReview {
                id: review.id.to_string(),
                author: review.author,
                state: review_state_wire(&review.state).to_string(),
                body_markdown: review.body,
                submitted_at: review.submitted_at.to_rfc3339(),
                stale: review.head_sha.as_deref() != Some(pr.head.sha.as_str()),
                head_sha: review.head_sha,
                dismissed_review_id: review.dismissed_review_id.map(|id| id.to_string()),
                effective,
            }
        })
        .collect()
}

fn review_state_wire(state: &ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "APPROVED",
        ReviewState::ChangesRequested => "CHANGES_REQUESTED",
        ReviewState::Commented => "COMMENTED",
        ReviewState::Dismissed => "DISMISSED",
    }
}

pub(super) fn commit_tree_sha(state: &WebState, pr: &PullRequest, commit: &str) -> Option<String> {
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let repo = state.repo_manager.open_parts(&pr.owner, &pr.repo).ok()?;
    let tree_spec = format!("{commit}^{{tree}}");
    let output = std::process::Command::new(&state.repo_manager.config().git_bin)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &tree_spec,
        ])
        .current_dir(&repo.path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let tree = String::from_utf8(output.stdout).ok()?;
    let tree = tree.trim();
    if tree.len() == 40 && tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(tree.to_ascii_lowercase())
    } else {
        None
    }
}

pub(super) fn threads_for_pr(state: &WebState, pr: &PullRequest) -> Vec<ReviewThread> {
    let repo = find_repo(state, &format!("{}/{}", pr.owner, pr.repo))
        .expect("PR owner/repo must resolve to a repository");
    let comments = state
        .github
        .core()
        .list_review_comments(&pr.owner, &pr.repo, pr.number)
        .unwrap_or_default();
    comments
        .into_iter()
        .map(|comment| ReviewThread {
            id: comment.id.to_string(),
            repo: repo_id(&repo),
            pr_number: pr.number as u32,
            resolved: false,
            file_path: Some(comment.path.clone()),
            line: comment.line.map(|line| line as u32),
            anchor_sha: Some(pr.head.sha.clone()),
            comments: vec![WebReviewComment {
                id: comment.id.to_string(),
                author: comment.author,
                body_markdown: comment.body,
                body_html: None,
                created_at: comment.created_at.to_rfc3339(),
                edited_at: None,
                suggestion: None,
                evidence: None,
            }],
            created_at: comment.created_at.to_rfc3339(),
            updated_at: comment.created_at.to_rfc3339(),
        })
        .collect()
}

pub(super) fn comment_input(request: CreateReviewCommentRequest) -> Option<ReviewCommentInput> {
    let path = request.file_path?;
    Some(ReviewCommentInput {
        path,
        line: request.line.map(u64::from),
        body: request.body_markdown,
    })
}

pub(super) fn review_state(verdict: ReviewVerdict) -> ReviewState {
    match verdict {
        ReviewVerdict::Comment => ReviewState::Commented,
        ReviewVerdict::Approve => ReviewState::Approved,
        ReviewVerdict::RequestChanges => ReviewState::ChangesRequested,
    }
}

pub(super) fn web_pr_state(pr: &PullRequest) -> WebPullRequestState {
    if pr.merged || matches!(pr.state, jeryu_core::PullRequestState::Merged) {
        WebPullRequestState::Merged
    } else if matches!(pr.state, jeryu_core::PullRequestState::Closed) {
        WebPullRequestState::Closed
    } else {
        WebPullRequestState::Open
    }
}
