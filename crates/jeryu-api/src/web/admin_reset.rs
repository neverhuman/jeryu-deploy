//! An administrator's password reset, delivered by mail
//! (`POST /api/v1/admin/users/:login/reset-password`).
//!
//! The reset used to hand the temporary password back to the administrator,
//! who then had to carry it to the person some other way. It now goes to the
//! person's own address and never appears in the answer:
//!
//! - The address is the account's proved one when it has one. Without one,
//!   the administrator may supply an address, which is stored unproved, or the
//!   account's stored unproved address is used. With none of them the reset
//!   is refused with `email_required` before anything changes.
//! - The account gets a fresh one-time password through core's administrative
//!   reset, which drops its sessions, tokens and bots and forces a password
//!   change at the next sign-in.
//! - The temporary password is mailed with the sign-in address. A failed send
//!   is `502 mail_failed`; the administrator retries, and a retry issues a
//!   fresh password, so an unsent one is never live for long.
//!
//! Proving the address: a person who signs in with the mailed password and
//! completes the forced change has shown they read that mailbox. A successful
//! send records a marker (`account_emails.reset_mailed_ms`,
//! `db/migrations/0018_signup_links.sql`) which the password change in
//! [`super::auth::change_password`] spends to mark the address verified. A
//! claim of another address clears the marker, and a reset whose mail did not
//! leave clears it too, so only a temporary password that reached the stored
//! address proves anything.
//!
//! Operator notes: `docs/account-email.md`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Extension, Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::Utc;
use jeryu_core::{AccountSummary, ForgeError, UserRole};
use rusqlite::params;
use serde::Deserialize;
use serde_json::json;

use super::account_email::{
    AccountEmail, AccountEmailStore, audit, normalize_address, storage_failed,
};
use super::auth::{auth_rate_limit_exceeded, forbidden, rate_limited};
use super::mail::OutgoingMail;
use super::{WebState, api_error};

