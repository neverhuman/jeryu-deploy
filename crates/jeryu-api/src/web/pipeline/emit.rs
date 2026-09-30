//! The forge's own emit points. Each helper turns something the server just
//! did into a [`NewEvent`] and hands it to [`super::emit`], which is
//! best-effort: none of these can fail the request they ride on.

use jeryu_core::{PullRequest, PullRequestState, Repository};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::super::WebState;
use super::super::control_plane::{GateRunnerHeartbeat, is_automation, is_reviewer, work_label};
use super::{NewEvent, emit};

/// A pull request event, tagged with the family and shift branch when the
/// head is a shift branch so it lands on the todo's trace.
pub(crate) struct PullEvent<'a> {
    pub kind: &'a str,
    pub actor: &'a str,
    pub outcome: Option<&'a str>,
    pub needs_human: bool,
    pub summary: String,
    pub reason: Option<String>,
    pub sha: Option<String>,
    pub detail: Option<Value>,
}

pub(crate) fn pull(state: &WebState, pr: &PullRequest, event: PullEvent<'_>) {
    let (family, shift) =
        super::super::shift::shift_context(state, &pr.owner, &pr.repo, &pr.head.ref_name);
    emit(
        state,
        NewEvent {
            actor: Some(event.actor.to_string()),
            family,
            repo: Some(format!("{}/{}", pr.owner, pr.repo)),
            pr: i64::try_from(pr.number).ok(),
            sha: Some(event.sha.unwrap_or_else(|| pr.head.sha.clone())),
            shift,
            outcome: event.outcome.map(str::to_string),
            needs_human: event.needs_human,
            reason: event.reason,
            detail: event.detail,
            ..NewEvent::forge(event.kind, event.summary)
        },
    );
}

/// `repo.archived` / `repo.unarchived`: the repository just became read-only,
/// or writable again. Archiving deletes nothing and is reversible, so the two
/// carry the same shape and both are a plain success -- the kind is what says
/// which way it went.
pub(crate) fn repository_archived(state: &WebState, repo: &Repository, actor: &str) {
    let (kind, summary) = if repo.archived {
        (
            "repo.archived",
            format!(
                "{} is archived: read-only until it is unarchived",
                repo.full_name
            ),
        )
    } else {
        (
            "repo.unarchived",
            format!("{} is unarchived: writable again", repo.full_name),
        )
    };
    emit(
        state,
        NewEvent {
            actor: Some(actor.to_string()),
            repo: Some(repo.full_name.clone()),
            outcome: Some("success".to_string()),
            needs_human: false,
            detail: Some(json!({ "archived": repo.archived })),
            ..NewEvent::forge(kind, summary)
        },
    );
}

/// `repo.renamed` / `repo.transferred`: the repository now lives at `to`.
/// The old slug keeps resolving (API reads and git clones) until a repository
/// is created there, so neither is a warning -- the kind says which move it
/// was, and `detail` carries `{from, to}` for the timeline.
pub(crate) fn repository_moved(state: &WebState, from: &str, repo: &Repository, actor: &str) {
    let from_owner = from.split_once('/').map_or(from, |(owner, _)| owner);
    let kind = if from_owner == repo.owner {
        "repo.renamed"
    } else {
        "repo.transferred"
    };
    let to = repo.full_name.clone();
    emit(
        state,
        NewEvent {
            actor: Some(actor.to_string()),
            repo: Some(to.clone()),
            outcome: Some("success".to_string()),
            needs_human: false,
            detail: Some(json!({ "from": from, "to": to })),
            ..NewEvent::forge(kind, format!("{from} moved to {to}"))
        },
    );
}

fn pr_label(pr: &PullRequest) -> String {
    format!("{}/{}#{}", pr.owner, pr.repo, pr.number)
}

/// `pr.review`: a review verdict on the exact head.
pub(crate) fn pull_reviewed(
    state: &WebState,
    pr: &PullRequest,
    reviewer: &str,
    verdict: &str,
    body: Option<&str>,
) {
    let changes = verdict == "request_changes";
    pull(
        state,
        pr,
        PullEvent {
            kind: "pr.review",
            actor: reviewer,
            outcome: Some(verdict),
            needs_human: changes,
            summary: format!("{reviewer} reviewed {}: {verdict}", pr_label(pr)),
            reason: body
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(str::to_string),
            sha: None,
            detail: Some(json!({ "title": pr.title, "author": pr.author })),
        },
    );
}

/// `pr.approved`: the one-click approval route.
pub(crate) fn pull_approved(state: &WebState, pr: &PullRequest, reviewer: &str) {
    pull(
        state,
        pr,
        PullEvent {
            kind: "pr.approved",
            actor: reviewer,
            outcome: Some("approve"),
            needs_human: false,
            summary: format!("{reviewer} approved {}", pr_label(pr)),
            reason: None,
            sha: None,
            detail: Some(json!({ "title": pr.title, "author": pr.author })),
        },
    );
}

