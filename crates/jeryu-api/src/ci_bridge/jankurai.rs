//! The verdict side of the governed jankurai: who may hand in a report, what
//! that report has to be bound to, and the `jankurai/proof` check derived from
//! it.
//!
//! The forge runs no auditor of its own. It records a ticket per head
//! ([`super::audit_queue`]), a gate runner runs the pinned binary, and the
//! report comes back through the authenticated ingest. The score a head keeps
//! is still the forge's own reading of the report JSON — a submission carries
//! evidence, never a verdict.

use super::*;

/// The auditor identity the governed installation receipt attests.
///
/// The receipt shipped with the forge is the family's statement of which
/// jankurai build is authoritative, so it is read — and checked against every
/// pinned constant beside it — before any claim is judged against it. A receipt
/// that drifted from the pin proves nothing, and fails closed here.
pub(crate) fn governed_auditor_identity() -> Result<(String, String), String> {
    let receipt: serde_json::Value =
        serde_json::from_str(GOVERNED_JANKURAI_INSTALLATION_RECEIPT_JSON)
            .map_err(|error| format!("governed jankurai receipt JSON failed: {error}"))?;
    let pinned = [
        ("/binary/version_output", GOVERNED_JANKURAI_VERSION),
        ("/binary/sha256", GOVERNED_JANKURAI_SHA256),
        ("/source/remote", GOVERNED_JANKURAI_SOURCE_REPO),
        ("/source/tag", GOVERNED_JANKURAI_SOURCE_TAG),
        ("/source/commit", GOVERNED_JANKURAI_SOURCE_REV),
        ("/source/tree", GOVERNED_JANKURAI_SOURCE_TREE),
        (
            "/source/archive_sha256",
            GOVERNED_JANKURAI_SOURCE_ARCHIVE_SHA256,
        ),
        (
            "/source/cargo_lock_sha256",
            GOVERNED_JANKURAI_CARGO_LOCK_SHA256,
        ),
        ("/build/rustc", GOVERNED_JANKURAI_RUSTC_VERSION),
        ("/build/cargo", GOVERNED_JANKURAI_CARGO_VERSION),
        ("/build/target_triple", GOVERNED_JANKURAI_TARGET_TRIPLE),
        ("/build/mode", GOVERNED_JANKURAI_BUILD_MODE),
        ("/governance/manifest_repo", GOVERNED_JANKURAI_MANIFEST_REPO),
        (
            "/governance/manifest_commit",
            GOVERNED_JANKURAI_MANIFEST_COMMIT,
        ),
        ("/governance/manifest_tree", GOVERNED_JANKURAI_MANIFEST_TREE),
        (
            "/governance/manifest_sha256",
            GOVERNED_JANKURAI_MANIFEST_SHA256,
        ),
        ("/governance/status", "governed"),
        ("/conclusion", "success"),
    ];
    for (pointer, expected) in pinned {
        if receipt.pointer(pointer).and_then(serde_json::Value::as_str) != Some(expected) {
            return Err(format!(
                "governed jankurai receipt disagrees with the pinned auditor: {pointer}"
            ));
        }
    }
    Ok((
        GOVERNED_JANKURAI_VERSION.to_string(),
        GOVERNED_JANKURAI_SHA256.to_string(),
    ))
}

/// What a runner says it ran, checked against the governed receipt. A report
/// produced by any other binary is refused rather than recorded as a pass.
pub(crate) fn verify_reported_auditor(version: &str, sha256: &str) -> Result<(), String> {
    let (expected_version, expected_sha256) = governed_auditor_identity()?;
    if version.trim() != expected_version {
        return Err(format!(
            "reported jankurai version is not the governed one: {version}"
        ));
    }
    if sha256.trim().to_ascii_lowercase() != expected_sha256 {
        return Err(format!(
            "reported jankurai sha256 is not the governed one: {sha256}"
        ));
    }
    Ok(())
}

