//! Who may watch an agent run without being a global admin.
//!
//! Viewing (status, events, the SSE stream, and a read-only attach) is open to
//! the login that started the session and to writers of the session's
//! repository. Driving a run stays admin-only while runs are host PTYs: on the
//! host, input to an agent is input to a process running beside the forge.

use axum::http::Method;
use jeryu_core::{AccountSummary, UserRole};

use super::*;

/// The run id when `path` is one of the read-only per-run routes a non-admin
/// may be granted: `GET /api/v1/agent-runs/{id}` plus `/events`,
/// `/tty/stream`, and `/attach`.
pub(in crate::web) fn agent_run_viewer_id<'a>(method: &Method, path: &'a str) -> Option<&'a str> {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return None;
    }
    let rest = path.strip_prefix("/api/v1/agent-runs/")?;
    let (id, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let valid_id = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    (valid_id && matches!(tail, "" | "events" | "tty/stream" | "attach")).then_some(id)
}

impl AgentRunStore {
    /// The login that started this run, if any.
    pub(in crate::web) fn run_owner(&self, run_id: &str) -> Option<String> {
        let inner = self.inner.lock().expect("agent run store mutex");
        inner
            .runs
            .get(run_id)
            .and_then(|record| record.owner.clone())
    }

    /// Owner and repository of one run, for access decisions.
    fn run_scope(&self, run_id: &str) -> Option<(Option<String>, Option<String>)> {
        let inner = self.inner.lock().expect("agent run store mutex");
        let record = inner.runs.get(run_id)?;
        Some((record.owner.clone(), record.repo.clone()))
    }
}

/// True when `account` may watch `run_id`: an admin, the session's owner, or a
/// writer of the session's repository. A non-admin never learns whether an
/// unknown run exists.
pub(in crate::web) fn may_view_run(
    state: &WebState,
    account: &AccountSummary,
    run_id: &str,
) -> bool {
    // Admins keep their existing view, including typed not-found answers.
    if account.role == UserRole::Admin {
        return true;
    }
    let Some((owner, repo)) = state.agent_runs.run_scope(run_id) else {
        return false;
    };
    if owner.as_deref() == Some(account.login.as_str()) {
        return true;
    }
    repo.as_deref()
        .and_then(|repo| repo.split_once('/'))
        .is_some_and(|(repo_owner, name)| {
            state
                .core
                .user_can_write_repo(&account.login, repo_owner, name)
        })
}

/// Only an authenticated admin may drive a host-PTY run from an attach socket.
pub(in crate::web) fn may_control_host_run(account: Option<&AccountSummary>) -> bool {
    account.is_some_and(|account| {
        account.role == UserRole::Admin && account.login != crate::web::auth::ANONYMOUS_LOGIN
    })
}
