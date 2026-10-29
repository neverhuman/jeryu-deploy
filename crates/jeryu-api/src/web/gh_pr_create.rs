//! Exact gh 2.45.0 noninteractive draft creation adapter.
use super::surface::{authorize_github_repo_request, bind_authenticated_actor, github_response};
use super::{WebState, auth::HeaderAuth};
use crate::github::Method;
use axum::{
    Json,
    response::{IntoResponse, Response},
};
use jeryu_core::PullRequestState;
use serde_json::{Value, json};
pub(super) const REPOSITORY: &str = r#"
	fragment repo on Repository {
		id
		name
		owner { login }
		hasIssuesEnabled
		description
		hasWikiEnabled
		viewerPermission
		defaultBranchRef {
			name
		}
	}

	query RepositoryInfo($owner: String!, $name: String!) {
		repository(owner: $owner, name: $name) {
			...repo
			parent {
				...repo
			}
			mergeCommitAllowed
			rebaseMergeAllowed
			squashMergeAllowed
		}
	}"#;
pub(super) const BRANCH: &str = r#"
	query PullRequestForBranch($owner: String!, $repo: String!, $headRefName: String!, $states: [PullRequestState!]) {
		repository(owner: $owner, name: $repo) {
			pullRequests(headRefName: $headRefName, states: $states, first: 30, orderBy: { field: CREATED_AT, direction: DESC }) {
				nodes {url,id,number,state,baseRefName,headRefName,isCrossRepository,headRepositoryOwner{id,login,...on User{name}}}
			}
			defaultBranchRef { name }
		}
	}"#;
pub(super) const CREATE: &str = r#"
		mutation PullRequestCreate($input: CreatePullRequestInput!) {
			createPullRequest(input: $input) {
				pullRequest {
					id
					url
				}
			}
	}"#;

fn compact(value: &str) -> String {
    value.chars().filter(|c| !c.is_whitespace()).collect()
}
fn segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}
fn error(message: &str) -> Response {
    Json(json!({"errors":[{"message":message}],
        "documentation_url": crate::links::absolute("/docs/graphql"),
        "jeryu_repair_hint":{"repair_hint":"Use the exact supported gh 2.45.0 draft command or POST /repos/{owner}/{repo}/pulls."}})).into_response()
}

pub(super) fn handle(state: &WebState, body: &str, auth: &HeaderAuth) -> Option<Response> {
    let request: Value = serde_json::from_str(body).ok()?;
    let query = compact(request.get("query")?.as_str()?);
    let operation = [REPOSITORY, BRANCH, CREATE]
        .iter()
        .position(|q| compact(q) == query)?;
    let names = [
        "RepositoryInfo",
        "PullRequestForBranch",
        "PullRequestCreate",
    ];
    if request
        .get("operationName")
        .or_else(|| request.get("operation_name"))
        .is_some_and(|v| !v.is_null() && v.as_str() != Some(names[operation]))
    {
        return Some(error("unsupported operation name"));
    }
    Some(execute(state, &request["variables"], operation, auth))
}

