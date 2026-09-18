//! Deployment routes (`/repos/{owner}/{repo}/deployments`, their `statuses`,
//! and `environments`) and their GitHub-shaped renderers.
//!
//! The forge core stores deployments append-only; these routes only create and
//! read. `environments` follows GitHub's envelope and adds what the release
//! views need per environment: the `latest` attempt, the `current` live
//! deployment and the `previous` one it replaced (the rollback target).

use jeryu_core::{
    CreateDeploymentRequest, CreateDeploymentStatusRequest, Deployment, DeploymentFilter,
    DeploymentStatus, DeploymentWithStatus, EnvironmentSummary,
};
use serde_json::{Value, json};

use crate::routes::Response;

use super::GithubRouter;
use super::support::{
    Pagination, actor, error_response, json_response, owner_json, paginate, parse_body,
};

impl GithubRouter {
    pub(super) fn list_deployments(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        page: Pagination,
        query: &str,
    ) -> Response {
        match self
            .core
            .list_deployments(owner, repo, &deployment_filter(query))
        {
            Ok(deployments) => paginate(path, page, &deployments, |slice, _total| {
                Value::Array(slice.iter().map(deployment_json).collect())
            }),
            Err(err) => error_response(err),
        }
    }

    pub(super) fn create_deployment(&self, owner: &str, repo: &str, body: &str) -> Response {
        let req: CreateDeploymentRequest = match parse_body(body) {
            Ok(value) => value,
            Err(response) => return response,
        };
        match self.core.create_deployment(owner, repo, &actor(body), req) {
            Ok(deployment) => json_response(201, &deployment_json(&deployment)),
            Err(err) => error_response(err),
        }
    }

    pub(super) fn get_deployment(&self, owner: &str, repo: &str, id: &str) -> Response {
        let Some(id) = parse_id(id) else {
            return deployment_not_found(id);
        };
        match self.core.get_deployment(owner, repo, id) {
            Ok(deployment) => json_response(200, &deployment_json(&deployment)),
            Err(err) => error_response(err),
        }
    }

    pub(super) fn list_deployment_statuses(
        &self,
        owner: &str,
        repo: &str,
        id: &str,
        path: &str,
        page: Pagination,
    ) -> Response {
        let Some(id) = parse_id(id) else {
            return deployment_not_found(id);
        };
        let deployment = match self.core.get_deployment(owner, repo, id) {
            Ok(deployment) => deployment,
            Err(err) => return error_response(err),
        };
        match self.core.list_deployment_statuses(owner, repo, id) {
            Ok(statuses) => paginate(path, page, &statuses, |slice, _total| {
                Value::Array(
                    slice
                        .iter()
                        .map(|status| status_json(&deployment, status))
                        .collect(),
                )
            }),
            Err(err) => error_response(err),
        }
    }

    pub(super) fn create_deployment_status(
        &self,
        owner: &str,
        repo: &str,
        id: &str,
        body: &str,
    ) -> Response {
        let Some(id) = parse_id(id) else {
            return deployment_not_found(id);
        };
        let req: CreateDeploymentStatusRequest = match parse_body(body) {
            Ok(value) => value,
            Err(response) => return response,
        };
        let deployment = match self.core.get_deployment(owner, repo, id) {
            Ok(deployment) => deployment,
            Err(err) => return error_response(err),
        };
        match self
            .core
            .create_deployment_status(owner, repo, id, &actor(body), req)
        {
            Ok(status) => json_response(201, &status_json(&deployment, &status)),
            Err(err) => error_response(err),
        }
    }

    pub(super) fn list_environments(&self, owner: &str, repo: &str) -> Response {
        match self.core.deployment_environments(owner, repo) {
            Ok(environments) => json_response(
                200,
                &json!({
                    "total_count": environments.len(),
                    "environments": environments.iter().map(environment_json).collect::<Vec<_>>(),
                }),
            ),
            Err(err) => error_response(err),
        }
    }
}

