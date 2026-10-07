//! Live pushes for runner heartbeats.
//!
//! A gate slot's or the reviewer's heartbeat that starts or finishes a pass is
//! pushed as `runner.changed` frames. The admin-only [`RUNNERS_SCOPE`] gets
//! the whole beat, the same facts `GET /api/v1/control-plane/runners` shows
//! admins. Each repository the beat touches gets one frame on its
//! `repo.<owner>.<name>` scope that names only that repository, so a reader
//! who may see one repository learns nothing about another, and no runner
//! names. A page treats a frame like a nudge to refetch; a beat that changes
//! nothing pushes nothing. Background timers, deployers and the audit runner
//! push nothing: their beats are not passes.

use chrono::Utc;
use jeryu_readmodel::contracts::WebEvent;
use serde_json::{Value, json};

use super::{GateRunnerHeartbeat, runner_kind, work_label};
use crate::web::WebState;

/// Every gate and review change, for `/runners`. Admin-only, like
/// `GET /api/v1/control-plane/runners`.
pub(crate) const RUNNERS_SCOPE: &str = "runners";

/// Push `current` if it started or finished a pass since `previous`.
pub(crate) fn runner_changed(
    state: &WebState,
    previous: Option<&GateRunnerHeartbeat>,
    current: &GateRunnerHeartbeat,
) {
    let kind = runner_kind(current);
    if !matches!(kind, "gate" | "reviewer") {
        return;
    }
    let changed = previous.is_none_or(|previous| {
        previous.current != current.current || previous.last != current.last
    });
    if !changed {
        return;
    }
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
    publish(state, RUNNERS_SCOPE, &current.runner_id, summary, payload);
    for repo in touched_repos(previous, current) {
        let Some((owner, name)) = repo.split_once('/') else {
            continue;
        };
        let (summary, payload) = repo_frame(kind, previous, current, repo);
        publish(
            state,
            &format!("repo.{owner}.{name}"),
            repo,
            summary,
            payload,
        );
    }
}

fn publish(state: &WebState, scope: &str, entity: &str, summary: String, payload: Value) {
    let (scope_name, entity) = (scope.to_string(), entity.to_string());
    state.ws.publish(scope, move |seq| WebEvent {
        seq,
        timestamp: Utc::now().to_rfc3339(),
        scope: scope_name,
        kind: "runner.changed".to_string(),
        entity,
        summary,
        payload,
    });
}

/// Each repository whose page shows this change: the one being worked on now,
/// the one just finished, and the one the previous beat was working on (its
/// bar has to go away).
fn touched_repos<'a>(
    previous: Option<&'a GateRunnerHeartbeat>,
    current: &'a GateRunnerHeartbeat,
) -> Vec<&'a str> {
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
}

/// What `repo`'s own page may learn from this beat: whether a gate or review
/// started, finished or stopped on it, and its own pull request and sha.
/// Nothing about any other repository, and no runner name.
fn repo_frame(
    kind: &str,
    previous: Option<&GateRunnerHeartbeat>,
    current: &GateRunnerHeartbeat,
    repo: &str,
) -> (String, Value) {
    let noun = if kind == "reviewer" { "review" } else { "gate" };
    if let Some(task) = current.current.as_ref().filter(|task| task.repo == repo) {
        let label = work_label(repo, task.pr, &task.sha);
        return (
            format!("{noun} started on {label}"),
            json!({ "repo": repo, "pass": noun, "phase": "started", "pr": task.pr, "sha": task.sha }),
        );
    }
    let newly_finished = current
        .last
        .as_ref()
        .filter(|last| last.repo == repo)
        .filter(|last| previous.and_then(|p| p.last.as_ref()) != Some(*last));
    if let Some(last) = newly_finished {
        let label = work_label(repo, last.pr, &last.sha);
        return (
            format!("{noun} finished on {label}: {}", last.conclusion),
            json!({
                "repo": repo, "pass": noun, "phase": "finished", "pr": last.pr,
                "sha": last.sha, "conclusion": last.conclusion,
            }),
        );
    }
    (
        format!("{noun} on {repo} is no longer running"),
        json!({ "repo": repo, "pass": noun, "phase": "stopped" }),
    )
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
            touched_repos(Some(&previous), &current),
            vec!["acme/api", "globex/web"]
        );
    }

    #[test]
    fn one_repository_is_one_scope() {
        let current = beat(Some("acme/api"), Some("acme/api"));
        assert_eq!(touched_repos(None, &current), vec!["acme/api"]);
    }

    #[test]
    fn a_repository_frame_names_only_its_own_repository() {
        let previous = beat(Some("acme/api"), None);
        let current = beat(Some("initech/app"), Some("globex/web"));
        for repo in touched_repos(Some(&previous), &current) {
            let (summary, payload) = repo_frame("gate", Some(&previous), &current, repo);
            let text = format!("{summary} {payload}");
            for other in ["acme/api", "globex/web", "initech/app"] {
                assert_eq!(
                    text.contains(other),
                    other == repo,
                    "the {repo} frame and {other}: {text}"
                );
            }
            assert!(!text.contains("build-1"), "no runner name: {text}");
        }
        let (_, left) = repo_frame("gate", Some(&previous), &current, "acme/api");
        assert_eq!(left["phase"], "stopped");
        let (_, done) = repo_frame("gate", Some(&previous), &current, "globex/web");
        assert_eq!(done["phase"], "finished");
        assert_eq!(done["conclusion"], "success");
        let (_, started) = repo_frame("gate", Some(&previous), &current, "initech/app");
        assert_eq!(started["phase"], "started");
    }
}

#[cfg(test)]
mod scope_tests {
    use jeryu_core::{ForgeCore, UserRole};

    use super::RUNNERS_SCOPE;
    use crate::web::WebState;

    /// The runners scope carries every repository's passes, so it is exactly
    /// as closed as `GET /api/v1/control-plane/runners`: admins only.
    #[test]
    fn runners_scope_is_admin_only() {
        let core = ForgeCore::new();
        let admin = core
            .create_account("alice", "alice-password", UserRole::Admin)
            .unwrap();
        let user = core
            .create_account("bob", "bob-password", UserRole::User)
            .unwrap();
        let state = WebState::new(core);
        assert!(crate::web::ws::authorize_scope(
            &state,
            &admin,
            RUNNERS_SCOPE
        ));
        assert!(!crate::web::ws::authorize_scope(
            &state,
            &user,
            RUNNERS_SCOPE
        ));
    }
}
