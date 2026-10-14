//! Agent presence: which coding agents are working on which repositories
//! right now (`docs/agent-presence.md`, ADR 0006).
//!
//! - `POST /api/v1/agent-presence` registers or beats one session. Every beat
//!   carries the whole record and replaces the stored one.
//! - `DELETE /api/v1/agent-presence/:session_id` ends it.
//! - `GET /api/v1/repos/:id/agent-presence` lists every agent on one
//!   repository: reported sessions naming it or its family, jeryu's own
//!   running agent runs, gate runners and reviewers on it, and working queue
//!   slots of its family.
//! - `GET /api/v1/agent-presence[?family=]` lists reported sessions for a
//!   family page or an overview.
//!
//! Who may report is decided here, not by the request gate: a global admin
//! or a `JERYU_PRESENCE_REPORTERS` login reports any scope; any other login
//! reports only repositories it can write, and a family only when it can
//! write every hosted repository of that family. A session belongs to the
//! login that registered it: nobody else but an admin beats or ends it.
//! Reads need a login, never anonymous; the repository read is gated on the
//! repository like `/agent-runs`, and `host` is shown to admins only.

mod sources;
mod store;
mod types;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::Utc;
use jeryu_core::{AccountSummary, Repository, UserRole};
use serde_json::json;

use super::WebState;
use super::repositories::{effective_family, find_repo};
use super::strict_query::StrictQuery;
use super::workcells_support::{TypedError, typed_error};
use sources::{external_item, mark_collisions, repo_items, sort};
pub(crate) use store::PresenceStore;
use store::effective_state;
#[cfg(test)]
pub(crate) use store::{LINGER_MS, PresenceRow};
#[cfg(test)]
pub(crate) use types::PresenceState;
use types::{
    BeatAccepted, BeatError, CheckedBeat, OverviewQuery, PresenceBeat, PresenceList, RepoQuery,
    valid_session_id,
};
pub(crate) use types::{OverviewQuery as PresenceOverviewQuery, RepoQuery as PresenceRepoQuery};

const REPORTERS_ENV: &str = "JERYU_PRESENCE_REPORTERS";
const DOCS: &str = "docs/agent-presence.md";
const PURPOSE: &str = "report or list the coding agents working on a repository";

/// Agent presence state carried on `WebState`.
#[derive(Clone)]
pub(crate) struct AgentPresenceState {
    pub(crate) store: PresenceStore,
    /// Logins that may report any scope without being forge admins
    /// (`JERYU_PRESENCE_REPORTERS`, comma-separated). No default: a reporter
    /// login is one installation's identity.
    pub(crate) reporters: Arc<Vec<String>>,
}

impl AgentPresenceState {
    pub(crate) fn open(path: &Path) -> Self {
        let configured = std::env::var(REPORTERS_ENV).unwrap_or_default();
        Self {
            store: PresenceStore::open(path).expect("open agent presence store"),
            reporters: Arc::new(super::shift::reporter_list(&configured)),
        }
    }

    /// A named reporter or any forge admin.
    pub(crate) fn may_report_any(&self, login: &str, admin: bool) -> bool {
        admin || self.reporters.iter().any(|reporter| reporter == login)
    }

    #[cfg(test)]
    pub(crate) fn with_reporters(mut self, logins: &[&str]) -> Self {
        self.reporters = Arc::new(logins.iter().map(|login| (*login).to_string()).collect());
        self
    }
}

fn presence_error(status: StatusCode, code: &str, reason: &str, hint: &str) -> AxumResponse {
    typed_error(TypedError {
        status,
        code,
        purpose: PURPOSE,
        reason,
        common_fixes: &[
            "check the body against docs/agent-presence.md",
            "beat a session from the login that registered it",
        ],
        docs_url: DOCS,
        repair_hint: hint,
        message: reason,
    })
}

fn invalid(reason: &str) -> AxumResponse {
    presence_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "presence_invalid_request",
        reason,
        "fix the field named in the message and send the whole record again",
    )
}