/// A Git object id as the ingest accepts it: a full, lowercase hex sha.
pub(crate) fn is_object_id(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The proof of a head whose audit is waiting for a gate runner. It is never
/// completed here: with no runner, `jankurai/proof` stays pending and says so.
pub(crate) fn jankurai_queued_output(ticket: &super::audit_queue::AuditTicket) -> CheckRunOutput {
    CheckRunOutput {
        title: "queued for a gate runner".to_string(),
        summary: format!(
            "- head: {}\n- base: {}\n- state: waiting for a gate runner to claim and audit this head\n\nThe forge records audit work; it does not run the auditor. This check completes \
             only when a runner submits a provenance-checked report for this exact head.",
            ticket.head_sha, ticket.base_sha
        ),
        text: None,
    }
}

/// The proof of a head with nothing to diff against. Not a tool failure, and
/// not an excuse to audit the whole repository.
pub(crate) fn jankurai_no_base_output() -> CheckRunOutput {
    CheckRunOutput {
        title: "no base branch yet".to_string(),
        summary: "This head has no merge-base with `main`, so there is no diff to audit. \
                  Land a `main` for this repository and the next push is audited against it."
            .to_string(),
        text: None,
    }
}

/// The proof of a head that needs an audit no ticket could be recorded for.
pub(crate) fn jankurai_unqueued_output(reason: &str) -> CheckRunOutput {
    CheckRunOutput {
        title: "waiting for audit capacity".to_string(),
        summary: format!(
            "This head still needs an audit: {reason}. The check stays pending until a gate \
             runner audits it; the forge never scores a head itself."
        ),
        text: None,
    }
}

/// The check name, posted at most once per head.
pub(super) const JANKURAI_PROOF_CHECK: &str = "jankurai/proof";
/// Public origin of the forge web UI, e.g. `https://git.neverhuman.org`. The
/// production unit sets it; it wins over the pushing client's `Host` header,
/// which on this host is the local tunnel address nobody else can open.
const PUBLIC_ORIGIN_ENV: &str = "JERYU_PRODUCTION_ORIGIN";

/// Link the proof check at the Quality gate page of this head, where each
/// applied cap is listed with what it means and how to clear it. A web page
/// over https, never the raw score JSON under `/api/`.
pub(crate) fn jankurai_score_details_url(
    origin_base_url: &str,
    owner: &str,
    repo: &str,
    head_sha: &str,
) -> Option<String> {
    proof_details_url(
        std::env::var(PUBLIC_ORIGIN_ENV).ok().as_deref(),
        origin_base_url,
        &format!("/quality-gate/heads/{owner}/{repo}/{head_sha}"),
    )
}

/// The one URL every proof of a head links to: the configured public origin,
/// or the origin the push arrived on when that is reachable from outside this
/// machine. A loopback or tunnel address is dropped rather than published — a
/// link only this host can open reads as a working report and is not one, and
/// two audits of one head that disagreed only about it produced the duplicate
/// checks readers could not tell apart.
pub(super) fn proof_details_url(
    public_origin: Option<&str>,
    origin_base_url: &str,
    path: &str,
) -> Option<String> {
    let base = public_origin
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .or_else(|| Some(origin_base_url.trim()).filter(|origin| !origin.is_empty()))
        .filter(|origin| !is_host_local(origin))?;
    Some(crate::github::check_runs::web_page_url(base, path))
}

/// Whether `base` names this machine only.
fn is_host_local(base: &str) -> bool {
    let host = base
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = host.split('/').next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(name, _)| name);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.is_empty()
        || host == "localhost"
        || host == "0.0.0.0"
        || host == "::1"
        || host.starts_with("127.")
        || host.ends_with(".localhost")
}

const MAX_REASON_CHARS: usize = 200;

fn truncate_reason(reason: &str) -> String {
    let reason = reason.trim();
    if reason.chars().count() <= MAX_REASON_CHARS {
        return reason.to_string();
    }
    let kept: String = reason.chars().take(MAX_REASON_CHARS).collect();
    format!("{kept}...")
}

