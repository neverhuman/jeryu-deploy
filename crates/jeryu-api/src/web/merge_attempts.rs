//! The last merge attempt per pull request, and merge-identity grant gaps.
//!
//! A reviewer can approve a pull request that the merger then fails to land:
//! the merge identity has no grant on the repository (`403 repository access
//! denied`), or the merge queue refuses the PR (`queue_merge_commits`). Those
//! answers used to reach only the merger's own log, so `/runners` said
//! "approved" while the PR sat unmerged. The forge answers every merge request
//! itself, so it records the outcome here, keyed by PR:
//!
//! - native `POST /api/v1/repos/:id/pulls/:number/{merge,queue}`, observed by
//!   the [`observe`] layer outside the auth gate so its 403s are seen too;
//! - the GitHub edge `PUT /repos/:owner/:repo/pulls/:number/merge`, recorded by
//!   `surface::github_forward_request` through [`record_edge`].
//!
//! `/runners` joins the record onto a reviewer's last approval, and the PR page
//! reads it from `GET /api/v1/repos/:id/pulls/:number/merge-attempt`. The
//! store is in memory: a restarted forge learns the next attempt within one
//! merger pass. Grant gaps need no attempt at all: [`grant_gap`] asks whether
//! the merge identity (`JERYU_MERGE_IDENTITY`, default `merge-bot`) can
//! write to a repository, so the first reviewer beat naming one of its PRs
//! flags it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::*;

const MAX_RECORDS: usize = 4096;
const MAX_MESSAGE_CHARS: usize = 300;
/// Error bodies above this are not read back for their `code`/`message`.
const MAX_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MergeAttempt {
    /// `merged`, `queued`, or `refused`.
    pub result: String,
    pub status: u16,
    /// The forge error code of a refusal (`permission_denied`,
    /// `queue_merge_commits`, ...); absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Who asked to merge, when the request authenticated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub at: String,
}

impl MergeAttempt {
    pub(crate) fn refused(&self) -> bool {
        self.result == "refused"
    }

    /// One line naming what blocks the merge, e.g.
    /// `queue_merge_commits - rebase onto main`.
    pub(crate) fn reason(&self) -> Option<String> {
        if !self.refused() {
            return None;
        }
        let code = self.code.as_deref().unwrap_or("refused");
        let message = self.message.as_deref().unwrap_or_default();
        Some(if message.is_empty() {
            code.to_string()
        } else {
            format!("{code} - {message}")
        })
    }
}

/// The merge identity has no write grant on a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MergeGrantGap {
    pub repo: String,
    pub identity: String,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MergeAttemptStore {
    attempts: Arc<Mutex<BTreeMap<(String, u64), MergeAttempt>>>,
}

impl MergeAttemptStore {
    pub(crate) fn record(&self, repo: &str, number: u64, attempt: MergeAttempt) {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (repo.to_string(), number);
        if !attempts.contains_key(&key) && attempts.len() >= MAX_RECORDS {
            // Forget the oldest attempt rather than refusing to learn a new one.
            if let Some(oldest) = attempts
                .iter()
                .min_by(|a, b| a.1.at.cmp(&b.1.at))
                .map(|(key, _)| key.clone())
            {
                attempts.remove(&oldest);
            }
        }
        attempts.insert(key, attempt);
    }

    pub(crate) fn last(&self, repo: &str, number: u64) -> Option<MergeAttempt> {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(repo.to_string(), number))
            .cloned()
    }
}

/// The login whose merges land approved pull requests
/// (`JERYU_MERGE_IDENTITY`). An automation login belongs to one installation,
/// so this public source carries no default: unset, there is no merger to
/// describe or to warn about.
pub(crate) fn merge_identity() -> Option<&'static str> {
    super::auth::MERGE_IDENTITY.configured()
}