fn store_failed(reason: &str) -> AxumResponse {
    presence_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "presence_store_failed",
        reason,
        "check <data_dir>/shift.sqlite",
    )
}

fn write_failed(error: store::StoreError, session_id: &str) -> AxumResponse {
    match error {
        store::StoreError::Owned => owned(session_id),
        store::StoreError::Limit => presence_error(
            StatusCode::TOO_MANY_REQUESTS,
            "presence_limit",
            "at most 64 listed sessions per reporter and 1024 for the forge",
            "reuse an existing session id, or end sessions you no longer run; an ended or expired session stops counting 10 minutes later",
        ),
        store::StoreError::Full => presence_error(
            StatusCode::TOO_MANY_REQUESTS,
            "presence_limit",
            "the forge stores at most 16384 sessions",
            "reuse an existing session id; stored sessions are pruned 14 days after their last beat",
        ),
        store::StoreError::WriteBudget => presence_error(
            StatusCode::TOO_MANY_REQUESTS,
            "presence_write_budget",
            "at most 120 presence writes per login per minute",
            "reduce beat frequency and retry after one minute",
        ),
        store::StoreError::Database(reason) => store_failed(&reason),
    }
}

fn owned(session_id: &str) -> AxumResponse {
    presence_error(
        StatusCode::CONFLICT,
        "presence_session_owned",
        &format!("session {session_id:?} was registered by another login"),
        "send a session id of your own; only its reporter or an admin may beat or end it",
    )
}

fn is_admin(account: &AccountSummary) -> bool {
    account.role == UserRole::Admin
}

fn can_read(state: &WebState, account: &AccountSummary, repo: &Repository) -> bool {
    is_admin(account)
        || !repo.private
        || state
            .core
            .user_can_read_repo(&account.login, &repo.owner, &repo.name)
}

fn visible_family(
    state: &WebState,
    account: &AccountSummary,
    raw: &str,
) -> Result<String, Box<AxumResponse>> {
    let key = super::family::canonical(raw);
    if state.core.list_repositories(None).iter().any(|repo| {
        (can_read(state, account, repo)
            || state
                .agent_presence
                .may_report_any(&account.login, is_admin(account)))
            && effective_family(state, repo).as_deref() == Some(key.as_str())
    }) {
        Ok(key)
    } else {
        Err(Box::new(presence_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "family_unknown",
            "no such visible family",
            "name a family of a repository you can read",
        )))
    }
}

fn can_write(state: &WebState, account: &AccountSummary, repo: &Repository) -> bool {
    is_admin(account)
        || state
            .core
            .user_can_write_repo(&account.login, &repo.owner, &repo.name)
}

/// The beat's repositories as canonical `owner/name`. A repository the caller
/// may not read is reported as unknown, so the answer never tells an
/// outsider that a private repository exists.
fn resolve_repos(
    state: &WebState,
    account: &AccountSummary,
    names: &[String],
) -> Result<Vec<Repository>, Box<AxumResponse>> {
    let mut found = Vec::new();
    let mut unknown = Vec::new();
    for name in names {
        match find_repo(state, name).filter(|repo| can_read(state, account, repo)) {
            Some(repo) => {
                if !found.iter().any(|seen: &Repository| seen.id == repo.id) {
                    found.push(repo);
                }
            }
            None => unknown.push(name.clone()),
        }
    }
    if !unknown.is_empty() {
        return Err(Box::new(presence_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "presence_repo_unknown",
            &format!("this forge hosts no repository {unknown:?} you can see"),
            "send repositories as owner/name of hosted repositories, or drop them",
        )));
    }
    Ok(found)
}

