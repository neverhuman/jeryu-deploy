//! Guided GraphQL compatibility endpoint.
//!
//! Jeryu does not execute arbitrary GitHub GraphQL. The endpoint supports a
//! tiny read-only subset used by common discovery probes and otherwise returns
//! a GitHub-shaped error with typed Jeryu repair routes.

use jeryu_core::{AccountSummary, UserRole};
use serde::Deserialize;
use serde_json::{Value, json};

use super::GithubRouter;
use super::support::{MCP_GUIDANCE_TOOLS, docs_url, json_response, parse_body};
use crate::routes::Response;

#[derive(Debug, Deserialize)]
struct GraphqlRequest {
    query: String,
    #[serde(default)]
    variables: Value,
    #[serde(default, rename = "operationName", alias = "operation_name")]
    operation_name: Option<String>,
}

impl GithubRouter {
    /// Account-scoped GraphQL for the local web edge: the caller only sees
    /// repositories it may read.
    pub(crate) fn graphql_for_account(&self, body: &str, account: &AccountSummary) -> Response {
        self.graphql_scoped(body, Some(account))
    }

    pub(super) fn graphql(&self, body: &str) -> Response {
        self.graphql_scoped(body, None)
    }

    /// `account` is `None` for the trusted in-process edge, which is already
    /// past authorization; the web edge always passes the caller.
    fn graphql_scoped(&self, body: &str, account: Option<&AccountSummary>) -> Response {
        let request = match parse_body::<GraphqlRequest>(body) {
            Ok(request) => request,
            Err(response) => return response,
        };
        let query = request.query.trim();
        if query.is_empty() {
            return json_response(
                422,
                &json!({
                    "message": "Validation Failed",
                    "errors": [{ "field": "query", "code": "missing" }],
                    "documentation_url": graphql_docs_url(),
                }),
            );
        }

        if is_typename_probe(query) {
            return json_response(200, &json!({ "data": { "__typename": "Query" } }));
        }
        if is_viewer_login_query(query) {
            return json_response(
                200,
                &json!({ "data": { "viewer": self.viewer_json(account) } }),
            );
        }
        if is_repository_read_query(query)
            && let Some((owner, name)) = repository_args(query, &request.variables)
        {
            let repo = self.core().get_repository(&owner, &name).ok();
            // A repository the caller cannot read reads as absent, which is
            // GraphQL's own shape for "no such node" and leaks nothing about
            // whether it exists.
            let repo = repo.filter(|repo| self.account_can_read(account, &repo.owner, &repo.name));
            return json_response(
                200,
                &json!({
                    "data": {
                        "repository": repo.map(|repo| json!({
                            "name": repo.name,
                            "nameWithOwner": repo.full_name,
                            "isPrivate": repo.private,
                            "defaultBranchRef": { "name": repo.default_branch },
                            "url": format!("/repos/{owner}/{name}")
                        }))
                    }
                }),
            );
        }

        unsupported_response(query, request.operation_name.as_deref())
    }

    fn account_can_read(&self, account: Option<&AccountSummary>, owner: &str, name: &str) -> bool {
        match account {
            None => true,
            Some(account) => {
                account.role == UserRole::Admin
                    || self.core().user_can_read_repo(&account.login, owner, name)
            }
        }
    }
}

fn is_typename_probe(query: &str) -> bool {
    let normalized = normalized_query(query);
    normalized == "{__typename}"
        || normalized.contains("{__typename}")
        || normalized.contains("query{__typename}")
}

fn is_viewer_login_query(query: &str) -> bool {
    let normalized = normalized_query(query);
    normalized.contains("viewer{login")
        || normalized.contains("viewer{idlogin")
        || normalized.contains("viewer{loginname")
}

fn is_repository_read_query(query: &str) -> bool {
    let normalized = normalized_query(query);
    normalized.contains("repository(")
        && (normalized.contains("namewithowner") || normalized.contains("defaultbranchref"))
}

fn repository_args(query: &str, variables: &Value) -> Option<(String, String)> {
    let owner = variables
        .get("owner")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| quoted_arg(query, "owner"))?;
    let name = variables
        .get("name")
        .or_else(|| variables.get("repo"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| quoted_arg(query, "name"))?;
    Some((owner, name))
}

fn quoted_arg(query: &str, name: &str) -> Option<String> {
    let needle = format!("{name}:");
    let start = query.find(&needle)? + needle.len();
    let rest = query[start..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value = &rest[quote.len_utf8()..];
    let end = value.find(quote)?;
    Some(value[..end].to_string())
}

fn normalized_query(query: &str) -> String {
    query
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn unsupported_response(query: &str, operation_name: Option<&str>) -> Response {
    json_response(
        501,
        &json!({
            "message": "GraphQL query requires a guided Jeryu route",
            "documentation_url": graphql_docs_url(),
            "errors": [{
                "type": "GUIDED_ROUTE_REQUIRED",
                "message": "Use the listed Jeryu REST or MCP route for this workflow."
            }],
            "jeryu_repair_hint": {
                "purpose": "route unsupported GitHub GraphQL request",
                "reason": "Jeryu intentionally supports a guided GitHub-compatible subset and typed local APIs instead of broad GraphQL execution.",
                "common_fixes": [
                    "Use the GitHub-compatible REST route for repository, PR, issue, check, release, or hook workflows.",
                    "Use the matching Jeryu MCP tool id when an agent needs typed repair context.",
                    "Run the jeryu-api conformance tests before widening the GraphQL subset."
                ],
                "docs_url": graphql_docs_url(),
                "repair_hint": "Prefer the listed Jeryu MCP/API alternatives; add a narrow conformance test before supporting another GraphQL read query."
            },
            "jeryu_mcp_tools": MCP_GUIDANCE_TOOLS,
            "jeryu_api_routes": [
                "GET /repos",
                "GET /repos/{owner}/{repo}",
                "PATCH /repos/{owner}/{repo} (archived: bool or name: string; admin only)",
                "POST /repos/{owner}/{repo}/transfer (new_owner, optional new_name; admin only)",
                "GET /repos/{owner}/{repo}/pulls",
                "PATCH /repos/{owner}/{repo}/pulls/{number} (draft: bool, title, body, state; author or admin. The named Jeryu routes are POST /api/v1/repos/{id}/pulls/{number}/ready and /draft)",
                "GET /repos/{owner}/{repo}/issues",
                "GET /repos/{owner}/{repo}/commits",
                "GET /repos/{owner}/{repo}/commits/{ref}/status",
                "GET /repos/{owner}/{repo}/commits/{ref}/check-runs"
            ],
            "operation_name": operation_name,
            "query_fingerprint": query_fingerprint(query),
        }),
    )
}

fn graphql_docs_url() -> String {
    format!("{}/graphql", docs_url().trim_end_matches("/rest"))
}

fn query_fingerprint(query: &str) -> String {
    format!("graphql:{}:{}", query.len(), normalized_query(query).len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_arg_extracts_inline_values() {
        let query = r#"query { repository(owner: "alice", name: "jeryu") { name } }"#;
        assert_eq!(quoted_arg(query, "owner").as_deref(), Some("alice"));
        assert_eq!(quoted_arg(query, "name").as_deref(), Some("jeryu"));
    }
}
