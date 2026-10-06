//! Live pushes for runner heartbeats.
//!
//! A heartbeat that starts or finishes a pass is pushed as one `runner.changed`
//! frame on the [`RUNNERS_SCOPE`] scope, and, for gate slots and the reviewer,
//! on the `repo.<owner>.<name>` scope of every repository the pass touches. A
//! page treats the frame as a nudge to refetch; it never carries more than the
//! heartbeat said, and a beat that changes nothing pushes nothing.

use chrono::Utc;
use jeryu_readmodel::contracts::WebEvent;
use serde_json::json;

use super::{GateRunnerHeartbeat, runner_kind, work_label};
use crate::web::WebState;

/// Every runner change, for `/runners`. Readable by any signed-in account,
/// like `GET /api/v1/control-plane/runners`.
pub(crate) const RUNNERS_SCOPE: &str = "runners";

/// Push `current` if it started or finished a pass since `previous`.
pub(crate) fn runner_changed(
    state: &WebState,
    previous: Option<&GateRunnerHeartbeat>,
    current: &GateRunnerHeartbeat,
) {
    let changed = previous.is_none_or(|previous| {
        previous.current != current.current || previous.last != current.last
    });
    if !changed {
        return;
    }
    let kind = runner_kind(current);
    let task = current.current.as_ref();
    let summary = match (task, current.last.as_ref()) {
        (Some(task), _) => format!(
            "{} started {}",
            current.runner_id,
            work_label(&task.repo, task.pr, &task.sha)
        ),
        (None, Some(last)) => format!(
            "{} finished {}: {}",
            current.runner_id,
            work_label(&last.repo, last.pr, &last.sha),
            last.conclusion
        ),
        (None, None) => format!("{} is idle", current.runner_id),
    };
    let payload = json!({
        "runnerId": current.runner_id,
        "kind": kind,
        "current": task.map(|task| json!({
            "repo": task.repo,
            "pr": task.pr,
            "sha": task.sha,
            "recipe": task.recipe,
            "startedAt": task.started_at.to_rfc3339(),
        })),
        "last": current.last.as_ref().map(|last| json!({
            "repo": last.repo,
            "pr": last.pr,
            "sha": last.sha,
            "recipe": last.recipe,
            "conclusion": last.conclusion,
            "seconds": last.seconds,
        })),
    });
    let frame = |scope: &str| {
        let (scope, summary, payload) = (scope.to_string(), summary.clone(), payload.clone());
        let entity = current.runner_id.clone();
        move |seq| WebEvent {
            seq,
            timestamp: Utc::now().to_rfc3339(),
            scope,
            kind: "runner.changed".to_string(),
            entity,
            summary,
            payload,
        }
    };
    state.ws.publish(RUNNERS_SCOPE, frame(RUNNERS_SCOPE));
    if !matches!(kind, "gate" | "reviewer") {
        return;
    }
    for scope in repo_scopes(previous, current) {
        state.ws.publish(&scope, frame(&scope));
    }
}

/// `repo.<owner>.<name>` for each repository whose page shows this change: the
/// one being worked on now, the one just finished, and the one the previous
/// beat was working on (its bar has to go away).
fn repo_scopes(
    previous: Option<&GateRunnerHeartbeat>,
    current: &GateRunnerHeartbeat,
) -> Vec<String> {
    let mut repos: Vec<&str> = Vec::new();
    repos.extend(current.current.as_ref().map(|task| task.repo.as_str()));
    repos.extend(current.last.as_ref().map(|last| last.repo.as_str()));
    repos.extend(
        previous
            .and_then(|previous| previous.current.as_ref())
            .map(|task| task.repo.as_str()),
    );
    repos.sort_unstable();
    repos.dedup();
    repos
        .into_iter()
        .filter_map(|repo| repo.split_once('/'))
        .map(|(owner, name)| format!("repo.{owner}.{name}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::control_plane::{GateRunnerResult, GateRunnerTask};

    fn beat(current: Option<&str>, last: Option<&str>) -> GateRunnerHeartbeat {
        let at = "2026-10-06T18:00:00Z".parse().expect("time");
        GateRunnerHeartbeat {
            runner_id: "build-1/slot0".to_string(),
            host: "build-1".to_string(),
            slot: 0,
            labels: Vec::new(),
            interval_seconds: None,
            current: current.map(|repo| GateRunnerTask {
                repo: repo.to_string(),
                pr: Some(7),
                sha: "abcdef1".to_string(),
                recipe: "just required".to_string(),
                target: None,
                started_at: at,
            }),
            last: last.map(|repo| GateRunnerResult {
                repo: repo.to_string(),
                pr: Some(6),
                sha: "1234567".to_string(),
                recipe: "just required".to_string(),
                conclusion: "success".to_string(),
                target: None,
                reason: None,
                seconds: 600,
                finished_at: at,
            }),
            code: None,
            tools: Vec::new(),
        }
    }

    #[test]
    fn a_finished_pass_reaches_the_repo_it_left_and_the_one_it_ran() {
        let previous = beat(Some("acme/api"), None);
        let current = beat(None, Some("globex/web"));
        assert_eq!(
            repo_scopes(Some(&previous), &current),
            vec!["repo.acme.api", "repo.globex.web"]
        );
    }

    #[test]
    fn one_repository_is_one_scope() {
        let current = beat(Some("acme/api"), Some("acme/api"));
        assert_eq!(repo_scopes(None, &current), vec!["repo.acme.api"]);
    }
}