/// `?environment=&sha=&ref=` as on GitHub; unknown keys are ignored and the
/// first occurrence of a key wins.
fn deployment_filter(query: &str) -> DeploymentFilter {
    let mut filter = DeploymentFilter::default();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        if value.is_empty() {
            continue;
        }
        let slot = match key {
            "environment" => &mut filter.environment,
            "sha" => &mut filter.sha,
            "ref" => &mut filter.ref_name,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(value);
        }
    }
    filter
}

/// Decodes `%XX` escapes and `+` (refs may carry `/`, environment names spaces).
/// A malformed escape is kept literally rather than rejected.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| value.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[i], escaped) {
            (_, Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b'+', None) => {
                out.push(b' ');
                i += 1;
            }
            (byte, None) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_id(id: &str) -> Option<u64> {
    id.parse::<u64>().ok().filter(|id| *id > 0)
}

fn deployment_not_found(id: &str) -> Response {
    json_response(
        404,
        &json!({
            "message": format!("deployment {id} not found"),
            "documentation_url": "/docs/rest",
        }),
    )
}

fn deployment_url(deployment: &Deployment) -> String {
    format!(
        "/repos/{}/{}/deployments/{}",
        deployment.owner, deployment.repo, deployment.id
    )
}

fn deployment_json(deployment: &Deployment) -> Value {
    let url = deployment_url(deployment);
    json!({
        "id": deployment.id,
        "url": url,
        "sha": deployment.sha,
        "ref": deployment.ref_name,
        "task": deployment.task,
        "payload": deployment.payload,
        "original_environment": deployment.environment,
        "environment": deployment.environment,
        "description": deployment.description,
        "creator": owner_json(&deployment.creator),
        "created_at": deployment.created_at,
        "updated_at": deployment.created_at,
        "statuses_url": format!("{url}/statuses"),
        "repository_url": format!("/repos/{}/{}", deployment.owner, deployment.repo),
        "transient_environment": deployment.transient_environment,
        "production_environment": deployment.production_environment,
    })
}

fn status_json(deployment: &Deployment, status: &DeploymentStatus) -> Value {
    let deployment_url = deployment_url(deployment);
    json!({
        "id": status.id,
        "url": format!("{deployment_url}/statuses/{}", status.id),
        "state": status.state.as_str(),
        "creator": owner_json(&status.creator),
        "description": status.description,
        "environment": deployment.environment,
        "target_url": status.log_url,
        "log_url": status.log_url,
        "environment_url": status.environment_url,
        "created_at": status.created_at,
        "updated_at": status.created_at,
        "deployment_url": deployment_url,
        "repository_url": format!("/repos/{}/{}", deployment.owner, deployment.repo),
    })
}

fn with_status_json(entry: &DeploymentWithStatus) -> Value {
    json!({
        "deployment": deployment_json(&entry.deployment),
        "status": entry
            .status
            .as_ref()
            .map(|status| status_json(&entry.deployment, status)),
        "succeeded": entry.succeeded,
    })
}

fn environment_json(environment: &EnvironmentSummary) -> Value {
    json!({
        "name": environment.environment,
        "latest": environment.latest.as_ref().map(with_status_json),
        "current": environment.current.as_ref().map(with_status_json),
        "previous": environment.previous.as_ref().map(with_status_json),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_reads_github_keys_decodes_and_keeps_the_first() {
        let filter =
            deployment_filter("environment=prod%2Feu&sha=abc&ref=release+1&environment=dev&x=y");
        assert_eq!(filter.environment.as_deref(), Some("prod/eu"));
        assert_eq!(filter.sha.as_deref(), Some("abc"));
        assert_eq!(filter.ref_name.as_deref(), Some("release 1"));
        assert_eq!(
            deployment_filter("environment=&sha"),
            DeploymentFilter::default()
        );
    }

    #[test]
    fn malformed_escapes_are_kept_literally() {
        assert_eq!(percent_decode("a%zzb%2"), "a%zzb%2");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%41%42"), "AB");
    }

    #[test]
    fn ids_must_be_positive_integers() {
        assert_eq!(parse_id("7"), Some(7));
        for bad in ["0", "-1", "abc", "", "18446744073709551616"] {
            assert_eq!(parse_id(bad), None, "{bad:?}");
        }
    }
}