/// `Some` when the merge identity exists on this forge but cannot write to
/// `owner/name`. A forge without the identity has no merger to warn about.
pub(crate) fn grant_gap(state: &WebState, repo: &str) -> Option<MergeGrantGap> {
    let (owner, name) = repo.split_once('/')?;
    let identity = merge_identity()?;
    let account = state.core.get_account(identity).ok()?;
    if account.role == UserRole::Admin || state.core.user_can_write_repo(identity, owner, name) {
        return None;
    }
    Some(MergeGrantGap {
        repo: repo.to_string(),
        message: format!("{identity} has no write grant on {repo}; its merges answer 403"),
        identity: identity.to_owned(),
    })
}

fn clip(message: &str) -> String {
    let message = message.trim();
    match message.char_indices().nth(MAX_MESSAGE_CHARS) {
        Some((cut, _)) => format!("{}…", &message[..cut]),
        None => message.to_string(),
    }
}

/// The attempt a merge answer describes. `success_result` names a 2xx.
pub(crate) fn attempt_from_answer(
    status: u16,
    body: &str,
    actor: Option<&str>,
    success_result: &str,
) -> MergeAttempt {
    let ok = (200..300).contains(&status);
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let text = |key: &str| value[key].as_str().map(clip).filter(|s| !s.is_empty());
    MergeAttempt {
        result: if ok { success_result } else { "refused" }.to_string(),
        status,
        code: if ok {
            None
        } else {
            text("code").or_else(|| Some(format!("http_{status}")))
        },
        message: if ok { None } else { text("message") },
        actor: actor.map(str::to_string),
        at: chrono::Utc::now().to_rfc3339(),
    }
}

/// `(repo id, number, success result)` for a native merge or enqueue request.
fn native_merge_target(
    method: &axum::http::Method,
    path: &str,
) -> Option<(String, u64, &'static str)> {
    if method != axum::http::Method::POST {
        return None;
    }
    let rest = path.strip_prefix("/api/v1/repos/")?;
    let segments: Vec<&str> = rest.split('/').collect();
    let (action, number, id) = match segments.as_slice() {
        [id @ .., "pulls", number, action] if !id.is_empty() => (*action, *number, id.join("/")),
        _ => return None,
    };
    let result = match action {
        "merge" => "merged",
        "queue" => "queued",
        _ => return None,
    };
    Some((id, number.parse().ok()?, result))
}

/// Records the answer to every native merge and enqueue request. Sits outside
/// `auth::gate` so a merge identity with no grant is recorded too.
pub(super) async fn observe(
    State(state): State<Arc<WebState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> AxumResponse {
    let Some((id, number, success)) = native_merge_target(request.method(), request.uri().path())
    else {
        return next.run(request).await;
    };
    let Some(repo) = super::repositories::find_repo(&state, &id) else {
        return next.run(request).await;
    };
    let actor =
        super::auth::authenticate_headers(&state, request.headers()).map(|auth| auth.account.login);
    let response = next.run(request).await;
    let status = response.status().as_u16();
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => axum::body::Bytes::new(),
    };
    let attempt = attempt_from_answer(
        status,
        std::str::from_utf8(&bytes).unwrap_or_default(),
        actor.as_deref(),
        success,
    );
    state
        .merge_attempts
        .record(&format!("{}/{}", repo.owner, repo.name), number, attempt);
    AxumResponse::from_parts(parts, axum::body::Body::from(bytes))
}

