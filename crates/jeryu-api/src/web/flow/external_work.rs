//! Passive external-origin observations, separate from native lifecycle events.

use serde::{Deserialize, Serialize};

use crate::web::WebState;
use crate::web::shift::queue::{discover, git, show_file};

const SNAPSHOT: &str = "tracking/external-work.json";
const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct ExternalWork {
    pub snapshots: Vec<Snapshot>,
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Snapshot {
    pub schema_version: u32,
    pub family: String,
    pub source_repo: String,
    pub provider: String,
    pub source_url: String,
    pub default_branch: String,
    pub observed_at: String,
    pub task_observed_at: String,
    pub branch_observed_at: String,
    pub branches: Vec<Branch>,
    pub phases: Vec<Phase>,
    pub milestones: Vec<Milestone>,
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PullRequest {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Branch {
    pub name: String,
    pub head_sha: String,
    pub ahead_by: Option<u64>,
    pub behind_by: Option<u64>,
    pub comparison: String,
    pub state: String,
    pub task_id: Option<String>,
    pub url: String,
    pub pull_request: Option<PullRequest>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Phase {
    pub id: String,
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Milestone {
    pub id: String,
    pub label: String,
    pub task_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Task {
    pub todo_id: String,
    pub task_id: String,
    pub title: String,
    pub phase_id: String,
    pub phase_source: String,
    pub parent_id: Option<String>,
    pub milestone_ids: Vec<String>,
    pub priority_rank: u32,
    pub critical_path: bool,
    pub state: String,
    pub reason: String,
    pub dependencies: Vec<String>,
    pub pending_dependencies: Vec<String>,
    pub merged: bool,
    pub observed_efforts: u32,
    pub source_url: String,
    pub pull_requests: Vec<PullRequest>,
}

fn https(text: &str) -> bool {
    reqwest::Url::parse(text).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

impl Snapshot {
    fn validate(&self, family: &str) -> Result<(), String> {
        if self.tasks.len() > 5000
            || self.branches.len() > 5000
            || self.phases.len() > 256
            || self.milestones.len() > 256
        {
            return Err("external-work item count exceeds the read budget".to_string());
        }
        if self.schema_version != 1 || self.family != family || !https(&self.source_url) {
            return Err("external-work schema, family or source URL is invalid".to_string());
        }
        for time in [
            &self.observed_at,
            &self.task_observed_at,
            &self.branch_observed_at,
        ] {
            chrono::DateTime::parse_from_rfc3339(time)
                .map_err(|_| "external-work observation date is invalid")?;
        }
        let ids: std::collections::BTreeSet<_> = self
            .tasks
            .iter()
            .map(|task| task.todo_id.as_str())
            .collect();
        let phases: std::collections::BTreeSet<_> =
            self.phases.iter().map(|phase| phase.id.as_str()).collect();
        let parents: std::collections::BTreeMap<_, _> = self
            .tasks
            .iter()
            .map(|task| (task.todo_id.as_str(), task.parent_id.as_deref()))
            .collect();
        if ids.len() != self.tasks.len() || phases.len() != self.phases.len() {
            return Err("external-work task or phase IDs are duplicated".to_string());
        }
        for task in &self.tasks {
            if !safe_id(&task.todo_id)
                || !phases.contains(task.phase_id.as_str())
                || !https(&task.source_url)
                || ![
                    "planned",
                    "in_progress",
                    "review",
                    "needs_human",
                    "waiting_dependency",
                    "blocked",
                    "merged",
                ]
                .contains(&task.state.as_str())
            {
                return Err(format!(
                    "external-work task {} has invalid metadata",
                    task.todo_id
                ));
            }
            if let Some(parent) = &task.parent_id
                && (parent == &task.todo_id || !ids.contains(parent.as_str()))
            {
                return Err(format!(
                    "external-work task {} has an invalid parent",
                    task.todo_id
                ));
            }
            if task
                .dependencies
                .iter()
                .chain(&task.pending_dependencies)
                .any(|id| !ids.contains(id.as_str()))
                || task.pull_requests.iter().any(|pr| !https(&pr.url))
            {
                return Err(format!(
                    "external-work task {} has invalid evidence links",
                    task.todo_id
                ));
            }
            // The UI supports parent/follow-up hierarchy; reject cycles at any depth.
            let mut visited = std::collections::BTreeSet::new();
            let mut cursor = Some(task.todo_id.as_str());
            while let Some(id) = cursor {
                if !visited.insert(id) {
                    return Err("external-work task parent cycle".to_string());
                }
                if visited.len() > 64 {
                    return Err("external-work hierarchy exceeds64 levels".to_string());
                }
                cursor = parents.get(id).copied().flatten();
            }
        }
        if self.milestones.iter().any(|milestone| {
            milestone
                .task_ids
                .iter()
                .any(|id| !ids.contains(id.as_str()))
        }) {
            return Err("external-work milestone references an unknown task".to_string());
        }
        for branch in &self.branches {
            if branch.head_sha.len() != 40
                || !branch.head_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
                || !https(&branch.url)
                || branch
                    .pull_request
                    .as_ref()
                    .is_some_and(|pr| !https(&pr.url))
                || ![
                    "review",
                    "merged",
                    "closed",
                    "contained",
                    "changed_after_merge",
                    "unreviewed",
                    "unknown",
                ]
                .contains(&branch.state.as_str())
                || branch
                    .task_id
                    .as_ref()
                    .is_some_and(|id| !ids.contains(id.as_str()))
            {
                return Err(format!(
                    "external-work branch {} has invalid evidence",
                    branch.name
                ));
            }
        }
        Ok(())
    }
}

/// Read dated observations from queue Git blobs. No provider calls, lifecycle writes or claims.
pub(crate) fn read(state: &WebState, family: Option<&str>, repo: Option<&str>) -> ExternalWork {
    let mut result = ExternalWork::default();
    let binary = &state.repo_manager.config().git_bin;
    for queue in discover(&state.repo_manager) {
        if family.is_some_and(|family| family != queue.family.name) {
            continue;
        }
        let spec = format!("{}:{SNAPSHOT}", queue.head);
        let Ok(size) = git(binary, &queue.path, &["cat-file", "-s", &spec], &[], None) else {
            continue;
        };
        let size = String::from_utf8_lossy(&size).trim().parse::<usize>();
        if !size.is_ok_and(|size| size <= MAX_BYTES) {
            result.problems.push(format!(
                "{}: external observations exceed the read budget",
                queue.full_name()
            ));
            continue;
        }
        let parsed = show_file(binary, &queue.path, &queue.head, SNAPSHOT)
            .ok_or_else(|| "external observation blob unavailable".to_string())
            .and_then(|text| {
                serde_json::from_str::<Snapshot>(&text)
                    .map_err(|error| format!("external observation JSON: {error}"))
            })
            .and_then(|snapshot| {
                snapshot.validate(&queue.family.name)?;
                Ok(snapshot)
            });
        match parsed {
            Ok(snapshot)
                if repo.is_none_or(|repo| snapshot.source_repo.eq_ignore_ascii_case(repo)) =>
            {
                result.snapshots.push(snapshot)
            }
            Ok(_) => {}
            Err(error) => result
                .problems
                .push(format!("{}: {error}", queue.full_name())),
        }
    }
    result
}

#[cfg(test)]
mod tests;
