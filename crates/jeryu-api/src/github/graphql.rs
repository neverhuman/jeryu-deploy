//! Guided GraphQL compatibility endpoint.
//!
//! Jeryu does not execute arbitrary GitHub GraphQL. The endpoint serves the
//! small read-only subset in [`SUPPORTED_OPERATIONS`] — discovery probes
//! and one captured GitHub CLI PR read — and answers anything else with a GitHub-shaped
//! error naming the REST route that serves the same intent.
//!
//! [`SUPPORTED_OPERATIONS`] is what the capability manifest, the REST document
//! and `docs/graphql.md` all describe, so no surface can claim more or less
//! GraphQL than this module answers.

use jeryu_core::{AccountSummary, PullRequestState, UserRole};
use serde::Deserialize;
use serde_json::{Value, json};

use super::GithubRouter;
use super::support::{docs_url, json_response, mcp_guidance_tools, parse_body};
use crate::routes::Response;

/// Request captured from GitHub CLI 2.45.0 for the bounded `gh pr view` command.
pub(crate) const GH_PR_VIEW_QUERY: &str = "query PullRequestByNumber($owner: String!, $repo: String!, $pr_number: Int!) { repository(owner: $owner, name: $repo) { pullRequest(number: $pr_number) {number,title,state,isDraft,headRefOid,url,id} } }";

/// Match the entire captured query, including its variable types and fields.
/// This is a command adapter, not a GraphQL evaluator.
pub(crate) fn gh_pr_view_target(body: &str) -> Option<(String, String, u64)> {
    let request: GraphqlRequest = serde_json::from_str(body).ok()?;
    if compact(&request.query) != compact(GH_PR_VIEW_QUERY)
        || request
            .operation_name
            .as_deref()
            .is_some_and(|name| name != "PullRequestByNumber")
    {
        return None;
    }
    let owner = request.variables.get("owner")?.as_str()?;
    let repo = request.variables.get("repo")?.as_str()?;
    let number = request.variables.get("pr_number")?.as_u64()?;
    let segment = |value: &str| {
        !value.is_empty()
            && value
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || b"-_.".contains(&ch))
            && value != "."
            && value != ".."
    };
    if !segment(owner) || !segment(repo) || number == 0 || number > i32::MAX as u64 {
        return None;
    }
    Some((owner.to_string(), repo.to_string(), number))
}

fn compact(query: &str) -> String {
    query.chars().filter(|ch| !ch.is_whitespace()).collect()
}

/// Every GraphQL operation this endpoint answers. Each entry is the query as a
/// caller writes it; [`is_typename_probe`], [`is_viewer_login_query`] and
/// [`is_repository_read_query`] are what actually match, and
/// `the_supported_operations_are_the_ones_served` holds the list to them.
pub(crate) const SUPPORTED_OPERATIONS: &[&str] = &[
    GH_PR_VIEW_QUERY,
    "query { __typename }",
    "query { viewer { login } }",
    "query { repository(owner: \"...\", name: \"...\") { name nameWithOwner isPrivate \
     defaultBranchRef { name } url } }",
];

