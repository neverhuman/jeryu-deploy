//! The forge's own emit points. Each helper turns something the server just
//! did into a [`NewEvent`] and hands it to [`super::emit`], which is
//! best-effort: none of these can fail the request they ride on.

use jeryu_core::{PullRequest, Repository};
use serde_json::{Value, json};

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
    match segments.as_slice() {
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
            deployment(state, owner, repo, actor, &record, Some(&response));
        }
        _ => {}
    }
}

/// `deploy.created`, or `deploy.status` when `status` is the posted status.
fn deployment(
    state: &WebState,
    owner: &str,
    repo: &str,
    actor: &str,
    deployment: &Value,
    status: Option<&Value>,
) {
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
            })),
            ..NewEvent::forge(kind, summary)
        },
    );
}

/// A heartbeat's optional pull request number as an event's: none stays none.
fn event_pr(pr: Option<u64>) -> Option<i64> {
    pr.and_then(|pr| i64::try_from(pr).ok())
        .filter(|pr| *pr > 0)
}

/// `gate.started` / `gate.finished` (or `review.*` for pr-redteam) when a
/// runner's heartbeat shows a new `current` task or a new `last` result.
/// Runners beat every minute, so an unchanged beat emits nothing.
///
/// A background timer's beat (`automation` label) emits nothing at all: the
/// release scripts post their own `pin.*` and release events, and a second
/// `gate.*` line for the same fact would misreport it as a gate.
pub(crate) fn runner_heartbeat(
    state: &WebState,
    previous: Option<&GateRunnerHeartbeat>,
    current: &GateRunnerHeartbeat,
) {
    if is_automation(current) {
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
        emit(
            state,
            NewEvent {
                actor: Some(current.runner_id.clone()),
                repo: Some(task.repo.clone()),
                pr: event_pr(task.pr),
                sha: Some(task.sha.clone()),
                detail: Some(source_detail(&task.recipe)),
                ..NewEvent::forge(
                    &format!("{noun}.started"),
                    format!(
                        "{} {verb} {}",
                        current.runner_id,
                        work_label(&task.repo, task.pr, &task.sha)
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
                reason: bad_gate
                    .then(|| format!("the {} recipe ended {}", result.recipe, result.conclusion)),
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
