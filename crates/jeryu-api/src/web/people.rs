//! Which forge accounts are people and which are automation.
//!
//! An account is a person unless an administrator marks it `service`, or the
//! instance already names it as automation in its configuration (the shift
//! pull-request author, a `JERYU_SHIFT_REPORTERS` login). Account-owned bots
//! (`/api/v1/bots`) are not accounts at all and never appear here.
//!
//! The mark is kept per login in `site_settings` under `account_kind:<login>`,
//! so it needs no migration of its own and records who set it and when.
//!
//! - `GET /api/v1/admin/users`: every account with its `kind` and where the
//!   kind came from (`kind_source`: `set`, `config` or `default`).
//! - `PUT /api/v1/admin/users/:login/kind`: `{"kind": "human" | "service"}`.
//! - `GET /api/v1/people`: active people, for "assign to" pickers. Any
//!   logged-in account may read it; it carries logins and display names only.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::Utc;
use jeryu_core::{AccountStatus, AccountSummary, UserRole};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::auth::forbidden;
use super::shift::rfc3339_ms;
use super::{WebState, api_error};

const KIND_PREFIX: &str = "account_kind:";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccountKind {
    Human,
    Service,
}

impl AccountKind {
    fn as_str(self) -> &'static str {
        match self {
            AccountKind::Human => "human",
            AccountKind::Service => "service",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "human" => Some(AccountKind::Human),
            "service" => Some(AccountKind::Service),
            _ => None,
        }
    }
}

/// Where an account's kind came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KindSource {
    /// An administrator set it.
    Set,
    /// The instance's configuration names the login as automation.
    Config,
    /// Nothing says otherwise, so the account is a person.
    Default,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Kind {
    pub kind: AccountKind,
    pub source: KindSource,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
}

/// Every stored mark, by login.
fn stored_kinds(state: &WebState) -> Result<HashMap<String, Kind>, String> {
    let rows = state.site_settings.list_prefix(KIND_PREFIX)?;
    Ok(rows
        .into_iter()
        .filter_map(|(key, setting)| {
            let login = key.strip_prefix(KIND_PREFIX)?.to_string();
            let kind = AccountKind::parse(&setting.value)?;
            Some((
                login,
                Kind {
                    kind,
                    source: KindSource::Set,
                    updated_by: Some(setting.updated_by),
                    updated_at: Some(rfc3339_ms(setting.updated_ms)),
                },
            ))
        })
        .collect())
}

/// True when the instance's own configuration names `login` as automation.
fn configured_service(state: &WebState, login: &str) -> bool {
    state.shift.pr_author.as_deref() == Some(login)
        || state
            .shift
            .reporters
            .iter()
            .any(|reporter| reporter == login)
}

fn resolve(state: &WebState, stored: &HashMap<String, Kind>, login: &str) -> Kind {
    if let Some(kind) = stored.get(login) {
        return kind.clone();
    }
    let (kind, source) = if configured_service(state, login) {
        (AccountKind::Service, KindSource::Config)
    } else {
        (AccountKind::Human, KindSource::Default)
    };
    Kind {
        kind,
        source,
        updated_by: None,
        updated_at: None,
    }
}

/// The kind of one account. A store that cannot be read counts as "no mark",
/// so a configured automation login still reads as automation.
pub(crate) fn kind_of(state: &WebState, login: &str) -> Kind {
    let stored = stored_kinds(state).unwrap_or_default();
    resolve(state, &stored, login)
}

/// An active account that is a person: the only kind of account a todo can
/// be assigned to.
pub(crate) fn is_assignable_person(state: &WebState, login: &str) -> bool {
    state
        .core
        .get_account(login)
        .is_ok_and(|account| account.status == AccountStatus::Active)
        && kind_of(state, login).kind == AccountKind::Human
}

fn storage_error(error: &str) -> AxumResponse {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "storage_failed",
        &format!("could not read account kinds: {error}"),
    )
}

/// One account as the admin user list reports it: the core summary plus
/// `kind`, `kind_source` and, for a set kind, who set it and when.
fn with_kind(account: &AccountSummary, kind: &Kind) -> Value {
    let mut value = serde_json::to_value(account).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.insert("kind".into(), kind.kind.as_str().into());
        map.insert(
            "kind_source".into(),
            serde_json::to_value(kind.source).unwrap_or(Value::Null),
        );
        map.insert(
            "kind_updated_by".into(),
            kind.updated_by.clone().map_or(Value::Null, Value::from),
        );
        map.insert(
            "kind_updated_at".into(),
            kind.updated_at.clone().map_or(Value::Null, Value::from),
        );
    }
    value
}

/// `GET /api/v1/admin/users`
pub(crate) async fn admin_users(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> AxumResponse {
    if account.role != UserRole::Admin {
        return forbidden("admin role required");
    }
    let stored = match stored_kinds(&state) {
        Ok(stored) => stored,
        Err(error) => return storage_error(&error),
    };
    let users: Vec<Value> = state
        .core
        .list_accounts()
        .iter()
        .map(|user| with_kind(user, &resolve(&state, &stored, &user.login)))
        .collect();
    Json(users).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetKindRequest {
    pub kind: AccountKind,
}

/// `PUT /api/v1/admin/users/:login/kind`
pub(crate) async fn admin_set_kind(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    AxumPath(login): AxumPath<String>,
    Json(request): Json<SetKindRequest>,
) -> AxumResponse {
    if account.role != UserRole::Admin {
        return forbidden("admin role required");
    }
    let Ok(user) = state.core.get_account(&login) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "account_not_found",
            &format!("no account {login:?}"),
        );
    };
    let key = format!("{KIND_PREFIX}{}", user.login);
    let now_ms = Utc::now().timestamp_millis();
    if let Err(error) =
        state
            .site_settings
            .set(&key, Some(request.kind.as_str()), &account.login, now_ms)
    {
        return storage_error(&error);
    }
    Json(with_kind(&user, &kind_of(&state, &user.login))).into_response()
}

#[derive(Debug, Serialize)]
pub(crate) struct Person {
    pub login: String,
    pub display_name: String,
}

/// `GET /api/v1/people`: active accounts that are people, by login.
pub(crate) async fn people(State(state): State<Arc<WebState>>) -> AxumResponse {
    let stored = match stored_kinds(&state) {
        Ok(stored) => stored,
        Err(error) => return storage_error(&error),
    };
    let mut people: Vec<Person> = state
        .core
        .list_accounts()
        .into_iter()
        .filter(|user| user.status == AccountStatus::Active)
        .filter(|user| resolve(&state, &stored, &user.login).kind == AccountKind::Human)
        .map(|user| Person {
            display_name: user.display_name.clone(),
            login: user.login,
        })
        .collect();
    people.sort_by(|a, b| a.login.cmp(&b.login));
    Json(people).into_response()
}