/// Authentication guidance shared by all discovery surfaces.
pub(crate) const AUTH_NOTE: &str = "Authenticate with a web session or personal access token as Authorization: Bearer. Scoped agent tokens (jat_) may run only the captured gh 2.45.0 pr view query, subject to repository and task reach; other GraphQL returns 403 insufficient_scope. The viewer is the authenticated account; an unreadable repository reads as absent.";

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
    /// past authorization; the web edge passes the account or first authorizes
    /// the exact scoped PR read through the REST repository/task checks.
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

        if let Some((owner, repo, number)) = gh_pr_view_target(body) {
            if !self.account_can_read(account, &owner, &repo) {
                return json_response(200, &json!({"data": {"repository": null}}));
            }
            let repository = self.core().get_repository(&owner, &repo).ok().map(|_| {
                let pr = self.core().get_pull_request(&owner, &repo, number).ok();
                json!({"pullRequest": pr.map(|pr| json!({
                    "number": pr.number,
                    "title": pr.title,
                    "state": if pr.merged || pr.state == PullRequestState::Merged { "MERGED" }
                        else if pr.state == PullRequestState::Closed { "CLOSED" } else { "OPEN" },
                    "isDraft": pr.draft,
                    "headRefOid": pr.head.sha,
                    "url": super::support::web_url(&super::pulls::pull_request_web_path(&owner, &repo, number)),
                    "id": pr.id.to_string(),
                }))})
            });
            return json_response(200, &json!({"data": {"repository": repository}}));
        }
        // A modified command query must never fall through to a discovery matcher.
        if query.contains("pullRequest") || query.contains("PullRequest") {
            return unsupported_response(query, request.operation_name.as_deref());
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
            "message": "GraphQL query is outside the supported subset; use the listed REST route",
            "documentation_url": graphql_docs_url(),
            "errors": [{
                "type": "GUIDED_ROUTE_REQUIRED",
                "message": "Use the listed Jeryu REST route for this workflow."
            }],
            "jeryu_repair_hint": {
                "purpose": "route unsupported GitHub GraphQL request",
                "reason": "Jeryu serves a small read-only GraphQL subset and the GitHub-compatible REST routes; it does not execute arbitrary GraphQL.",
                "common_fixes": [
                    "Use the GitHub-compatible REST route for repository, PR, issue, check, release, or hook workflows.",
                    "Send one of the supported discovery operations or the exact captured gh 2.45.0 PR read listed here.",
                    "Run the jeryu-api conformance tests before widening the GraphQL subset."
                ],
                "docs_url": graphql_docs_url(),
                "repair_hint": "Send the listed REST route instead; add a narrow conformance test before supporting another GraphQL read query."
            },
            "graphql_supported_operations": SUPPORTED_OPERATIONS,
            "graphql_auth": AUTH_NOTE,
            "jeryu_mcp_tools": mcp_guidance_tools(),
            "jeryu_api_routes": [
                "GET /repos",
                "GET /repos/{owner}/{repo}",
                "PATCH /repos/{owner}/{repo} (archived: bool or name: string; admin only)",
                "POST /repos/{owner}/{repo}/transfer (new_owner, optional new_name; admin only)",
                "GET /repos/{owner}/{repo}/pulls?state=open|closed|all&sort=created|updated|popularity|long-running&direction=asc|desc&head=owner:branch&base=branch (default created/desc; an unaccepted value is a 422)",
                "PATCH /repos/{owner}/{repo}/pulls/{number} (draft: bool, title, body, state; author or admin. The named Jeryu routes are POST /api/v1/repos/{id}/pulls/{number}/ready and /draft)",
                "GET /repos/{owner}/{repo}/issues?state=open|closed|all&sort=created|updated|comments&direction=asc|desc (default created/desc; an unaccepted value is a 422)",
                "GET /repos/{owner}/{repo}/commits?sha=&direction=asc|desc (newest first by default; an unaccepted value is a 422)",
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

    /// Every operation the published subset names is one this module matches,
    /// and the matchers answer nothing the list leaves out: the manifest, the
    /// REST document and the docs page all quote this list.
    #[test]
    fn the_supported_operations_are_the_ones_served() {
        assert_eq!(SUPPORTED_OPERATIONS.len(), 4);
        let matched = |query: &str| {
            is_typename_probe(query)
                || is_viewer_login_query(query)
                || is_repository_read_query(query)
                || gh_pr_view_target(&json!({"query": query, "variables": {"owner": "alice", "repo": "demo", "pr_number": 1}}).to_string()).is_some()
        };
        for operation in SUPPORTED_OPERATIONS {
            assert!(matched(operation), "the subset lists {operation}, unserved");
        }
        for unsupported in [
            "mutation { addStar(input: { starrableId: \"R_1\" }) { starrable { id } } }",
            "query { organization(login: \"acme\") { name } }",
            "query { repository(owner: \"a\", name: \"b\") { pullRequests(first: 10) { nodes { id } } } }",
            "query { search(query: \"repo:a/b\", type: ISSUE, first: 5) { issueCount } }",
        ] {
            assert!(
                !matched(unsupported),
                "{unsupported} is answered but unlisted"
            );
        }
    }

    #[test]
    fn quoted_arg_extracts_inline_values() {
        let query = r#"query { repository(owner: "alice", name: "jeryu") { name } }"#;
        assert_eq!(quoted_arg(query, "owner").as_deref(), Some("alice"));
        assert_eq!(quoted_arg(query, "name").as_deref(), Some("jeryu"));
    }
}
