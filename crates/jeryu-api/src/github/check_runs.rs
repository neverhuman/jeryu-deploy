//! Check-run routes (`/repos/{owner}/{repo}/check-runs`) and their
//! GitHub-shaped renderers.

use jeryu_core::{
    CheckConclusion, CheckRun, CheckRunStatus, CreateCheckRunRequest, check_conclusion_wire_value,
};
use serde_json::{Value, json};

use crate::routes::Response;

use super::GithubRouter;
use super::support::{Pagination, docs_url, error_response, json_response, paginate, parse_body};

impl GithubRouter {
    pub(super) fn list_check_runs(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        page: Pagination,
    ) -> Response {
        match self.core.list_check_runs(owner, repo, None) {
            Ok(list) => {
                let runs: Vec<Value> = list.check_runs.iter().map(check_run_json).collect();
                paginate(
                    path,
                    page,
                    &runs,
                    |slice, total| json!({ "total_count": total, "check_runs": slice }),
                )
            }
            Err(err) => error_response(err),
        }
    }

    pub(super) fn list_check_runs_for_reference(
        &self,
        owner: &str,
        repo: &str,
        reference: &str,
        path: &str,
        page: Pagination,
    ) -> Response {
        let head_sha = match self.resolve_check_run_reference(owner, repo, reference) {
            Ok(head_sha) => head_sha,
            Err(response) => return response,
        };
        match self.core.list_check_runs(owner, repo, Some(&head_sha)) {
            Ok(list) => {
                let runs: Vec<Value> = list.check_runs.iter().map(check_run_json).collect();
                paginate(
                    path,
                    page,
                    &runs,
                    |slice, total| json!({ "total_count": total, "check_runs": slice }),
                )
            }
            Err(err) => error_response(err),
        }
    }

    fn resolve_check_run_reference(
        &self,
        owner: &str,
        repo: &str,
        reference: &str,
    ) -> Result<String, Response> {
        #[cfg(feature = "web")]
        if let Some(manager) = &self.repo_manager {
            use jeryu_gitd::refs::RefService;

            let resolved = manager.resolve_parts(owner, repo).map_err(|err| {
                json_response(
                    422,
                    &json!({
                        "message": format!("repository does not resolve for check-run lookup: {err}"),
                        "documentation_url": docs_url(),
                    }),
                )
            })?;
            let refs = RefService::new((**manager).clone());
            return match refs.resolve_commit(&resolved, reference) {
                Ok(Some(head_sha)) => Ok(head_sha),
                Ok(None) => Err(json_response(
                    422,
                    &json!({
                        "message": format!(
                            "commit reference {reference:?} is unknown or ambiguous"
                        ),
                        "documentation_url": docs_url(),
                    }),
                )),
                Err(err) => Err(json_response(
                    500,
                    &json!({
                        "message": err.to_string(),
                        "documentation_url": docs_url(),
                    }),
                )),
            };
        }

        let _ = (owner, repo);
        Ok(reference.to_owned())
    }

    pub(super) fn create_check_run(&self, owner: &str, repo: &str, body: &str) -> Response {
        let req: CreateCheckRunRequest = match parse_body(body) {
            Ok(value) => value,
            Err(response) => return response,
        };
        if let Some(problem) = req.details_url.as_deref().and_then(details_url_problem) {
            return json_response(
                422,
                &json!({
                    "message": format!("details_url {problem}"),
                    "documentation_url": docs_url(),
                }),
            );
        }
        match self.core.create_check_run(owner, repo, req) {
            Ok(run) => json_response(201, &check_run_json(&run)),
            Err(err) => error_response(err),
        }
    }
}

/// Why `url` cannot be a check run's `details_url`, if it cannot. The link is
/// what a reader clicks from the PR page to learn why a check failed, so it
/// must be a human web page served over https, never a raw `/api/` JSON route.
pub(crate) fn details_url_problem(url: &str) -> Option<&'static str> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Some("must be an https:// web page");
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let path = rest.find('/').map_or("", |start| &rest[start..]);
    if path == "/api" || path.starts_with("/api/") {
        return Some("must be a web page, not an /api/ route");
    }
    None
}

/// The https web page at `path` on the forge `base` (`http://host` or
/// `https://host`) points at: check links are always served over https.
pub(crate) fn web_page_url(base: &str, path: &str) -> String {
    let host = base
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    format!("https://{host}/{}", path.trim_start_matches('/'))
}

fn check_run_status(status: &CheckRunStatus) -> &'static str {
    match status {
        CheckRunStatus::Queued => "queued",
        CheckRunStatus::InProgress => "in_progress",
        CheckRunStatus::Completed => "completed",
    }
}

fn check_conclusion(conclusion: &CheckConclusion) -> &'static str {
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

fn check_run_json(run: &CheckRun) -> Value {
    json!({
        "id": run.id,
        "name": run.name,
        "head_sha": run.head_sha,
        "status": check_run_status(&run.status),
        "conclusion": run.conclusion.as_ref().map(check_conclusion),
        "details_url": run.details_url,
        "output": run.output.as_ref().map(|output| json!({
            "title": output.title,
            "summary": output.summary,
            "text": output.text,
        })),
        "started_at": run.started_at,
        "completed_at": run.completed_at,
    })
}

#[cfg(test)]
mod tests {
    use super::{details_url_problem, web_page_url};

    #[test]
    fn details_url_rejects_api_routes_and_plain_http() {
        assert!(
            details_url_problem("https://forge.test/api/v1/repos/1f2e/jankurai-scores?sha=abc")
                .is_some()
        );
        assert!(details_url_problem("https://forge.test/api").is_some());
        assert!(details_url_problem("http://forge.test/quality-gate").is_some());
        assert!(details_url_problem("/quality-gate").is_some());
        assert_eq!(
            details_url_problem("https://forge.test/quality-gate/heads/a/b/abc"),
            None
        );
        assert_eq!(details_url_problem("https://forge.test/apis/docs"), None);
        assert_eq!(details_url_problem("https://forge.test?x=/api/"), None);
    }

    #[test]
    fn web_page_url_is_always_https() {
        assert_eq!(
            web_page_url("http://forge.test/", "/quality-gate"),
            "https://forge.test/quality-gate"
        );
        assert_eq!(
            web_page_url("https://forge.test", "quality-gate"),
            "https://forge.test/quality-gate"
        );
    }
}