/// One `jankurai/proof` per head. Core's check list is append-only, so a
/// re-audit that reaches the same verdict must post nothing: a second row with
/// the same name leaves a reader guessing which one is current.
pub(super) fn proof_already_posted(
    core: &ForgeCore,
    owner: &str,
    repo: &str,
    head_sha: &str,
    conclusion: &CheckConclusion,
    details_url: &Option<String>,
    output: &CheckRunOutput,
) -> bool {
    let Ok(existing) = core.list_check_runs(owner, repo, Some(head_sha)) else {
        return false;
    };
    existing.check_runs.iter().any(|run| {
        run.name == JANKURAI_PROOF_CHECK
            && run.status == CheckRunStatus::Completed
            && run.conclusion.as_ref() == Some(conclusion)
            && &run.details_url == details_url
            && run.output.as_ref().is_some_and(|posted| {
                posted.title == output.title && posted.summary == output.summary
            })
    })
}

/// Human-readable verdict for the `jankurai/proof` check: score against the
/// effective floor, applied caps, and hard findings.
pub(crate) fn jankurai_proof_output(
    request: &RecordJankuraiScoreRequest,
    pass: bool,
) -> CheckRunOutput {
    let Some(score) = request.score else {
        let exit = request
            .tool_exit
            .map_or_else(|| "unknown".to_string(), |code| code.to_string());
        let reason = tool_failure_reason(request);
        return CheckRunOutput {
            title: format!("jankurai audit produced no score: {reason}"),
            summary: format!(
                "The authoritative jankurai audit did not produce a valid report \
                 (decision `{}`, exit {exit}); the proof fails closed.\n\n\
                 - reason: {reason}\n\
                 - what to do: rerun the audit on this head once the reason above is \
                 addressed; the Quality gate page of this head keeps the audit's own \
                 output.",
                request.decision
            ),
            text: Some(format!("jankurai audit failed: {reason}")),
        };
    };
    let floor = request
        .report
        .as_ref()
        .and_then(|report| report.pointer("/decision/minimum_score"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0)
        .max(HOST_JANKURAI_MINIMUM_SCORE);
    let hard_findings = request.hard_findings.unwrap_or(0);
    let title = if pass {
        format!("score {score} >= floor {floor}")
    } else if score < floor {
        format!("score {score} < floor {floor}")
    } else if hard_findings > 0 {
        format!("score {score} >= floor {floor}, {hard_findings} hard finding(s)")
    } else {
        format!(
            "score {score} >= floor {floor}, {} cap(s) applied",
            request.caps_applied.len()
        )
    };
    let caps = if request.caps_applied.is_empty() {
        "none".to_string()
    } else {
        request.caps_applied.join(", ")
    };
    CheckRunOutput {
        title,
        summary: format!(
            "- score: {score}\n- floor: {floor}\n- caps applied: {caps}\n- hard findings: {hard_findings}"
        ),
        text: (!pass).then(|| top_findings_text(request.report.as_ref())),
    }
}

/// How many findings a failing proof lists before it points at the report.
const PROOF_TEXT_FINDINGS: usize = 5;

/// The findings a reader needs to start fixing the head: rule id, `path:line`
/// and the auditor's own sentence, in report order.
fn top_findings_text(report: Option<&serde_json::Value>) -> String {
    let findings = report
        .and_then(|report| report.get("findings"))
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if findings.is_empty() {
        return "The audit recorded no individual findings; the Quality gate page of this \
                head has the full report."
            .to_string();
    }
    let mut lines = vec!["Top findings:".to_string()];
    for finding in findings.iter().take(PROOF_TEXT_FINDINGS) {
        let string = |key: &str| {
            finding
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let rule = string("rule_id").unwrap_or_else(|| "unknown".to_string());
        let mut where_at = string("path").unwrap_or_else(|| "(no path)".to_string());
        if let Some(line) = finding.get("line").and_then(serde_json::Value::as_i64) {
            where_at = format!("{where_at}:{line}");
        }
        let problem = string("problem").unwrap_or_else(|| "no description".to_string());
        lines.push(format!("- {rule} at {where_at}: {problem}"));
    }
    if findings.len() > PROOF_TEXT_FINDINGS {
        lines.push(format!(
            "- ... and {} more on the Quality gate page of this head.",
            findings.len() - PROOF_TEXT_FINDINGS
        ));
    }
    lines.join("\n")
}

/// Why a `tool-failed` score has no number. The host's own error, recorded with
/// the score when the audit never produced a report, is the whole point of the
/// check: "tool-failed" alone tells a reader nothing.
fn tool_failure_reason(request: &RecordJankuraiScoreRequest) -> String {
    request
        .report
        .as_ref()
        .and_then(|report| report.pointer(HOST_ERROR_POINTER))
        .and_then(serde_json::Value::as_str)
        .map(truncate_reason)
        .or_else(|| {
            request.report.as_ref().map(|_| {
                "the audit report is not a valid diff-score JSON (score, caps_applied, \
                 decision.hard_findings and decision.minimum_score must all be present \
                 and in range)"
                    .to_string()
            })
        })
        .unwrap_or_else(|| "the audit wrote no report at all".to_string())
}

/// Where the host's own failure reason lives inside a stored `tool-failed`
/// report, so the Quality gate page and the check output read the same words.
const HOST_ERROR_POINTER: &str = "/host_error";

/// The reasonless form: a runner's submitted report either parses or it does
/// not, and the ingest records a `tool-failed` score without a host reason.
pub(crate) fn jankurai_score_request(
    branch: &str,
    commit_sha: &str,
    report: Option<serde_json::Value>,
    exit_code: i64,
) -> (RecordJankuraiScoreRequest, bool) {
    jankurai_score_request_with_reason(branch, commit_sha, report, exit_code, None)
}

/// As [`jankurai_score_request`], recording `reason` — what the host saw go
/// wrong — with a score the auditor never produced.
pub(super) fn jankurai_score_request_with_reason(
    branch: &str,
    commit_sha: &str,
    report: Option<serde_json::Value>,
    exit_code: i64,
    reason: Option<String>,
) -> (RecordJankuraiScoreRequest, bool) {
    let parsed = report.as_ref().and_then(|report| {
        if exit_code != 0 {
            return None;
        }
        let score = u32::try_from(report.get("score")?.as_u64()?).ok()?;
        let decision = report.get("decision")?;
        let hard_findings = u32::try_from(decision.get("hard_findings")?.as_u64()?).ok()?;
        let minimum_score = u32::try_from(decision.get("minimum_score")?.as_u64()?).ok()?;
        if score > 100 || minimum_score > 100 {
            return None;
        }
        let caps_applied = report
            .get("caps_applied")?
            .as_array()?
            .iter()
            .map(|cap| cap.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()?;
        Some((score, hard_findings, minimum_score, caps_applied))
    });

    match parsed {
        Some((score, hard_findings, minimum_score, caps_applied)) => {
            let effective_floor = minimum_score.max(HOST_JANKURAI_MINIMUM_SCORE);
            let pass = score >= effective_floor && hard_findings == 0 && caps_applied.is_empty();
            (
                RecordJankuraiScoreRequest {
                    branch: branch.to_string(),
                    commit_sha: commit_sha.to_string(),
                    score: Some(score),
                    hard_findings: Some(hard_findings),
                    decision: "scored".to_string(),
                    caps_applied,
                    report,
                    tool_exit: None,
                },
                pass,
            )
        }
        None => (
            RecordJankuraiScoreRequest {
                branch: branch.to_string(),
                commit_sha: commit_sha.to_string(),
                score: None,
                hard_findings: None,
                decision: "tool-failed".to_string(),
                caps_applied: Vec::new(),
                report: report_with_reason(report, reason),
                tool_exit: Some(exit_code),
            },
            false,
        ),
    }
}

/// Keep the host's failure reason with the stored score: an audit that wrote no
/// report gets one whose only key is the reason, and a report the auditor did
/// write keeps every key it has plus the reason.
fn report_with_reason(
    report: Option<serde_json::Value>,
    reason: Option<String>,
) -> Option<serde_json::Value> {
    let reason = reason.map(|reason| truncate_reason(&reason));
    match (report, reason) {
        (Some(mut report), Some(reason)) => {
            if let Some(object) = report.as_object_mut() {
                object.insert("host_error".to_string(), serde_json::Value::from(reason));
            }
            Some(report)
        }
        (Some(report), None) => Some(report),
        (None, Some(reason)) => Some(serde_json::json!({ "host_error": reason })),
        (None, None) => None,
    }
}