/// `pr.merged`, from a direct merge (`via = "merge"`) or the merge queue.
pub(crate) fn pull_merged(state: &WebState, pr: &PullRequest, actor: &str, via: &str) {
    pull(
        state,
        pr,
        PullEvent {
            kind: "pr.merged",
            actor,
            outcome: Some("success"),
            needs_human: false,
            summary: format!("merged {}: {}", pr_label(pr), pr.title),
            reason: None,
            sha: pr.merge_commit_sha.clone(),
            detail: Some(json!({
                "via": via,
                "head_sha": pr.head.sha,
                "base": pr.base.ref_name,
                "author": pr.author,
            })),
        },
    );
}

/// `pr.opened`.
pub(crate) fn pull_opened(state: &WebState, pr: &PullRequest, actor: &str) {
    pull(
        state,
        pr,
        PullEvent {
            kind: "pr.opened",
            actor,
            outcome: None,
            needs_human: false,
            summary: format!("opened {}: {}", pr_label(pr), pr.title),
            reason: None,
            sha: None,
            detail: Some(json!({
                "head": pr.head.ref_name,
                "base": pr.base.ref_name,
                "author": pr.author,
            })),
        },
    );
}

/// Events for a successful write on the GitHub-compatible edge
/// (`/repos/*`, `/api/v3/repos/*`): pull requests opened, reviewed and merged
/// there, and the Deployments API, which is how `deploy-release.sh` reports.
pub(crate) fn github_edge(
    state: &WebState,
    write: bool,
    route_path: &str,
    actor: &str,
    request_body: &str,
    response_status: u16,
    response_body: &str,
) {
    if !write || !(200..300).contains(&response_status) {
        return;
    }
    let segments: Vec<&str> = route_path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let response: Value = serde_json::from_str(response_body).unwrap_or(Value::Null);
    let pull_request = |owner: &str, repo: &str, number: &str| {
        let number = number.parse::<u64>().ok()?;
        state.core.get_pull_request(owner, repo, number).ok()
    };
    let moved = |owner: &str, repo: &str| {
        let to = response["full_name"].as_str().unwrap_or_default();
        let from = format!("{owner}/{repo}");
        if to.is_empty() || to == from {
            return;
        }
        if let Ok(current) = state.core.get_repository(owner, repo) {
            repository_moved(state, &from, &current, actor);
        }
    };
    match segments.as_slice() {
        // The only write on this path is `PATCH`; it moves the repository
        // only when the body carried `name`.
        ["repos", owner, repo] => moved(owner, repo),
        ["repos", owner, repo, "transfer"] => moved(owner, repo),
        ["repos", owner, repo, "pulls"] => {
            let number = response["number"].as_u64().unwrap_or_default().to_string();
            if let Some(pr) = pull_request(owner, repo, &number) {
                pull_opened(state, &pr, actor);
            }
        }
        ["repos", owner, repo, "pulls", number, "merge"] => {
            if let Some(pr) = pull_request(owner, repo, number) {
                pull_merged(state, &pr, actor, "merge");
            }
        }
        ["repos", owner, repo, "pulls", number, "reviews"] => {
            let request: Value = serde_json::from_str(request_body).unwrap_or(Value::Null);
            let verdict = match request["event"].as_str().unwrap_or_default() {
                "APPROVE" | "APPROVED" => "approve",
                "REQUEST_CHANGES" | "CHANGES_REQUESTED" => "request_changes",
                _ => "comment",
            };
            if let Some(pr) = pull_request(owner, repo, number) {
                pull_reviewed(state, &pr, actor, verdict, request["body"].as_str());
            }
        }
        ["repos", owner, repo, "deployments"] => {
            deployment(state, owner, repo, actor, &response, None);
        }
        ["repos", owner, repo, "deployments", id, "statuses"] => {
            let Some(deployment_record) = id
                .parse::<u64>()
                .ok()
                .and_then(|id| state.core.get_deployment(owner, repo, id).ok())
            else {
                return;
            };
            let record = json!({
                "id": deployment_record.id,
                "sha": deployment_record.sha,
                "environment": deployment_record.environment,
                "payload": deployment_record.payload,
            });
            let request: Value = serde_json::from_str(request_body).unwrap_or(Value::Null);
            deployment(
                state,
                owner,
                repo,
                actor,
                &record,
                Some((&response, &request)),
            );
        }
        _ => {}
    }
}

