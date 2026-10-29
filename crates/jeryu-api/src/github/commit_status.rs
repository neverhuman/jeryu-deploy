//! Commit-status routes (combined status read and status creation) and their
//! GitHub-shaped renderers.

use jeryu_core::{CombinedStatus, CommitStatus, CommitStatusState, CreateCommitStatusRequest};
use serde_json::{Value, json};

use crate::routes::Response;

use super::GithubRouter;
use super::support::{actor, error_response, json_response, owner_json, parse_body};

impl GithubRouter {
    /// `GET /repos/:o/:r/commits/:ref/status`. With `?base=<branch>` the
    /// rollup answers for a merge into that base: statuses proved against
    /// another base are left out (core `status_answers_for_base`).
    /// Without it, every status on the commit counts, as on GitHub.
    pub(super) fn commit_status(
        &self,
        owner: &str,
        repo: &str,
        reference: &str,
        query: &str,
    ) -> Response {
        let base = super::listing::pairs(query)
            .find(|(key, _)| key == "base")
            .map(|(_, value)| value);
        let combined = match base {
            Some(base) => self
                .core
                .combined_status_for_base(owner, repo, reference, &base),
            None => self.core.combined_status(owner, repo, reference),
        };
        match combined {
            Ok(combined) => json_response(200, &combined_status_json(reference, &combined)),
            Err(err) => error_response(err),
        }
    }

    pub(super) fn create_status(&self, owner: &str, repo: &str, sha: &str, body: &str) -> Response {
        let req: CreateCommitStatusRequest = match parse_body(body) {
            Ok(value) => value,
            Err(response) => return response,
        };
        let creator = actor(body);
        // A green gate run on the default branch's own head is what moves the
        // green ref. Read the two fields that decide it before the request is
        // consumed; the ref moves only after the status is recorded, so a
        // failure there leaves no ref claiming a verdict the forge does not
        // hold.
        let proves_default_branch = matches!(req.state, CommitStatusState::Success)
            && req.context == main_health_context(repo);
        match self
            .core
            .create_commit_status(owner, repo, sha, &creator, req)
        {
            Ok(status) => {
                if proves_default_branch {
                    self.advance_green_ref(owner, repo, sha);
                }
                json_response(201, &commit_status_json(&status))
            }
            Err(err) => error_response(err),
        }
    }

    /// Move the repository's green ref onto `sha`, which a green
    /// `<repo>/main-health` status has just proved. Best-effort and never part
    /// of the status response: the status is the record, the ref is a
    /// projection of it that the next green verdict rebuilds.
    #[cfg(feature = "web")]
    fn advance_green_ref(&self, owner: &str, name: &str, sha: &str) {
        use crate::green_ref::Advance;

        let Some(manager) = self.repo_manager.as_ref() else {
            return;
        };
        let Ok(repository) = self.core.get_repository(owner, name) else {
            return;
        };
        let branch = repository.default_branch;
        match crate::green_ref::advance(manager, owner, name, &branch, sha) {
            Advance::Moved { from, to } => eprintln!(
                "green ref {owner}/{name} refs/green/{branch}: {} -> {to}",
                from.unwrap_or_else(|| "(absent)".to_string())
            ),
            Advance::Unchanged(_) => {}
            Advance::NotOnDefaultBranch => eprintln!(
                "green ref {owner}/{name}: {sha} is not on {branch}; refs/green/{branch} unchanged"
            ),
            Advance::Stale { green } => eprintln!(
                "green ref {owner}/{name}: {sha} is behind {green}; refs/green/{branch} unchanged"
            ),
            Advance::Unavailable(reason) => eprintln!(
                "green ref {owner}/{name}: refs/green/{branch} not moved to {sha}: {reason}"
            ),
        }
    }

    /// Without the git backend there is no ref to move.
    #[cfg(not(feature = "web"))]
    fn advance_green_ref(&self, _owner: &str, _name: &str, _sha: &str) {}
}

/// The commit status the gate posts for a run on a repository's own default
/// branch, rather than on a pull-request head or a merge-queue commit. Named
/// once here because the forge both moves the green ref from a green one (see
/// [`crate::green_ref`]) and reads it back for the `main_red` attention item.
pub(crate) fn main_health_context(repo_name: &str) -> String {
    format!("{repo_name}/main-health")
}

fn commit_status_state(state: &CommitStatusState) -> &'static str {
    match state {
        CommitStatusState::Error => "error",
        CommitStatusState::Failure => "failure",
        CommitStatusState::Pending => "pending",
        CommitStatusState::Success => "success",
    }
}

fn commit_status_json(status: &CommitStatus) -> Value {
    json!({
        "id": status.id,
        "state": commit_status_state(&status.state),
        "context": status.context,
        "base": status.base,
        "description": status.description,
        "target_url": status.target_url,
        "creator": owner_json(&status.creator),
        "created_at": status.created_at,
        "updated_at": status.updated_at,
    })
}

fn combined_status_json(reference: &str, combined: &CombinedStatus) -> Value {
    let statuses: Vec<Value> = combined.statuses.iter().map(commit_status_json).collect();
    json!({
        "state": commit_status_state(&combined.state),
        "sha": reference,
        "total_count": combined.total_count,
        "statuses": statuses,
    })
}