fn execute(state: &WebState, vars: &Value, operation: usize, auth: &HeaderAuth) -> Response {
    let input = &vars["input"];
    let target = if operation == 2 {
        input["repositoryId"]
            .as_str()
            .and_then(|id| id.strip_prefix("jeryu-repository:"))
            .and_then(|id| id.split_once('/'))
    } else {
        vars["owner"]
            .as_str()
            .zip(vars[if operation == 0 { "name" } else { "repo" }].as_str())
    };
    let Some((owner, repo)) = target.filter(|(o, r)| segment(o) && segment(r)) else {
        return error("invalid repository target");
    };
    let path = format!("/repos/{owner}/{repo}/pulls");
    let method = if operation == 2 {
        Method::Post
    } else {
        Method::Get
    };
    if let Some(denied) = authorize_github_repo_request(state, method, &path, auth) {
        return denied;
    }
    let Ok(repository) = state.core.get_repository(owner, repo) else {
        return error("repository not found");
    };
    if operation == 0 {
        let permission =
            if authorize_github_repo_request(state, Method::Post, &path, auth).is_none() {
                "WRITE"
            } else {
                "READ"
            };
        return Json(json!({"data":{"repository":{
            "id":format!("jeryu-repository:{owner}/{repo}"), "name":repo,
            "owner":{"login":owner},"hasIssuesEnabled":true,"description":repository.description,
            "hasWikiEnabled":false,"viewerPermission":permission,
            "defaultBranchRef":{"name":repository.default_branch}, "parent":null,
            "mergeCommitAllowed":true,"rebaseMergeAllowed":false,"squashMergeAllowed":false
        }}}))
        .into_response();
    }
    let head = if operation == 1 {
        &vars["headRefName"]
    } else {
        &input["headRefName"]
    };
    let Some(head) = head.as_str().filter(|h| !h.is_empty() && !h.contains(':')) else {
        return error("head must name an existing branch in this repository");
    };
    if jeryu_gitd::refs::validate_ref_name(
        &state.repo_manager.config().git_bin,
        &format!("refs/heads/{head}"),
    )
    .is_err()
    {
        return error("head must be a valid branch name");
    }
    // Serialize this adapter's check-and-create, including concurrent gh retries.
    static CREATES: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = if operation == 2 {
        match CREATES.lock() {
            Ok(guard) => Some(guard),
            Err(_) => return error("draft creation unavailable; retry later"),
        }
    } else {
        None
    };
    let Ok(pulls) = state.core.list_pull_requests(owner, repo, None) else {
        return error("could not list pull requests");
    };
    let open: Vec<_> = pulls
        .iter()
        .filter(|p| {
            p.head.ref_name == head
                && !p.merged
                && !matches!(p.state, PullRequestState::Closed | PullRequestState::Merged)
        })
        .collect();
    if operation == 1 {
        if vars["states"] != json!(["OPEN"]) {
            return error("only the OPEN branch lookup is supported");
        }
        let nodes: Vec<_> = open.iter().take(30).map(|p| json!({
            "url":crate::links::absolute(&crate::links::native_pull_app_path(owner,repo,p.number)),
            "id":p.id.to_string(),"number":p.number,"state":"OPEN",
            "baseRefName":p.base.ref_name,"headRefName":p.head.ref_name,"isCrossRepository":false,
            "headRepositoryOwner":{"id":owner,"login":owner,"name":owner}
        })).collect();
        return Json(json!({"data":{"repository":{"pullRequests":{"nodes":nodes},
            "defaultBranchRef":{"name":repository.default_branch}}}}))
        .into_response();
    }
    if input["draft"] != true
        || input["baseRefName"] != "main"
        || input["maintainerCanModify"] != true
        || input.as_object().is_none_or(|v| v.len() != 7)
        || !input["title"].is_string()
        || !input["body"].is_string()
    {
        return error("only explicit draft creation with base main, title and body is supported");
    }
    if !open.is_empty() {
        return error("an open pull request already exists for this branch");
    }
    let refs = jeryu_gitd::refs::RefService::new((*state.repo_manager).clone());
    let resolved = state.repo_manager.resolve_parts(owner, repo).ok();
    let oid = resolved.as_ref().and_then(|r| {
        refs.resolve_commit(r, &format!("refs/heads/{head}"))
            .ok()
            .flatten()
    });
    if oid.is_none() {
        return error("head branch not found; push the branch before creating a draft");
    }
    let payload = bind_authenticated_actor(
        &json!({"title":input["title"],"body":input["body"],
        "head":head,"head_sha":oid,"base":"main","draft":true})
        .to_string(),
        &auth.account.login,
    );
    let response = state.github.handle(Method::Post, &path, &payload);
    if response.status != 201 {
        return github_response(response);
    }
    let created: Value = serde_json::from_str(&response.body).expect("REST PR JSON");
    Json(json!({"data":{"createPullRequest":{"pullRequest":{
        "id":created["id"].to_string(),"url":created["html_url"]
    }}}}))
    .into_response()
}
