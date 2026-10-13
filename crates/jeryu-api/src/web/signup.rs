//! Sign-up that starts from a proved email address
//! (`<data_dir>/shift.sqlite`, `db/migrations/0018_signup_links.sql`).
//!
//! An account used to be created from a login and a password alone, so it
//! could exist with no address and nothing to recover it with. Sign-up is now
//! two steps, and the account only ever exists with its address proved:
//!
//! - `POST /api/v1/auth/signup/start` `{"email"}`: mail a link to the address.
//!   Open, and answers `202 {"result":"sent"}` for every valid address, so the
//!   response says nothing about whether an account already holds it. An
//!   address an account has proved is mailed a note naming that account
//!   instead of a link.
//! - `POST /api/v1/auth/signup` `{"token","login","password"}`: spend the
//!   link, create the account, and store the address already proved. A body
//!   without a token is refused with `email_required`.
//!
//! The link is the same shape as the recovery links in
//! [`super::account_email`]: 32 bytes of operating-system randomness of which
//! only the sha256 is stored, spent by one atomic `UPDATE`, so an expired,
//! replayed or unissued link is one indistinguishable refusal.
//!
//! What keeps the order of the second step safe:
//!
//! - A weak password is refused before the link is spent.
//! - A login that is taken, or that core refuses, un-spends the link, so the
//!   person picks another login with the same mail.
//! - An address another account proved while the link sat in the mailbox is
//!   refused before any account is created.
//! - An address another account proved between creating the account and
//!   storing its address disables the new account: an account never exists
//!   without the address it proved.
//!
//! Operator configuration: `docs/account-email.md`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::Utc;
use jeryu_core::{ForgeError, UserRole};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::json;

use super::account_email::{
    AccountEmailStore, HOUR_SECS, IssuedLink, PASSWORD_MIN_BYTES, address_key, audit, digest,
    link_refused, normalize_address, random_secret, storage_failed,
};
use super::auth::{
    auth_rate_limit_exceeded, client_ip_for_limits, limits_hit_for, new_account_session,
    rate_limited,
};
use super::mail::OutgoingMail;
use super::{WebState, api_error};

/// A sign-up link is good for a day, like a verification link: it proves the
/// same thing, a day after the person asked.
const SIGNUP_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// A consumed or expired row is kept this long before the prune drops it, so
/// it still counts towards the durable cap and a replay is answered from a
/// stored row.
const SIGNUP_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// One client address may start five sign-ups an hour, whatever address it
/// names, and a separate route bucket caps the whole path per minute.
const START_IP_MAX: u32 = 5;
const START_ROUTE_MAX: u32 = 30;
const START_ROUTE_WINDOW_SECS: i64 = 60;
/// One address may be mailed three times an hour from memory, so a stranger
/// cannot use the forge to post mail at somebody.
const START_ADDRESS_MAX: u32 = 3;
/// The durable half of the same cap, which a restart does not forget. It is
/// per address, not per client, so it holds even when every request arrives
/// through one proxy.
const START_ADDRESS_MAX_PER_HOUR: u32 = 5;

/// What one `signup_links` row records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignupRowKind {
    /// A spendable link.
    Link,
    /// The "an account already uses this address" message, which carries no
    /// link. Written consumed, so it counts and can never be spent.
    Notice,
}

impl SignupRowKind {
    fn stored(self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::Notice => "notice",
        }
    }
}

/// How storing the proved address for a new account went.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StoreVerified {
    Stored,
    /// Another account has proved the address.
    AddressTaken,
}

impl AccountEmailStore {
    /// Record one message to `address` and, for a link, return its secret.
    /// Issuing a link retires the address's older outstanding links, and rows
    /// nobody can spend or count any more are pruned.
    fn record_signup(
        &self,
        kind: SignupRowKind,
        address: &str,
        now_ms: i64,
    ) -> Result<IssuedLink, String> {
        let secret = random_secret();
        let hash = digest(&secret);
        let consumed = match kind {
            SignupRowKind::Link => None,
            SignupRowKind::Notice => Some(now_ms),
        };
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(|err| err.to_string())?;
        tx.execute(
            "DELETE FROM signup_links WHERE expires_ms < ?1",
            params![now_ms - SIGNUP_RETENTION_MS],
        )
        .map_err(|err| err.to_string())?;
        if kind == SignupRowKind::Link {
            tx.execute(
                "UPDATE signup_links SET consumed_ms = ?2
                 WHERE address = ?1 AND kind = 'link' AND consumed_ms IS NULL",
                params![address, now_ms],
            )
            .map_err(|err| err.to_string())?;
        }
        tx.execute(
            "INSERT INTO signup_links
               (token_hash, kind, address, issued_ms, expires_ms, consumed_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                hash,
                kind.stored(),
                address,
                now_ms,
                now_ms + SIGNUP_TTL_MS,
                consumed
            ],
        )
        .map_err(|err| err.to_string())?;
        tx.commit().map_err(|err| err.to_string())?;
        Ok(IssuedLink { secret })
    }