impl AccountEmailStore {
    /// Every stored address by login, for the administrator's user list.
    pub(crate) fn all(&self) -> Result<BTreeMap<String, AccountEmail>, String> {
        let conn = self.conn();
        let mut statement = conn
            .prepare("SELECT login, address, verified_ms FROM account_emails")
            .map_err(|err| err.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    AccountEmail {
                        address: row.get(1)?,
                        verified_ms: row.get(2)?,
                    },
                ))
            })
            .map_err(|err| err.to_string())?;
        rows.collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(|err| err.to_string())
    }

    /// Record that a temporary password was mailed to `address`, but only if
    /// it is still the account's stored address.
    pub(crate) fn mark_reset_mailed(
        &self,
        login: &str,
        address: &str,
        now_ms: i64,
    ) -> Result<(), String> {
        self.conn()
            .execute(
                "UPDATE account_emails SET reset_mailed_ms = ?3
                 WHERE login = ?1 AND address = ?2",
                params![login, address, now_ms],
            )
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    /// Forget an earlier mailed reset: the password it carried was replaced
    /// by one that did not reach the mailbox.
    pub(crate) fn clear_reset_mailed(&self, login: &str) -> Result<(), String> {
        self.conn()
            .execute(
                "UPDATE account_emails SET reset_mailed_ms = NULL WHERE login = ?1",
                params![login],
            )
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    /// Spend the mailed-reset marker: mark the address proved (keeping an
    /// earlier proof's time) and clear the marker. `true` when there was one.
    pub(crate) fn prove_by_mailed_reset(&self, login: &str, now_ms: i64) -> Result<bool, String> {
        self.conn()
            .execute(
                "UPDATE account_emails
                 SET verified_ms = COALESCE(verified_ms, ?2), reset_mailed_ms = NULL
                 WHERE login = ?1 AND reset_mailed_ms IS NOT NULL",
                params![login, now_ms],
            )
            .map(|changed| changed == 1)
            .map_err(|err| err.to_string())
    }
}

/// After a successful password change: a marker means the password just
/// replaced was one an administrator mailed, so the address it went to is
/// proved. A failure is recorded and swallowed; the change itself succeeded.
pub(super) fn prove_mailed_address(state: &WebState, login: &str) {
    let now_ms = Utc::now().timestamp_millis();
    match state.account_email.prove_by_mailed_reset(login, now_ms) {
        Ok(true) => audit(
            state,
            login,
            "account.email.verified",
            "completed",
            json!({ "via": "mailed_password_reset" }),
        ),
        Ok(false) => {}
        Err(error) => audit(
            state,
            login,
            "account.email.verified",
            "failed",
            json!({ "reason": format!("could not record the proof: {error}") }),
        ),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdminResetRequest {
    #[serde(default)]
    email: Option<String>,
}

/// `POST /api/v1/admin/users/:login/reset-password` — set a one-time password
/// and mail it to the account's address.
///
/// The body is optional: `{}` or nothing uses the address the account has,
/// `{"email"}` supplies one for an account without a proved address.
/// Answers `202 {"result":"sent","login","email"}` and never the password.
pub(super) async fn admin_reset_password(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    AxumPath(login): AxumPath<String>,
    body: Bytes,
) -> AxumResponse {
    if account.role != UserRole::Admin {
        return forbidden("admin role required");
    }
    if auth_rate_limit_exceeded(&state, peer.as_ref(), &headers, "reset", &login) {
        return rate_limited();
    }
    // Checked before anything changes: a reset nobody can be told about would
    // lock the person out.
    if !state.mailer.is_configured() {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "mail_unavailable",
            "this forge cannot send mail, so it cannot deliver a reset; configure mail first (docs/account-email.md)",
        );
    }
    let request: AdminResetRequest = if body.iter().all(u8::is_ascii_whitespace) {
        AdminResetRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(error) => {
                return api_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_input",
                    &format!("the body must be {{}} or {{\"email\": \"...\"}}: {error}"),
                );
            }
        }
    };
    let supplied = match request.email.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => match normalize_address(raw) {
            Some(address) => Some(address),
            None => {
                return api_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_input",
                    "enter a valid email address",
                );
            }
        },
    };
    let target = match state.core.get_account(&login) {
        Ok(target) => target,
        Err(ForgeError::NotFound(_)) => {
            return api_error(StatusCode::NOT_FOUND, "not_found", "user not found");
        }
        Err(error) => {
            return storage_failed("could not read the account", &error.to_string());
        }
    };
    let stored = match state.account_email.get(&target.login) {
        Ok(stored) => stored,
        Err(error) => return storage_failed("could not read the account address", &error),
    };
    let address = match choose_address(stored.as_ref(), supplied) {
        Ok(address) => address,
        Err((status, code, message)) => return api_error(status, code, message),
    };
    let now_ms = Utc::now().timestamp_millis();
    if stored.as_ref().map(|stored| stored.address.as_str()) != Some(address.as_str()) {
        match state.account_email.claim(&target.login, &address, now_ms) {
            Ok(true) => audit(
                &state,
                &account.login,
                "account.email.claimed",
                "completed",
                json!({ "for": &target.login }),
            ),
            Ok(false) => {
                return api_error(
                    StatusCode::CONFLICT,
                    "conflict",
                    "that address is already in use",
                );
            }
            Err(error) => return storage_failed("could not store the account address", &error),
        }
    }
    let password = match state.core.generate_one_time_password() {
        Ok(password) => password,
        Err(error) => {
            return storage_failed("could not generate password", &error.to_string());
        }
    };
    // Core's administrative reset drops every session, token and bot of the
    // account and forces a password change at the next sign-in.
    if let Err(error) = state.core.reset_account_password(&target.login, &password) {
        return match error {
            ForgeError::NotFound(_) => {
                api_error(StatusCode::NOT_FOUND, "not_found", "user not found")
            }
            error => storage_failed("could not reset password", &error.to_string()),
        };
    }
    let sign_in = state.mailer.link("").unwrap_or_default();
    let body = format!(
        "An administrator reset the password for the jeryu account {login}.\n\
         \n\
         Sign in at {sign_in}\n\
         \n\
         Login: {login}\n\
         Temporary password: {password}\n\
         \n\
         You must choose a new password when you sign in. The temporary\n\
         password stops working once you do, and choosing one also confirms\n\
         this address for recovering the account.\n",
        login = target.login,
    );
    let outcome = state
        .mailer
        .send(OutgoingMail {
            to: address.clone(),
            subject: "Your jeryu password was reset".to_string(),
            body,
        })
        .await;
    if let Err(error) = outcome {
        // The password that did not leave must not prove the address if it
        // is ever used, and an older mailed one no longer works.
        let _ = state.account_email.clear_reset_mailed(&target.login);
        audit(
            &state,
            &account.login,
            "account.password_reset.admin_mailed",
            "failed",
            json!({ "for": &target.login, "reason": format!("could not send the message: {error}") }),
        );
        return api_error(
            StatusCode::BAD_GATEWAY,
            "mail_failed",
            "the password was reset but the message could not be sent; reset again to send a fresh one",
        );
    }
    if let Err(error) = state
        .account_email
        .mark_reset_mailed(&target.login, &address, now_ms)
    {
        audit(
            &state,
            &account.login,
            "account.password_reset.admin_mailed",
            "failed",
            json!({ "for": &target.login, "reason": format!("could not record the mailed reset: {error}") }),
        );
    }
    audit(
        &state,
        &account.login,
        "account.password_reset.admin_mailed",
        "completed",
        json!({ "for": &target.login, "transport": state.mailer.transport_name() }),
    );
    (
        StatusCode::ACCEPTED,
        Json(json!({ "result": "sent", "login": target.login, "email": address })),
    )
        .into_response()
}

/// Which address a reset goes to: the proved one, else the one supplied, else
/// the stored unproved one.
fn choose_address(
    stored: Option<&AccountEmail>,
    supplied: Option<String>,
) -> Result<String, (StatusCode, &'static str, &'static str)> {
    match (stored, supplied) {
        (Some(stored), Some(supplied)) if stored.is_verified() && stored.address != supplied => {
            Err((
                StatusCode::CONFLICT,
                "conflict",
                "the account already has a verified address",
            ))
        }
        (Some(stored), _) if stored.is_verified() => Ok(stored.address.clone()),
        (_, Some(supplied)) => Ok(supplied),
        (Some(stored), None) => Ok(stored.address.clone()),
        (None, None) => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "email_required",
            "this account has no email address; supply one",
        )),
    }
}