/// `deploy.created`, or `deploy.status` when `posted` is the stored status
/// with the request that posted it. The Deployments API keeps only GitHub's
/// status fields; the request's `log_path` and `log_tail` (what
/// `deploy-release.sh` saw switch.sh print) go into the event's `detail`.
fn deployment(
    state: &WebState,
    owner: &str,
    repo: &str,
    actor: &str,
    deployment: &Value,
    posted: Option<(&Value, &Value)>,
) {
    let status = posted.map(|(status, _)| status);
    let request_text = |key: &str| {
        posted
            .and_then(|(_, request)| request[key].as_str())
            .filter(|text| !text.trim().is_empty())
    };
    let environment = deployment["environment"].as_str().unwrap_or("production");
    let release = deployment["payload"]["release"].as_str();
    let what = release.map_or_else(
        || {
            let sha = deployment["sha"].as_str().unwrap_or_default();
            format!("{owner}/{repo}@{}", sha.get(..10).unwrap_or(sha))
        },
        str::to_string,
    );
    let state_name = status.and_then(|s| s["state"].as_str());
    let failed = matches!(state_name, Some("failure" | "error"));
    let (kind, summary) = match state_name {
        Some(name) => (
            "deploy.status",
            format!("{environment} deploy of {what}: {name}"),
        ),
        None => (
            "deploy.created",
            format!("{environment} deploy of {what} started"),
        ),
    };
    emit(
        state,
        NewEvent {
            actor: Some(actor.to_string()),
            repo: Some(format!("{owner}/{repo}")),
            sha: deployment["sha"].as_str().map(str::to_string),
            outcome: state_name.map(str::to_string),
            needs_human: failed,
            reason: status
                .and_then(|s| s["description"].as_str())
                .map(str::to_string),
            log_url: status
                .and_then(|s| s["log_url"].as_str())
                .map(str::to_string),
            detail: Some(json!({
                "deployment_id": deployment["id"],
                "environment": environment,
                "release": release,
                "previous_release": deployment["payload"]["previous_release"],
                "log_path": request_text("log_path"),
                "log_tail": request_text("log_tail").map(tail_lines),
            })),
            ..NewEvent::forge(kind, summary)
        },
    );
}

/// JSON bytes a deploy event's `log_tail` may take, leaving room in the
/// 8 KiB of `detail` (`MAX_DETAIL_BYTES`) for the rest: a tail over that limit
/// would drop the whole event, and a failed deploy is the one with the
/// longest tail.
const LOG_TAIL_JSON_BYTES: usize = 6_000;

/// The last 20 lines of a posted log tail, cut from the front so the tail
/// takes at most [`LOG_TAIL_JSON_BYTES`] once encoded as a JSON string.
fn tail_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines[lines.len().saturating_sub(20)..].join("\n");
    let mut used = 0;
    let mut start = kept.len();
    for (index, ch) in kept.char_indices().rev() {
        let mut buffer = [0; 4];
        // The escaped length, less the two quotes.
        let encoded = Value::from(&*ch.encode_utf8(&mut buffer)).to_string().len() - 2;
        if used + encoded > LOG_TAIL_JSON_BYTES {
            break;
        }
        used += encoded;
        start = index;
    }
    kept[start..].to_string()
}

/// A heartbeat's optional pull request number as an event's: none stays none.
fn event_pr(pr: Option<u64>) -> Option<i64> {
    pr.and_then(|pr| i64::try_from(pr).ok())
        .filter(|pr| *pr > 0)
}

/// The number of the open pull request in `repo` (`owner/name`) whose head is
/// `sha`, if there is one.
fn open_pr_at_head(state: &WebState, repo: &str, sha: &str) -> Option<i64> {
    let (owner, name) = repo.split_once('/')?;
    let prs = state.core.list_pull_requests(owner, name, None).ok()?;
    prs.into_iter()
        .find(|pr| {
            !pr.merged
                && !matches!(
                    pr.state,
                    PullRequestState::Closed | PullRequestState::Merged
                )
                && pr.head.sha == sha
        })
        .and_then(|pr| i64::try_from(pr.number).ok())
}