    /// Issue a sign-up link for `address`. The returned secret is the only
    /// copy; the row holds its sha256.
    pub(crate) fn issue_signup(&self, address: &str, now_ms: i64) -> Result<IssuedLink, String> {
        self.record_signup(SignupRowKind::Link, address, now_ms)
    }

    /// Count a message that carried no link towards the address's cap.
    fn record_signup_notice(&self, address: &str, now_ms: i64) -> Result<(), String> {
        self.record_signup(SignupRowKind::Notice, address, now_ms)
            .map(|_| ())
    }

    /// Spend a sign-up link and return the address it proves. The same `None`
    /// for an unknown, expired or already-spent secret.
    pub(crate) fn spend_signup(&self, secret: &str, now_ms: i64) -> Result<Option<String>, String> {
        let hash = digest(secret);
        let conn = self.conn();
        let spent = conn
            .execute(
                "UPDATE signup_links SET consumed_ms = ?2
                 WHERE token_hash = ?1 AND kind = 'link'
                   AND consumed_ms IS NULL AND expires_ms > ?2",
                params![hash, now_ms],
            )
            .map_err(|err| err.to_string())?;
        if spent != 1 {
            return Ok(None);
        }
        conn.query_row(
            "SELECT address FROM signup_links WHERE token_hash = ?1",
            params![hash],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| err.to_string())
    }

