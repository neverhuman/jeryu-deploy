//! Which forge accounts are people and which are automation.
//!
//! An account is a person unless an administrator marks it `service`, or the
//! instance already names it as automation in its configuration: the shift
//! pull-request author, a `JERYU_SHIFT_REPORTERS` login, the review and merge
//! identities, or a login in `JERYU_AUTOMATION_IDENTITIES`,
//! `JERYU_CI_PUBLISHERS` or `JERYU_JANKURAI_SCORERS`. A `JERYU_DEPLOYERS`
//! login also defaults to service, because release hosts can hold those
//! credentials. An administrator can explicitly mark a human deployer
//! `human`; that mark cannot override another configured automation role.
//! Account-owned bots (`/api/v1/bots`) are not accounts at all.
//!
//! Whether an account is a person is a security decision (only people take
//! todos on), so it fails closed: when the marks cannot be read, nobody
//! counts as a person.
//!
//! The mark is kept per login in `site_settings` under `account_kind:<login>`,
//! so it needs no migration of its own and records who set it and when.
//!
//! - `GET /api/v1/admin/users`: every account with its `kind` and where the
//!   kind came from (`kind_source`: `set`, `config` or `default`).
//! - `PUT /api/v1/admin/users/:login/kind`: `{"kind": "human" | "service"}`.
//!
//! The "assign to" picker lists people per family, and only to callers who
//! can work in it: `GET /api/v1/shift/todos/:family/:id/people` (`shift.rs`).

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
    use super::auth::{
        AUTOMATION_IDENTITIES, CI_PUBLISHERS, JANKURAI_SCORERS, MERGE_IDENTITY, REVIEW_IDENTITY,
    };
    let named = |logins: &[String]| logins.iter().any(|named| named == login);
    state.shift.pr_author.as_deref() == Some(login)
        || named(&state.shift.reporters)
        || REVIEW_IDENTITY.configured() == Some(login)
        || MERGE_IDENTITY.configured() == Some(login)
        || named(AUTOMATION_IDENTITIES.configured())
        || named(CI_PUBLISHERS.configured())
        || named(JANKURAI_SCORERS.configured())
}

/// Configured automation wins over a stored mark. Deployment permission alone
/// permits an explicit human mark, while preserving the service default for
/// existing machine-held deployer credentials.
fn resolve(state: &WebState, stored: &HashMap<String, Kind>, login: &str) -> Kind {
    let unset = |kind, source| Kind {
        kind,
        source,
        updated_by: None,
        updated_at: None,
    };
    if configured_service(state, login) {
        return unset(AccountKind::Service, KindSource::Config);
    }
    match stored.get(login) {
        Some(kind) => kind.clone(),
        None if super::auth::DEPLOYERS
            .configured()
            .iter()
            .any(|named| named == login) =>
        {
            unset(AccountKind::Service, KindSource::Config)
        }
        None => unset(AccountKind::Human, KindSource::Default),
    }
}

/// The kind of one account, or the store error that kept it from being read.
pub(crate) fn kind_of(state: &WebState, login: &str) -> Result<Kind, String> {
    let stored = stored_kinds(state)?;
    Ok(resolve(state, &stored, login))
}

/// True only when `login` is known to be a person. A store that cannot be
/// read answers false: an account marked as automation must never be let
/// through because its mark was unreadable.
pub(crate) fn is_person(state: &WebState, login: &str) -> bool {
    kind_of(state, login).is_ok_and(|kind| kind.kind == AccountKind::Human)
}

/// The canonical login of an active account that is a person, the only kind
/// of account a todo can be assigned to. The login is the one the account
/// store returns, never the caller's spelling of it.
pub(crate) fn assignable_person(state: &WebState, login: &str) -> Option<String> {
    let account = state.core.get_account(login).ok()?;
    (account.status == AccountStatus::Active && is_person(state, &account.login))
        .then_some(account.login)
}

/// Active people, sorted by login, kept by `keep`. A store that cannot be
/// read is an error, not an empty list.
pub(crate) fn active_people(
    state: &WebState,
    keep: impl Fn(&str) -> bool,
) -> Result<Vec<Person>, String> {
    let stored = stored_kinds(state)?;
    let mut people: Vec<Person> = state
        .core
        .list_accounts()
        .into_iter()
        .filter(|user| user.status == AccountStatus::Active)
        .filter(|user| resolve(state, &stored, &user.login).kind == AccountKind::Human)
        .filter(|user| keep(&user.login))
        .map(|user| Person {
            display_name: user.display_name.clone(),
            login: user.login,
        })
        .collect();
    people.sort_by(|a, b| a.login.cmp(&b.login));
    Ok(people)
}

pub(crate) fn storage_error(error: &str) -> AxumResponse {
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

/// `email` (the stored address or `null`) and `emailVerified`, so an
/// administrator can see who a mailed reset would reach.
fn with_email(value: &mut Value, email: Option<&super::account_email::AccountEmail>) {
    if let Value::Object(map) = value {
        map.insert(
            "email".into(),
            email.map_or(Value::Null, |email| Value::from(email.address.clone())),
        );
        map.insert(
            "emailVerified".into(),
            Value::from(email.is_some_and(super::account_email::AccountEmail::is_verified)),
        );
    }
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
    let emails = match state.account_email.all() {
        Ok(emails) => emails,
        Err(error) => {
            return super::account_email::storage_failed(
                "could not read the account addresses",
                &error,
            );
        }
    };
    let users: Vec<Value> = state
        .core
        .list_accounts()
        .iter()
        .map(|user| {
            let mut value = with_kind(user, &resolve(&state, &stored, &user.login));
            with_email(&mut value, emails.get(&user.login));
            value
        })
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
    match kind_of(&state, &user.login) {
        Ok(kind) => Json(with_kind(&user, &kind)).into_response(),
        Err(error) => storage_error(&error),
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Person {
    pub login: String,
    pub display_name: String,
}