/// What an ordinary login may not claim: repositories it cannot write, and
/// families with a hosted repository it cannot write (or none at all).
fn forbidden_scope(
    state: &WebState,
    account: &AccountSummary,
    repos: &[Repository],
    families: &[String],
) -> Vec<String> {
    let mut refused: Vec<String> = repos
        .iter()
        .filter(|repo| !can_write(state, account, repo))
        .map(|repo| format!("repo {}", repo.full_name))
        .collect();
    for family in families {
        let members = super::family::repositories(state, family);
        if members.is_empty() {
            refused.push(format!("family {family} (it hosts no repository)"));
        } else if members.iter().any(|repo| !can_write(state, account, repo)) {
            refused.push(format!(
                "family {family} (you cannot write every repository of it)"
            ));
        }
    }
    refused
}

/// `POST /api/v1/agent-presence`: register or beat one session.
pub(crate) async fn beat(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    body: Bytes,
) -> AxumResponse {
    let beat: PresenceBeat = match serde_json::from_slice(&body) {
        Ok(beat) => beat,
        Err(err) => return invalid(&err.to_string()),
    };
    let beat: CheckedBeat = match beat.check() {
        Ok(beat) => beat,
        Err(BeatError::Invalid(reason)) => return invalid(&reason),
        Err(BeatError::ScopeEmpty) => {
            return presence_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "presence_scope_empty",
                "a session must name at least one repository or family",
                "send the repositories the session works on in repos, or its family in families",
            );
        }
    };
    let repos = match resolve_repos(&state, &account, &beat.repos) {
        Ok(repos) => repos,
        Err(response) => return *response,
    };
    let mut families = Vec::new();
    for raw in &beat.families {
        match visible_family(&state, &account, raw) {
            Ok(key) if !families.contains(&key) => families.push(key),
            Ok(_) => {}
            Err(response) => return *response,
        }
    }
    let admin = is_admin(&account);
    if !state.agent_presence.may_report_any(&account.login, admin) {
        let refused = forbidden_scope(&state, &account, &repos, &families);
        if !refused.is_empty() {
            return presence_error(
                StatusCode::FORBIDDEN,
                "presence_scope_forbidden",
                &format!("this login may not report {}", refused.join(", ")),
                "report only repositories you can write, or report as an admin or a JERYU_PRESENCE_REPORTERS login",
            );
        }
    }
    let store = &state.agent_presence.store;
    let row = store::PresenceRow {
        session_id: beat.session_id,
        reporter: account.login.clone(),
        agent: beat.agent,
        model: beat.model,
        host: beat.host,
        operator: beat.operator,
        lane: beat.lane,
        branch: beat.branch,
        title: beat.title,
        state: beat.state,
        repos: repos.into_iter().map(|repo| repo.full_name).collect(),
        families,
        started_ms: beat.started_ms,
        last_active_ms: beat.last_active_ms,
        ttl_seconds: beat.ttl_seconds,
        received_ms: 0,
        expires_ms: 0,
        ended_ms: None,
        cost_usd: beat.cost_usd,
        tokens: beat.tokens,
        links: beat.links,
    };
    match store.upsert_as(&row, &account.login, admin, Utc::now().timestamp_millis()) {
        Ok(stored) => Json(BeatAccepted {
            ok: true,
            session_id: stored.session_id,
            expires_at: stored.expires_ms,
        })
        .into_response(),
        Err(err) => write_failed(err, &row.session_id),
    }
}

/// `DELETE /api/v1/agent-presence/:session_id`: the session ended.
pub(crate) async fn end(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(session_id): AxumPath<String>,
) -> AxumResponse {
    if !valid_session_id(&session_id) {
        return invalid("session_id must be 1 to 64 characters of [A-Za-z0-9._:-]");
    }
    let store = &state.agent_presence.store;
    match store.end_as(
        &session_id,
        &account.login,
        is_admin(&account),
        Utc::now().timestamp_millis(),
    ) {
        Ok(Some(row)) => Json(json!({
            "ok": true,
            "session_id": row.session_id,
            "state": "ended",
            "ended_at": row.ended_ms,
        }))
        .into_response(),
        Ok(None) => presence_error(
            StatusCode::NOT_FOUND,
            "presence_session_not_found",
            "no session owned by this login is registered with that id",
            "end a session id that your login registered",
        ),
        Err(err) => write_failed(err, &session_id),
    }
}