    /// Make a link spent a moment ago spendable again, because the account it
    /// was spent for could not be created. Only the one row, and only while
    /// it has not expired.
    pub(crate) fn unspend_signup(&self, secret: &str, now_ms: i64) -> Result<(), String> {
        self.conn()
            .execute(
                "UPDATE signup_links SET consumed_ms = NULL
                 WHERE token_hash = ?1 AND kind = 'link' AND expires_ms > ?2",
                params![digest(secret), now_ms],
            )
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    /// How many messages `address` was sent for sign-up since `since_ms`.
    pub(crate) fn signups_sent_since(&self, address: &str, since_ms: i64) -> Result<u32, String> {
        self.conn()
            .query_row(
                "SELECT COUNT(*) FROM signup_links WHERE address = ?1 AND issued_ms >= ?2",
                params![address, since_ms],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
            .map_err(|err| err.to_string())
    }

    /// Store `address` for `login` already proved, because a sign-up link for
    /// it was just spent, and retire the address's other sign-up links.
    ///
    /// Another account's *unproved* claim on the address gives way: it never
    /// granted that account anything, and this person just proved they read
    /// the mailbox. That account's outstanding links for the address are
    /// retired with it. Another account's *proved* claim wins.
    pub(crate) fn insert_verified(
        &self,
        login: &str,
        address: &str,
        now_ms: i64,
    ) -> Result<StoreVerified, String> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(|err| err.to_string())?;
        let holder: Option<(String, Option<i64>)> = tx
            .query_row(
                "SELECT login, verified_ms FROM account_emails WHERE address = ?1",
                params![address],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|err| err.to_string())?;
        match holder {
            Some((holder, Some(_))) if holder != login => return Ok(StoreVerified::AddressTaken),
            Some((holder, None)) if holder != login => {
                tx.execute(
                    "DELETE FROM account_emails WHERE login = ?1",
                    params![holder],
                )
                .map_err(|err| err.to_string())?;
                tx.execute(
                    "UPDATE account_email_tokens SET consumed_ms = ?3
                     WHERE login = ?1 AND address = ?2 AND consumed_ms IS NULL",
                    params![holder, address, now_ms],
                )
                .map_err(|err| err.to_string())?;
            }
            _ => {}
        }
        // A row a deleted account left behind under the same login is
        // replaced, not kept: it belonged to somebody else.
        tx.execute(
            "INSERT INTO account_emails (login, address, verified_ms, added_ms, reset_mailed_ms)
             VALUES (?1, ?2, ?3, ?3, NULL)
             ON CONFLICT(login) DO UPDATE SET
               address = excluded.address,
               verified_ms = excluded.verified_ms,
               added_ms = excluded.added_ms,
               reset_mailed_ms = NULL",
            params![login, address, now_ms],
        )
        .map_err(|err| err.to_string())?;
        tx.execute(
            "UPDATE account_email_tokens SET consumed_ms = ?2
             WHERE login = ?1 AND consumed_ms IS NULL",
            params![login, now_ms],
        )
        .map_err(|err| err.to_string())?;
        tx.execute(
            "UPDATE signup_links SET consumed_ms = ?2
             WHERE address = ?1 AND consumed_ms IS NULL",
            params![address, now_ms],
        )
        .map_err(|err| err.to_string())?;
        tx.commit().map_err(|err| err.to_string())?;
        Ok(StoreVerified::Stored)
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct SignupStartRequest {
    email: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct SignupRequest {
    /// The secret from the mailed link. Optional in the body so that a bare
    /// `{login,password}` from an older client is answered with
    /// `email_required` and its repair, not a parse error.
    #[serde(default)]
    token: Option<String>,
    login: String,
    password: String,
}

/// `POST /api/v1/auth/signup/start` — mail a sign-up link, and say nothing.
///
/// `503 mail_unavailable` when the forge cannot send mail: sign-up is
/// impossible without it, and saying so is honest, because it is the same for
/// every address. A malformed address is `422`. Every valid address answers
/// `202 {"result":"sent"}`: whether an account holds it and whether a message
/// actually left are invisible to the caller.
pub(super) async fn start(
    State(state): State<Arc<WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(request): Json<SignupStartRequest>,
) -> AxumResponse {
    if !state.mailer.is_configured() {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "mail_unavailable",
            "this forge cannot send mail, so it cannot verify an address to sign up with; ask an administrator to create the account",
        );
    }
    let address = normalize_address(&request.email);
    if start_rate_limit_exceeded(&state, peer.as_ref(), &headers, address.as_deref()) {
        return rate_limited();
    }
    let Some(address) = address else {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "enter a valid email address",
        );
    };
    let now_ms = Utc::now().timestamp_millis();
    // Counted from the table, before the holder is looked up: a link and a
    // notice both count, so the cap is reached at the same rate either way.
    match state
        .account_email
        .signups_sent_since(&address, now_ms - HOUR_SECS * 1000)
    {
        Ok(sent) if sent >= START_ADDRESS_MAX_PER_HOUR => return rate_limited(),
        Ok(_) => {}
        Err(error) => return storage_failed("could not count the sent links", &error),
    }
    send_signup_mail(&state, &address, now_ms).await;
    (StatusCode::ACCEPTED, Json(json!({ "result": "sent" }))).into_response()
}

/// Mail a sign-up link, or, when an account has proved the address, a note
/// naming it. Every outcome is recorded and none of them reaches the caller.
async fn send_signup_mail(state: &Arc<WebState>, address: &str, now_ms: i64) {
    let holder = match state.account_email.verified_holder(address) {
        Ok(holder) => holder,
        Err(error) => {
            signup_audit(
                state,
                "failed",
                json!({ "reason": format!("could not read the address: {error}") }),
            );
            return;
        }
    };
    let (subject, body, kind) = match holder {
        Some(login) => {
            if let Err(error) = state.account_email.record_signup_notice(address, now_ms) {
                signup_audit(
                    state,
                    "failed",
                    json!({ "reason": format!("could not record the message: {error}") }),
                );
                return;
            }
            let forgot = state.mailer.link("forgot-password").unwrap_or_default();
            let body = format!(
                "Somebody asked to create a jeryu account with this address, but an\n\
                 account already uses it: the login is {login}.\n\
                 \n\
                 Sign in with that login, or reset its password here:\n\
                 \n\
                 {forgot}\n\
                 \n\
                 If this was not you, no action is needed: nothing has changed.\n"
            );
            ("Your jeryu account already exists", body, "notice")
        }
        None => {
            let link = match state.account_email.issue_signup(address, now_ms) {
                Ok(link) => link,
                Err(error) => {
                    signup_audit(
                        state,
                        "failed",
                        json!({ "reason": format!("could not issue the link: {error}") }),
                    );
                    return;
                }
            };
            let url = state
                .mailer
                .link(&format!("create-account?token={}", link.secret))
                .unwrap_or_default();
            let body = format!(
                "Somebody asked to create a jeryu account with this address.\n\
                 \n\
                 Follow this link within a day to choose a login and a password:\n\
                 \n\
                 {url}\n\
                 \n\
                 The link works once. If this was not you, no action is needed: no\n\
                 account is created until the link is followed.\n"
            );
            ("Finish creating your jeryu account", body, "link")
        }
    };
    let outcome = state
        .mailer
        .send(OutgoingMail {
            to: address.to_string(),
            subject: subject.to_string(),
            body,
        })
        .await;
    match outcome {
        Ok(()) => signup_audit(
            state,
            "completed",
            json!({ "message": kind, "transport": state.mailer.transport_name() }),
        ),
        Err(error) => signup_audit(
            state,
            "failed",
            json!({ "message": kind, "reason": format!("could not send the message: {error}") }),
        ),
    }
}

/// A sign-up request names no account yet, so its records are the forge's
/// own. The detail never carries the address or the login it is held by.
fn signup_audit(state: &WebState, phase: &str, detail: serde_json::Value) {
    audit(state, "local", "account.signup.requested", phase, detail);
}

/// `POST /api/v1/auth/signup` — spend a sign-up link and create the account.
///
/// Answers exactly as sign-in does on success: the account, its CSRF token and
/// the session cookie.
pub(super) async fn complete(
    State(state): State<Arc<WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(request): Json<SignupRequest>,
) -> AxumResponse {
    if auth_rate_limit_exceeded(&state, peer.as_ref(), &headers, "signup", &request.login) {
        return rate_limited();
    }
    let Some(token) = request
        .token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
    else {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "email_required",
            "sign-up needs a verified email address: POST /api/v1/auth/signup/start with {\"email\"} and follow the mailed link",
        );
    };
    if request.password.len() < PASSWORD_MIN_BYTES {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "password must be at least 12 bytes",
        );
    }
    let now_ms = Utc::now().timestamp_millis();
    let address = match state.account_email.spend_signup(token, now_ms) {
        Ok(Some(address)) => address,
        Ok(None) => return link_refused("signup_link_invalid"),
        Err(error) => return storage_failed("could not read the sign-up link", &error),
    };
    // Proved by another account while the link sat in the mailbox. The link
    // stays spent: it can only ever reach the same refusal.
    match state.account_email.verified_holder(&address) {
        Ok(Some(_)) => return address_in_use(),
        Ok(None) => {}
        Err(error) => return storage_failed("could not read the account address", &error),
    }
    let account = match state
        .core
        .create_account(&request.login, &request.password, UserRole::User)
    {
        Ok(account) => account,
        Err(error) => {
            // No account was created, so the person keeps their link and
            // tries another login.
            if let Err(unspend) = state.account_email.unspend_signup(token, now_ms) {
                signup_audit(
                    &state,
                    "failed",
                    json!({ "reason": format!("could not restore the link: {unspend}") }),
                );
            }
            return match error {
                ForgeError::Conflict(_) => api_error(
                    StatusCode::CONFLICT,
                    "conflict",
                    "an account with that login already exists",
                ),
                ForgeError::Validation(reason) => {
                    api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", &reason)
                }
                error => api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage_failed",
                    &format!("could not create account: {error}"),
                ),
            };
        }
    };
    let stored = state
        .account_email
        .insert_verified(&account.login, &address, now_ms);
    if stored != Ok(StoreVerified::Stored) {
        // An account must never exist without the address it proved, so the
        // one just created is switched off rather than left reachable.
        let _ = state.core.disable_account(&account.login);
        audit(
            &state,
            &account.login,
            "account.signup.completed",
            "failed",
            json!({ "reason": "the proved address could not be stored" }),
        );
        return match stored {
            Err(error) => storage_failed("could not store the account address", &error),
            Ok(_) => address_in_use(),
        };
    }
    audit(
        &state,
        &account.login,
        "account.signup.completed",
        "completed",
        json!({}),
    );
    new_account_session(&state, account)
}