/// `gate.started` / `gate.finished` (or `review.*` for pr-redteam) when a
/// runner's heartbeat shows a new `current` task or a new `last` result.
/// Runners beat every minute, so an unchanged beat emits nothing.
///
/// A background timer's beat (`automation` label) emits nothing at all: the
/// release scripts post their own `pin.*` and release events, and a second
/// `gate.*` line for the same fact would misreport it as a gate. A deploy
/// timer's beat (`deploy` label) is silent for the same reason: what it did is
/// a deployment, not a gate, and the repository page reads it as one.
pub(crate) fn runner_heartbeat(
    state: &WebState,
    previous: Option<&GateRunnerHeartbeat>,
    current: &GateRunnerHeartbeat,
) {
    if is_automation(current) || super::super::control_plane::is_deploy(current) {
        return;
    }
    let reviewer = is_reviewer(current);
    let (noun, verb) = if reviewer {
        ("review", "reviewing")
    } else {
        ("gate", "gating")
    };
    let source_detail = |recipe: &str| json!({ "runner": current.runner_id, "host": current.host, "recipe": recipe });
    if let Some(task) = &current.current
        && previous.and_then(|p| p.current.as_ref()) != Some(task)
    {
        // A runner may start before it knows the PR number (it sends 0), so
        // fall back to the open pull request whose head is the gated sha.
        let pr = event_pr(task.pr).or_else(|| open_pr_at_head(state, &task.repo, &task.sha));
        emit(
            state,
            NewEvent {
                actor: Some(current.runner_id.clone()),
                repo: Some(task.repo.clone()),
                pr,
                sha: Some(task.sha.clone()),
                detail: Some(source_detail(&task.recipe)),
                ..NewEvent::forge(
                    &format!("{noun}.started"),
                    format!(
                        "{} {verb} {}",
                        current.runner_id,
                        work_label(
                            &task.repo,
                            pr.and_then(|pr| u64::try_from(pr).ok()),
                            &task.sha
                        )
                    ),
                )
            },
        );
    }
    // A runner's first beat after a forge restart repeats an old `last`; only
    // a result that differs from the previous beat's is news.
    if let (Some(result), Some(previous)) = (&current.last, previous)
        && previous.last.as_ref() != Some(result)
    {
        let bad_gate = !reviewer && result.conclusion != "success";
        // A reviewer that held the PR, or ended without a usable verdict,
        // leaves the PR waiting on a person. A red gate is the author's next
        // step and shows up as failing checks instead.
        let reviewer_needs_human = reviewer
            && matches!(
                result.conclusion.as_str(),
                "hold" | "failed" | "publication_rejected" | "too_large"
            );
        emit(
            state,
            NewEvent {
                actor: Some(current.runner_id.clone()),
                repo: Some(result.repo.clone()),
                pr: event_pr(result.pr),
                sha: Some(result.sha.clone()),
                outcome: Some(result.conclusion.clone()),
                needs_human: reviewer_needs_human,
                seconds: i64::try_from(result.seconds).ok(),
                // A finished pass that went wrong always says why: a reader of
                // the feed sees "Review failed" with the cause beside it
                // instead of a red line with nothing to act on.
                reason: if bad_gate {
                    Some(format!(
                        "the {} recipe ended {}: {}",
                        result.recipe,
                        result.conclusion,
                        result_reason(result)
                    ))
                } else if reviewer_needs_human {
                    Some(format!(
                        "the {} review ended {}: {}",
                        result.recipe,
                        result.conclusion,
                        result_reason(result)
                    ))
                } else {
                    None
                },
                // The same verdict on the same head, beaten again after a
                // restart or a retry, collapses onto the event already stored.
                event_id: Some(result_event_id(noun, &current.runner_id, result)),
                detail: Some(source_detail(&result.recipe)),
                ..NewEvent::forge(
                    &format!("{noun}.finished"),
                    format!(
                        "{} {noun} of {}: {} in {}s",
                        current.runner_id,
                        work_label(&result.repo, result.pr, &result.sha),
                        result.conclusion,
                        result.seconds
                    ),
                )
            },
        );
    }
}

/// Why a finished pass ended where it did: the runner's own words when it sent
/// them, otherwise what its conclusion means. A runner that reports only
/// `failed` still leaves a sentence a reader can act on.
fn result_reason(result: &super::super::control_plane::GateRunnerResult) -> String {
    if let Some(reason) = result
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
    {
        return reason.to_string();
    }
    match result.conclusion.as_str() {
        "hold" => "the reviewer held the pull request for a person".to_string(),
        "failed" => "the pass ended without reaching a verdict and reported no reason".to_string(),
        "publication_rejected" => {
            "the verdict could not be published on the pull request".to_string()
        }
        "too_large" => "the diff was too large to review".to_string(),
        "interrupted" => "the pass was interrupted before it finished".to_string(),
        other => format!("the runner reported `{other}` and no reason"),
    }
}

/// One event per (runner, head, recipe, verdict): the id every repeat of that
/// same finished pass carries, so the store collapses them instead of painting
/// the feed with one line per retry.
fn result_event_id(
    noun: &str,
    runner_id: &str,
    result: &super::super::control_plane::GateRunnerResult,
) -> String {
    let digest = Sha256::digest(
        format!(
            "{runner_id}\n{}\n{:?}\n{}\n{}\n{}",
            result.repo, result.pr, result.sha, result.recipe, result.conclusion
        )
        .as_bytes(),
    );
    format!("{noun}.finished:{}", &hex::encode(digest)[..32])
}