/// `GET /api/v1/repos/:id/agent-presence`: every agent on one repository.
pub(crate) async fn repo_list(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(id): AxumPath<String>,
    StrictQuery(query): StrictQuery<RepoQuery>,
) -> AxumResponse {
    let page = match query.paging.page() {
        Ok(page) => page,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(repo) = find_repo(&state, &id) else {
        return presence_error(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("repository {id} was not found"),
            "address the repository by its id or owner/name",
        );
    };
    let now = Utc::now();
    let rows = match state.agent_presence.store.visible(now.timestamp_millis()) {
        Ok(rows) => rows,
        Err(err) => return store_failed(&err),
    };
    let items = repo_items(&state, &rows, &repo, is_admin(&account), now);
    let (items, info) = page.apply(items);
    Json(PresenceList {
        items,
        total: info.total,
        page: info,
    })
    .into_response()
}

/// `GET /api/v1/agent-presence[?family=]`: reported sessions of one family
/// (live, stale and recently ended), or every live session, that the caller
/// may see.
pub(crate) async fn overview(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    StrictQuery(query): StrictQuery<OverviewQuery>,
) -> AxumResponse {
    let page = match query.paging.page() {
        Ok(page) => page,
        Err(rejection) => return rejection.into_response(),
    };
    let family = match query
        .family
        .as_deref()
        .map(|raw| visible_family(&state, &account, raw))
        .transpose()
    {
        Ok(family) => family,
        Err(response) => return *response,
    };
    let now_ms = Utc::now().timestamp_millis();
    let rows = match state.agent_presence.store.visible(now_ms) {
        Ok(rows) => rows,
        Err(err) => return store_failed(&err),
    };
    let admin = is_admin(&account);
    let hosted: BTreeMap<String, Repository> = state
        .github
        .core()
        .list_repositories(None)
        .into_iter()
        .map(|repo| (repo.full_name.clone(), repo))
        .collect();
    let family_of = |name: &String| {
        hosted
            .get(name)
            .and_then(|repo| effective_family(&state, repo))
    };
    let mut items = Vec::new();
    for row in &rows {
        let matches = match &family {
            Some(key) => {
                row.families.contains(key)
                    || row
                        .repos
                        .iter()
                        .any(|name| family_of(name).as_ref() == Some(key))
            }
            None => matches!(effective_state(row, now_ms), "active" | "idle"),
        };
        if !matches {
            continue;
        }
        let repos: Vec<String> = row
            .repos
            .iter()
            .filter(|name| {
                admin
                    || hosted
                        .get(*name)
                        .is_some_and(|repo| can_read(&state, &account, repo))
            })
            .cloned()
            .collect();
        let families: Vec<String> = row
            .families
            .iter()
            .filter(|key| {
                admin
                    || hosted.values().any(|repo| {
                        effective_family(&state, repo).as_ref() == Some(*key)
                            && can_read(&state, &account, repo)
                    })
            })
            .cloned()
            .collect();
        if repos.is_empty() && families.is_empty() {
            continue;
        }
        let mut item = external_item(row, None, family.clone(), admin, now_ms);
        item.repos = Some(repos);
        item.families = Some(families);
        items.push(item);
    }
    // One protection lookup per repository named, not per item.
    let merge_only: BTreeMap<String, String> = items
        .iter()
        .flat_map(|item| item.repos.iter().flatten())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter_map(|name| {
            let repo = hosted.get(name)?;
            Some((name.clone(), sources::merge_only_branch(&state, repo)?))
        })
        .collect();
    mark_collisions(
        &mut items,
        |item| item.repos.clone(),
        |name| merge_only.get(name).cloned(),
    );
    sort(&mut items);
    let (items, info) = page.apply(items);
    Json(PresenceList {
        items,
        total: info.total,
        page: info,
    })
    .into_response()
}

#[cfg(test)]
mod tests;