fn address_in_use() -> AxumResponse {
    api_error(
        StatusCode::CONFLICT,
        "conflict",
        "that address is already in use",
    )
}

/// Per client address, per address asked for, and per route. The per-address
/// buckets are what hold when every client shares one proxy address.
fn start_rate_limit_exceeded(
    state: &WebState,
    peer: Option<&ConnectInfo<SocketAddr>>,
    headers: &HeaderMap,
    address: Option<&str>,
) -> bool {
    let ip = client_ip_for_limits(peer.map(|connect| connect.0), headers);
    let now = Utc::now();
    let mut limits = state
        .auth_rate_limits
        .lock()
        .expect("auth rate-limit mutex poisoned");
    if limits_hit_for(
        &mut limits,
        format!("signup_start:{ip}"),
        now,
        START_IP_MAX,
        HOUR_SECS,
    ) {
        return true;
    }
    if let Some(address) = address
        && limits_hit_for(
            &mut limits,
            format!("signup_start:address:{}", address_key(address)),
            now,
            START_ADDRESS_MAX,
            HOUR_SECS,
        )
    {
        return true;
    }
    limits_hit_for(
        &mut limits,
        "signup_start:route".to_string(),
        now,
        START_ROUTE_MAX,
        START_ROUTE_WINDOW_SECS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn store() -> AccountEmailStore {
        let path = std::env::temp_dir().join(format!("jeryu-signup-{}.sqlite", Uuid::new_v4()));
        AccountEmailStore::open(&path).expect("open the account email store")
    }

    #[test]
    fn a_signup_link_is_spendable_once_and_can_be_restored() {
        let store = store();
        let link = store.issue_signup("ada@example.com", 1_000).unwrap();
        assert_eq!(
            store.spend_signup(&link.secret, 2_000).unwrap().as_deref(),
            Some("ada@example.com")
        );
        assert_eq!(store.spend_signup(&link.secret, 3_000).unwrap(), None);
        store.unspend_signup(&link.secret, 3_000).unwrap();
        assert!(store.spend_signup(&link.secret, 4_000).unwrap().is_some());
    }

    #[test]
    fn an_expired_signup_link_and_a_guess_refuse_the_same_way() {
        let store = store();
        let link = store.issue_signup("ada@example.com", 1_000).unwrap();
        assert_eq!(
            store
                .spend_signup(&link.secret, 1_000 + SIGNUP_TTL_MS + 1)
                .unwrap(),
            None
        );
        assert_eq!(store.spend_signup(&random_secret(), 2_000).unwrap(), None);
        // Restoring an expired link does not bring it back.
        store
            .unspend_signup(&link.secret, 1_000 + SIGNUP_TTL_MS + 1)
            .unwrap();
        assert_eq!(
            store
                .spend_signup(&link.secret, 1_000 + SIGNUP_TTL_MS + 2)
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_new_link_retires_the_older_one_and_a_notice_is_never_spendable() {
        let store = store();
        let first = store.issue_signup("ada@example.com", 1_000).unwrap();
        let second = store.issue_signup("ada@example.com", 2_000).unwrap();
        assert_eq!(store.spend_signup(&first.secret, 3_000).unwrap(), None);
        let notice = store
            .record_signup(SignupRowKind::Notice, "ada@example.com", 3_000)
            .unwrap();
        assert_eq!(store.spend_signup(&notice.secret, 4_000).unwrap(), None);
        assert!(store.spend_signup(&second.secret, 4_000).unwrap().is_some());
        assert_eq!(
            store.signups_sent_since("ada@example.com", 0).unwrap(),
            3,
            "links and notices both count towards the cap"
        );
    }

    #[test]
    fn storing_a_proved_address_beats_an_unproved_claim_but_not_a_proved_one() {
        let store = store();
        store.claim("bob", "ada@example.com", 1_000).unwrap();
        assert_eq!(
            store
                .insert_verified("ada", "ada@example.com", 2_000)
                .unwrap(),
            StoreVerified::Stored
        );
        assert_eq!(store.get("bob").unwrap(), None);
        assert_eq!(
            store.verified_holder("ada@example.com").unwrap().as_deref(),
            Some("ada")
        );
        assert_eq!(
            store
                .insert_verified("carol", "ada@example.com", 3_000)
                .unwrap(),
            StoreVerified::AddressTaken
        );
    }

    #[test]
    fn the_stored_signup_row_holds_only_a_digest_of_the_link() {
        let store = store();
        let link = store.issue_signup("ada@example.com", 1_000).unwrap();
        let stored: String = store
            .conn()
            .query_row("SELECT token_hash FROM signup_links", [], |row| row.get(0))
            .unwrap();
        assert_ne!(stored, link.secret);
        assert_eq!(stored, digest(&link.secret));
    }
}