/// Records a GitHub-edge merge answer (`PUT /repos/:owner/:repo/pulls/:n/merge`).
pub(super) fn record_edge(
    state: &WebState,
    write: bool,
    path: &str,
    actor: &str,
    status: u16,
    body: &str,
) {
    if !write {
        return;
    }
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let ["repos", owner, repo, "pulls", number, "merge"] = segments.as_slice() else {
        return;
    };
    let Ok(number) = number.parse::<u64>() else {
        return;
    };
    state.merge_attempts.record(
        &format!("{owner}/{repo}"),
        number,
        attempt_from_answer(status, body, Some(actor), "merged"),
    );
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MergeAttemptResponse {
    repo: String,
    number: u64,
    attempt: Option<MergeAttempt>,
    /// `code - message` of the last refusal, absent when the last attempt
    /// landed or none was made.
    blocked_reason: Option<String>,
    grant_gap: Option<MergeGrantGap>,
    /// Who approved the current head, so the page can say whose approval is
    /// waiting on the merge.
    approved_by: Vec<String>,
}

/// `GET /api/v1/repos/:id/pulls/:number/merge-attempt`
pub(super) async fn show(
    State(state): State<Arc<WebState>>,
    AxumPath((id, number)): AxumPath<(String, u64)>,
) -> AxumResponse {
    let Some(repo) = super::repositories::find_repo(&state, &id) else {
        return api_error(StatusCode::NOT_FOUND, "not_found", "repository not found");
    };
    let full = format!("{}/{}", repo.owner, repo.name);
    let attempt = state.merge_attempts.last(&full, number);
    let approved_by = state
        .core
        .get_pull_request(&repo.owner, &repo.name, number)
        .map(|pr| {
            super::merge_queue::approvers(&state, &pr)
                .into_iter()
                .map(|approver| approver.login)
                .collect()
        })
        .unwrap_or_default();
    Json(MergeAttemptResponse {
        approved_by,
        blocked_reason: attempt.as_ref().and_then(MergeAttempt::reason),
        grant_gap: grant_gap(&state, &full),
        repo: full,
        number,
        attempt,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_keep_the_forge_code_and_message() {
        let attempt = attempt_from_answer(
            409,
            r#"{"code":"queue_merge_commits","message":"the pull request contains merge commits; rebase it onto the base"}"#,
            Some("merge-bot"),
            "queued",
        );
        assert!(attempt.refused());
        assert_eq!(attempt.code.as_deref(), Some("queue_merge_commits"));
        assert_eq!(
            attempt.reason().as_deref(),
            Some(
                "queue_merge_commits - the pull request contains merge commits; rebase it onto the base"
            )
        );
        let denied = attempt_from_answer(403, "<html>", None, "merged");
        assert_eq!(denied.code.as_deref(), Some("http_403"));
        let ok = attempt_from_answer(201, "{}", None, "queued");
        assert_eq!((ok.result.as_str(), ok.reason()), ("queued", None));
    }

    #[test]
    fn only_native_merge_and_enqueue_posts_are_observed() {
        use axum::http::Method;
        assert_eq!(
            native_merge_target(&Method::POST, "/api/v1/repos/7/pulls/6/merge"),
            Some(("7".to_string(), 6, "merged"))
        );
        assert_eq!(
            native_merge_target(&Method::POST, "/api/v1/repos/veox-ai/app/pulls/6/queue"),
            Some(("veox-ai/app".to_string(), 6, "queued"))
        );
        assert_eq!(
            native_merge_target(&Method::DELETE, "/api/v1/repos/7/pulls/6/queue"),
            None
        );
        assert_eq!(
            native_merge_target(&Method::POST, "/api/v1/repos/7/pulls/6/approve"),
            None
        );
        assert_eq!(
            native_merge_target(&Method::POST, "/api/v1/repos/7/pulls/x/merge"),
            None
        );
    }

    #[test]
    fn the_latest_attempt_per_pr_wins() {
        let store = MergeAttemptStore::default();
        store.record(
            "o/r",
            6,
            attempt_from_answer(403, r#"{"code":"permission_denied"}"#, None, "merged"),
        );
        store.record(
            "o/r",
            6,
            attempt_from_answer(409, r#"{"code":"queue_merge_commits"}"#, None, "queued"),
        );
        assert_eq!(
            store.last("o/r", 6).unwrap().code.as_deref(),
            Some("queue_merge_commits")
        );
        assert!(store.last("o/r", 7).is_none());
    }
}
