//! The email address an account owns, proving it, and recovering a password
//! with it (`<data_dir>/shift.sqlite`, `db/migrations/0017_account_emails.sql`).
//!
//! An account used to carry no address, so a person who lost their password
//! could only wait for an administrator to run
//! `POST /api/v1/admin/users/:login/reset-password`. These routes are the
//! self-service way back, and the verification that makes it safe:
//!
//! - `GET /api/v1/auth/email`: the caller's own address and whether it is
//!   proved.
//! - `PUT /api/v1/auth/email`: claim an address. It is stored unproved and a
//!   verification link is sent to it, so claiming an address never grants
//!   anything by itself.
//! - `POST /api/v1/auth/email/verify/resend`: send that link again.
//! - `POST /api/v1/auth/email/verify`: spend a verification link. Open,
//!   because the link is followed from a mail client that may not be signed
//!   in, and the link already names the account.
//! - `POST /api/v1/auth/password/forgot`: send a reset link. Open, and
//!   answers `202 {"result":"sent"}` for every well-formed request, so the
//!   response says nothing about whether an account has that address. It
//!   answers before the sending starts, so the response time says nothing
//!   either (`web/background.rs`).
//! - `POST /api/v1/auth/password/reset`: spend a reset link and set the
//!   password through core's self-service recovery
//!   (`ForgeCore::recover_password`), which ends every session and revokes
//!   every personal access token and the account's own bots' credentials,
//!   and clears a forced password change. Team bots are untouched.
//!
//! What keeps a link safe:
//!
//! - The secret is 32 bytes of operating-system randomness, and only its
//!   sha256 is stored, so the database holds nothing spendable. A reset
//!   link's secret is the recovery secret core issued
//!   (`ForgeCore::issue_password_recovery`); this store binds it to the proved
//!   address it was mailed to, and core spends it.
//! - Spending is one `UPDATE ... WHERE consumed_ms IS NULL AND expires_ms >
//!   now` under the store's mutex, so a replay of a spent link, an expired
//!   link, and a link that never existed are one indistinguishable refusal.
//! - A link records the address it was issued for, so one issued before the
//!   account moved to another address proves and recovers nothing.
//! - A reset is only ever sent to a *proved* address, so taking over an
//!   unproved address takes over no account.
//! - Claiming, proving or resetting retires that account's other outstanding
//!   links, so an older link in an older mailbox stops working.
//! - Nothing here writes a secret or an address to a log, and the rate-limit
//!   keys hold a digest of the address rather than the address.
//! - Recovery is rate limited before anything is looked up, and a reset link
//!   never reaches the caller: it goes only to the address an account proved.
//!
//! Sign-up from a proved address (`super::signup`) and the administrator's
//! mailed reset (`super::admin_reset`) are built on this store.
//!
//! Operator configuration and the backfill policy for accounts that already
//! exist: `docs/account-email.md`.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response as AxumResponse};
use chrono::Utc;
use jeryu_core::{AccountSummary, ForgeError};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::auth::{client_ip_for_limits, limits_hit_for, rate_limited};
use super::mail::OutgoingMail;
use super::shift::migrate_shift_store;
use super::{WebState, api_error};

/// A verification link is good for a day: long enough for a person who reads
/// their mail the next morning, short enough that an old mailbox is not a key.
const VERIFY_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// A reset link is good for an hour. It is the stronger capability of the two.
const RESET_TTL_MS: i64 = 60 * 60 * 1000;
/// A consumed or expired link is kept this long before the prune drops it, so
/// a replay in the minutes after a reset is still answered from a stored row
/// rather than from an empty table.
const TOKEN_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

pub(super) const HOUR_SECS: i64 = 60 * 60;
/// One client address may ask for five reset links an hour, whatever address
/// it names, and a separate route bucket caps the whole path per minute.
const FORGOT_IP_MAX: u32 = 5;
const FORGOT_ROUTE_MAX: u32 = 30;
const FORGOT_ROUTE_WINDOW_SECS: i64 = 60;
/// One address may be mailed three reset links an hour, so a stranger cannot
/// use the forge to post mail at somebody.
const FORGOT_ADDRESS_MAX: u32 = 3;
/// The durable half of the same cap, which a restart does not forget.
const FORGOT_ACCOUNT_MAX_PER_HOUR: u32 = 5;
/// Spending a link is cheap to get wrong by accident and expensive to guess,
/// so the cap is per client address and generous.
const SPEND_IP_MAX: u32 = 10;
/// How many verification links one account may be sent an hour.
const VERIFY_SEND_MAX: u32 = 5;
/// Reject a body larger than this before it is parsed. A token is 64 hex
/// characters and a password is a password.
pub(super) const ACCOUNT_EMAIL_BODY_MAX: usize = 4096;

/// Core's own floor, checked here so a weak password is refused before the
/// one-use link is spent on it.
pub(super) const PASSWORD_MIN_BYTES: usize = 12;
/// The longest new password the reset route hashes. The body cap already
/// bounds it; this names the bound before anything reaches core.
pub(super) const PASSWORD_MAX_BYTES: usize = 1024;
/// The longest link secret the reset route reads: core's recovery secrets
/// are well under it and core refuses longer ones itself.
const LINK_SECRET_MAX: usize = 128;

const EMAIL_MAX: usize = 254;
const LOCAL_MAX: usize = 64;

/// What a link is for. The purpose is part of the lookup, so a verification
/// link can never be spent as a reset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinkPurpose {
    Verify,
    Reset,
}

impl LinkPurpose {
    fn stored(self) -> &'static str {
        match self {
            Self::Verify => "verify",
            Self::Reset => "reset",
        }
    }

    fn ttl_ms(self) -> i64 {
        match self {
            Self::Verify => VERIFY_TTL_MS,
            Self::Reset => RESET_TTL_MS,
        }
    }
}

/// An account's stored address, folded to lowercase.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AccountEmail {
    pub address: String,
    /// `None` until a verification link for `address` was followed.
    pub verified_ms: Option<i64>,
}

impl AccountEmail {
    pub(crate) fn is_verified(&self) -> bool {
        self.verified_ms.is_some()
    }
}

/// A spent link, as the store hands it back.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SpentLink {
    pub login: String,
    pub address: String,
}

/// The secret half of a freshly issued link. Never stored, never logged.
pub(crate) struct IssuedLink {
    pub(super) secret: String,
}

impl IssuedLink {
    /// The secret, for a test that has to present a link the handler never
    /// mailed (an expiry in the past, a link of the wrong purpose).
    #[cfg(test)]
    pub(crate) fn secret_for_test(&self) -> String {
        self.secret.clone()
    }
}

impl std::fmt::Debug for IssuedLink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IssuedLink")
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct AccountEmailStore {
    inner: Arc<std::sync::Mutex<Connection>>,
}

impl AccountEmailStore {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|err| err.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|err| err.to_string())?;
        migrate_shift_store(&conn)?;
        Ok(Self {
            inner: Arc::new(std::sync::Mutex::new(conn)),
        })
    }

    pub(super) fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.inner.lock().expect("account email mutex poisoned")
    }

    /// The account's address, or `None` when it has claimed none.
    pub(crate) fn get(&self, login: &str) -> Result<Option<AccountEmail>, String> {
        self.conn()
            .query_row(
                "SELECT address, verified_ms FROM account_emails WHERE login = ?1",
                params![login],
                |row| {
                    Ok(AccountEmail {
                        address: row.get(0)?,
                        verified_ms: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(|err| err.to_string())
    }

    /// Store `address` (already folded by [`normalize_address`]) for `login`,
    /// unproved, and retire every link the account still had outstanding: an
    /// address the person is replacing must not keep a live reset link in its
    /// mailbox. A temporary password mailed to the old address proves nothing
    /// about the new one, so the mailed-reset marker is cleared too.
    ///
    /// Returns `false` for another account's verified address or an unverified
    /// claim younger than a day. An expired unverified claim and its outstanding
    /// links are retired atomically, so a claim cannot squat an address forever.
    pub(crate) fn claim(&self, login: &str, address: &str, now_ms: i64) -> Result<bool, String> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(|err| err.to_string())?;
        let holder: Option<(String, Option<i64>, i64)> = tx
            .query_row(
                "SELECT login, verified_ms, added_ms FROM account_emails WHERE address = ?1",
                params![address],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|err| err.to_string())?;
        if let Some((holder, verified_ms, added_ms)) = holder
            && holder != login
        {
            if verified_ms.is_some() || added_ms > now_ms.saturating_sub(VERIFY_TTL_MS) {
                return Ok(false);
            }
            tx.execute(
                "UPDATE account_email_tokens SET consumed_ms = ?2
                 WHERE login = ?1 AND consumed_ms IS NULL",
                params![holder, now_ms],
            )
            .map_err(|err| err.to_string())?;
            tx.execute(
                "DELETE FROM account_emails WHERE login = ?1",
                params![holder],
            )
            .map_err(|err| err.to_string())?;
        }
        tx.execute(
            "INSERT INTO account_emails (login, address, verified_ms, added_ms)
             VALUES (?1, ?2, NULL, ?3)
             ON CONFLICT(login) DO UPDATE SET
               address = excluded.address,
               verified_ms = NULL,
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
        tx.commit().map_err(|err| err.to_string())?;
        Ok(true)
    }

    /// Mark the account's address proved, but only if it is still the address
    /// the link was issued for. Returns `false` when the account has moved on.
    pub(crate) fn mark_verified(
        &self,
        login: &str,
        address: &str,
        now_ms: i64,
    ) -> Result<bool, String> {
        let changed = self
            .conn()
            .execute(
                "UPDATE account_emails SET verified_ms = ?3
                 WHERE login = ?1 AND address = ?2",
                params![login, address, now_ms],
            )
            .map_err(|err| err.to_string())?;
        Ok(changed == 1)
    }

    /// The account that proved `address`, if any. An unproved address answers
    /// `None`, which is what keeps recovery off it.
    pub(crate) fn verified_holder(&self, address: &str) -> Result<Option<String>, String> {
        self.conn()
            .query_row(
                "SELECT login FROM account_emails
                 WHERE address = ?1 AND verified_ms IS NOT NULL",
                params![address],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| err.to_string())
    }

    /// Issue a link for `purpose`, retiring any outstanding link of the same
    /// purpose, and prune rows nobody can spend any more. The returned secret
    /// is the only copy; the row holds its sha256.
    ///
    /// Retiring marks `consumed_ms` rather than deleting the row, so a
    /// retired link still counts towards [`Self::issued_since`]: a person who
    /// asks ten times gets one spendable link and has still been sent ten
    /// messages, which is what the send cap is about.
    pub(crate) fn issue(
        &self,
        purpose: LinkPurpose,
        login: &str,
        address: &str,
        now_ms: i64,
    ) -> Result<IssuedLink, String> {
        self.issue_secret(purpose, login, address, random_secret(), now_ms)
    }

    /// As [`Self::issue`], for a secret minted elsewhere: a reset link carries
    /// the recovery secret core issued, and this row binds it to `address`.
    pub(crate) fn issue_secret(
        &self,
        purpose: LinkPurpose,
        login: &str,
        address: &str,
        secret: String,
        now_ms: i64,
    ) -> Result<IssuedLink, String> {
        let hash = digest(&secret);
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(|err| err.to_string())?;
        tx.execute(
            "DELETE FROM account_email_tokens WHERE expires_ms < ?1",
            params![now_ms - TOKEN_RETENTION_MS],
        )
        .map_err(|err| err.to_string())?;
        tx.execute(
            "UPDATE account_email_tokens SET consumed_ms = ?3
             WHERE login = ?1 AND purpose = ?2 AND consumed_ms IS NULL",
            params![login, purpose.stored(), now_ms],
        )
        .map_err(|err| err.to_string())?;
        tx.execute(
            "INSERT INTO account_email_tokens
               (token_hash, purpose, login, address, issued_ms, expires_ms, consumed_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
            params![
                hash,
                purpose.stored(),
                login,
                address,
                now_ms,
                now_ms + purpose.ttl_ms()
            ],
        )
        .map_err(|err| err.to_string())?;
        tx.commit().map_err(|err| err.to_string())?;
        Ok(IssuedLink { secret })
    }

    /// Spend `secret`, if it is a live link of `purpose`. The same `None` for
    /// an unknown, expired, already-spent or wrong-purpose secret.
    pub(crate) fn spend(
        &self,
        purpose: LinkPurpose,
        secret: &str,
        now_ms: i64,
    ) -> Result<Option<SpentLink>, String> {
        let hash = digest(secret);
        let conn = self.conn();
        let spent = conn
            .execute(
                "UPDATE account_email_tokens SET consumed_ms = ?3
                 WHERE token_hash = ?1 AND purpose = ?2
                   AND consumed_ms IS NULL AND expires_ms > ?3",
                params![hash, purpose.stored(), now_ms],
            )
            .map_err(|err| err.to_string())?;
        if spent != 1 {
            return Ok(None);
        }
        conn.query_row(
            "SELECT login, address FROM account_email_tokens WHERE token_hash = ?1",
            params![hash],
            |row| {
                Ok(SpentLink {
                    login: row.get(0)?,
                    address: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(|err| err.to_string())
    }

    /// Retire every outstanding link for `login` after a reset: a password
    /// that just changed must not leave a second reset link live.
    pub(crate) fn drop_outstanding(&self, login: &str, now_ms: i64) -> Result<(), String> {
        self.conn()
            .execute(
                "UPDATE account_email_tokens SET consumed_ms = ?2
                 WHERE login = ?1 AND consumed_ms IS NULL",
                params![login, now_ms],
            )
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    /// How many links of `purpose` the account was issued since `since_ms`.
    pub(crate) fn issued_since(
        &self,
        purpose: LinkPurpose,
        login: &str,
        since_ms: i64,
    ) -> Result<u32, String> {
        self.conn()
            .query_row(
                "SELECT COUNT(*) FROM account_email_tokens
                 WHERE login = ?1 AND purpose = ?2 AND issued_ms >= ?3",
                params![login, purpose.stored(), since_ms],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
            .map_err(|err| err.to_string())
    }
}

/// 32 bytes of operating-system randomness as 64 hex characters.
///
/// `Uuid::new_v4` is the crate's seam onto `getrandom`; two of them carry 244
/// unpredictable bits, well past what a link needs.
pub(super) fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    hex::encode(bytes)
}

pub(super) fn digest(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// A rate-limit key that identifies an address without holding it: the
/// limiter's map is in memory and read by anything that can print the state.
pub(super) fn address_key(address: &str) -> String {
    hex::encode(&Sha256::digest(address.as_bytes())[..16])
}

/// The same rules core applies to a waitlist address, so one address is one
/// address across the forge: trimmed, folded to lowercase, one `@`, a bounded
/// local part of ordinary characters, and a dotted domain of label characters.
pub(crate) fn normalize_address(raw: &str) -> Option<String> {
    let address = raw.trim().to_ascii_lowercase();
    let (local, domain) = address.split_once('@')?;
    let ok = address.len() >= 3
        && address.len() <= EMAIL_MAX
        && !local.is_empty()
        && local.len() <= LOCAL_MAX
        && address.matches('@').count() == 1
        && local_part_ok(local)
        && domain_ok(domain);
    ok.then_some(address)
}

fn local_part_ok(local: &str) -> bool {
    !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-')
        })
}

fn domain_ok(domain: &str) -> bool {
    let labels: Vec<&str> = domain.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

#[derive(Debug, Deserialize)]
pub(super) struct ClaimAddressRequest {
    address: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct SpendLinkRequest {
    token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ForgotPasswordRequest {
    email: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ResetPasswordRequest {
    token: String,
    new_password: String,
}

#[derive(Debug, Serialize)]
pub(super) struct AccountEmailResponse {
    address: Option<String>,
    verified: bool,
}

/// `GET /api/v1/auth/email` — the caller's own address and whether it is proved.
pub(super) async fn get_email(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> AxumResponse {
    match state.account_email.get(&account.login) {
        Ok(stored) => Json(AccountEmailResponse {
            verified: stored.as_ref().is_some_and(AccountEmail::is_verified),
            address: stored.map(|stored| stored.address),
        })
        .into_response(),
        Err(error) => storage_failed("could not read the account address", &error),
    }
}

/// `PUT /api/v1/auth/email` — claim an address and send its verification link.
///
/// A new address is stored unproved whatever happens next, so a failed send
/// leaves the account exactly as a fresh claim does and a resend repairs it.
/// Saving the address the account already proved changes nothing and answers
/// `200 {"result":"verified","verified":true}`.
pub(super) async fn put_email(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Json(request): Json<ClaimAddressRequest>,
) -> AxumResponse {
    let Some(address) = normalize_address(&request.address) else {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "enter a valid email address",
        );
    };
    // Saving the address the account has already proved must not un-prove it:
    // a second press of Save would otherwise cost the person their recovery.
    match state.account_email.get(&account.login) {
        Ok(Some(stored)) if stored.address == address && stored.is_verified() => {
            return (
                StatusCode::OK,
                Json(json!({ "result": "verified", "verified": true })),
            )
                .into_response();
        }
        Ok(_) => {}
        Err(error) => return storage_failed("could not read the account address", &error),
    }
    let now_ms = Utc::now().timestamp_millis();
    match state.account_email.claim(&account.login, &address, now_ms) {
        // The conflict is only ever reported to a signed-in account about an
        // address it just typed, which it already knows it typed.
        Ok(false) => {
            return api_error(
                StatusCode::CONFLICT,
                "conflict",
                "that address is already in use",
            );
        }
        Ok(true) => {}
        Err(error) => return storage_failed("could not store the account address", &error),
    }
    audit(
        &state,
        &account.login,
        "account.email.claimed",
        "completed",
        json!({}),
    );
    send_verification(&state, &account.login, &address, now_ms).await;
    (
        StatusCode::ACCEPTED,
        Json(json!({ "result": "sent", "verified": false })),
    )
        .into_response()
}

/// `POST /api/v1/auth/email/verify/resend` — send the verification link again.
pub(super) async fn resend_verification(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> AxumResponse {
    let stored = match state.account_email.get(&account.login) {
        Ok(Some(stored)) => stored,
        Ok(None) => {
            return api_error(
                StatusCode::CONFLICT,
                "conflict",
                "this account has no email address to verify",
            );
        }
        Err(error) => return storage_failed("could not read the account address", &error),
    };
    if stored.is_verified() {
        return (
            StatusCode::OK,
            Json(json!({ "result": "verified", "verified": true })),
        )
            .into_response();
    }
    let now_ms = Utc::now().timestamp_millis();
    let since_ms = now_ms - HOUR_SECS * 1000;
    match state
        .account_email
        .issued_since(LinkPurpose::Verify, &account.login, since_ms)
    {
        Ok(issued) if issued >= VERIFY_SEND_MAX => return rate_limited(),
        Ok(_) => {}
        Err(error) => return storage_failed("could not count the sent links", &error),
    }
    send_verification(&state, &account.login, &stored.address, now_ms).await;
    (
        StatusCode::ACCEPTED,
        Json(json!({ "result": "sent", "verified": false })),
    )
        .into_response()
}

/// `POST /api/v1/auth/email/verify` — spend a verification link.
///
/// Open: the link is followed from a mail client that may hold no session, and
/// the link itself names the account. An unknown, expired, replayed or
/// wrong-purpose secret is one refusal.
pub(super) async fn verify_email(
    State(state): State<Arc<WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(request): Json<SpendLinkRequest>,
) -> AxumResponse {
    if spend_rate_limit_exceeded(&state, peer.as_ref(), &headers, "email_verify") {
        return rate_limited();
    }
    let now_ms = Utc::now().timestamp_millis();
    let spent = match state
        .account_email
        .spend(LinkPurpose::Verify, request.token.trim(), now_ms)
    {
        Ok(Some(spent)) => spent,
        Ok(None) => return link_refused("verification_link_invalid"),
        Err(error) => return storage_failed("could not read the verification link", &error),
    };
    match state
        .account_email
        .mark_verified(&spent.login, &spent.address, now_ms)
    {
        // The account replaced the address after the link was sent, so the
        // link proves nothing about the address it holds now.
        Ok(false) => link_refused("verification_link_invalid"),
        Ok(true) => {
            audit(
                &state,
                &spent.login,
                "account.email.verified",
                "completed",
                json!({}),
            );
            Json(json!({ "result": "verified", "verified": true })).into_response()
        }
        Err(error) => storage_failed("could not record the verification", &error),
    }
}

/// `POST /api/v1/auth/password/forgot` — send a reset link, and say nothing.
///
/// Every well-formed request answers `202 {"result":"sent"}`: whether an
/// account holds that address, whether it proved it, and whether a message
/// actually left are all invisible to the caller. A malformed address is the
/// same answer, because refusing it would tell a caller which of its guesses
/// are even addresses this forge would store.
///
/// The answer also carries no *timing* of any of that: looking the address
/// up, issuing a link and handing a message to the host's mailer all happen
/// after the response, on a spawned task (`web/background.rs`). An address no
/// account has proved therefore costs a caller the same wait as one that just
/// received a link.
pub(super) async fn forgot_password(
    State(state): State<Arc<WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(request): Json<ForgotPasswordRequest>,
) -> AxumResponse {
    let address = normalize_address(&request.email);
    if forgot_rate_limit_exceeded(&state, peer.as_ref(), &headers, address.as_deref()) {
        return rate_limited();
    }
    if let Some(address) = address {
        let worker = Arc::clone(&state);
        state
            .background
            .spawn(async move { issue_reset(&worker, &address).await });
    }
    (StatusCode::ACCEPTED, Json(json!({ "result": "sent" }))).into_response()
}

/// Send one reset link, if the address is proved by an account that can sign
/// in. Every outcome is recorded and none of them reaches the caller.
async fn issue_reset(state: &Arc<WebState>, address: &str) {
    let now_ms = Utc::now().timestamp_millis();
    let login = match state.account_email.verified_holder(address) {
        Ok(Some(login)) => login,
        Ok(None) => {
            // Either no account holds the address or it never proved it. The
            // detail names neither, so the log is not an address directory.
            audit(
                state,
                "local",
                "account.password_reset.requested",
                "requested",
                json!({ "reason": "no account has verified that address" }),
            );
            return;
        }
        Err(error) => {
            audit(
                state,
                "local",
                "account.password_reset.requested",
                "failed",
                json!({ "reason": format!("could not read the address: {error}") }),
            );
            return;
        }
    };
    let since_ms = now_ms - HOUR_SECS * 1000;
    match state
        .account_email
        .issued_since(LinkPurpose::Reset, &login, since_ms)
    {
        Ok(issued) if issued >= FORGOT_ACCOUNT_MAX_PER_HOUR => {
            audit(
                state,
                &login,
                "account.password_reset.requested",
                "requested",
                json!({ "reason": "hourly reset link cap reached" }),
            );
            return;
        }
        Ok(_) => {}
        Err(error) => {
            audit(
                state,
                &login,
                "account.password_reset.requested",
                "failed",
                json!({ "reason": format!("could not count the sent links: {error}") }),
            );
            return;
        }
    }
    // Core mints the recovery secret and is the one that spends it; an
    // account that cannot sign in is refused there, and gets no message.
    let recovery = match state.core.issue_password_recovery(&login) {
        Ok(recovery) => recovery,
        Err(error) => {
            audit(
                state,
                &login,
                "account.password_reset.requested",
                "failed",
                json!({ "reason": format!("core refused the recovery: {error}") }),
            );
            return;
        }
    };
    let link = match state.account_email.issue_secret(
        LinkPurpose::Reset,
        &login,
        address,
        recovery.token,
        now_ms,
    ) {
        Ok(link) => link,
        Err(error) => {
            audit(
                state,
                &login,
                "account.password_reset.requested",
                "failed",
                json!({ "reason": format!("could not issue the link: {error}") }),
            );
            return;
        }
    };
    let Some(url) = state
        .mailer
        .link(&format!("reset-password?token={}", link.secret))
    else {
        audit(
            state,
            &login,
            "account.password_reset.requested",
            "failed",
            json!({ "reason": "mail is not configured", "docs": DOCS }),
        );
        return;
    };
    let body = format!(
        "Somebody asked to reset the password for the jeryu account {login}.\n\
         \n\
         Follow this link within the hour to choose a new one:\n\
         \n\
         {url}\n\
         \n\
         The link works once. If this was not you, no action is needed: your\n\
         password has not changed and the link expires on its own.\n"
    );
    let outcome = state
        .mailer
        .send(OutgoingMail {
            to: address.to_string(),
            subject: "Reset your jeryu password".to_string(),
            body,
            html: None,
        })
        .await;
    match outcome {
        Ok(()) => audit(
            state,
            &login,
            "account.password_reset.requested",
            "completed",
            json!({ "transport": state.mailer.transport_name() }),
        ),
        Err(error) => audit(
            state,
            &login,
            "account.password_reset.requested",
            "failed",
            json!({ "reason": format!("could not send the message: {error}") }),
        ),
    }
}

/// `POST /api/v1/auth/password/reset` — spend a reset link and set the password.
///
/// The password is checked against core's floor *before* the link is spent, so
/// a password the forge would refuse does not burn the person's one link.
pub(super) async fn reset_password(
    State(state): State<Arc<WebState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(request): Json<ResetPasswordRequest>,
) -> AxumResponse {
    if spend_rate_limit_exceeded(&state, peer.as_ref(), &headers, "password_reset") {
        return rate_limited();
    }
    if request.new_password.len() < PASSWORD_MIN_BYTES {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "password must be at least 12 bytes",
        );
    }
    if request.new_password.len() > PASSWORD_MAX_BYTES {
        return api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
            "password must be at most 1024 bytes",
        );
    }
    let token = request.token.trim();
    if token.is_empty() || token.len() > LINK_SECRET_MAX {
        return link_refused("reset_link_invalid");
    }
    let now_ms = Utc::now().timestamp_millis();
    let spent = match state.account_email.spend(LinkPurpose::Reset, token, now_ms) {
        Ok(Some(spent)) => spent,
        Ok(None) => return link_refused("reset_link_invalid"),
        Err(error) => return storage_failed("could not read the reset link", &error),
    };
    // The address must still be the proved one: an account that replaced its
    // address after the link was sent is not reachable at the old one.
    match state.account_email.verified_holder(&spent.address) {
        Ok(Some(holder)) if holder == spent.login => {}
        Ok(_) => return link_refused("reset_link_invalid"),
        Err(error) => return storage_failed("could not read the account address", &error),
    }
    // Core spends the same secret: it ends every session, revokes every
    // personal access token and the account's own bots' credentials, and
    // clears a forced password change, in one transaction. Its refusals
    // (spent, expired, account changed since issue) are one answer, as here.
    match state.core.recover_password(token, &request.new_password) {
        Ok(_) => {}
        Err(ForgeError::Validation(_) | ForgeError::NotFound(_)) => {
            return link_refused("reset_link_invalid");
        }
        Err(error) => return storage_failed("could not set the new password", &error.to_string()),
    }
    if let Err(error) = state.account_email.drop_outstanding(&spent.login, now_ms) {
        return storage_failed("could not retire the remaining links", &error);
    }
    audit(
        &state,
        &spent.login,
        "account.password_reset.completed",
        "completed",
        json!({}),
    );
    Json(json!({ "result": "reset" })).into_response()
}

/// Issue and send a verification link. A failure is recorded, never returned:
/// the claim already succeeded, and a resend is the repair.
async fn send_verification(state: &Arc<WebState>, login: &str, address: &str, now_ms: i64) {
    // The cap lives here rather than only on the resend route, so claiming an
    // address again and again is a way to post mail at nobody either.
    match state
        .account_email
        .issued_since(LinkPurpose::Verify, login, now_ms - HOUR_SECS * 1000)
    {
        Ok(issued) if issued >= VERIFY_SEND_MAX => {
            audit(
                state,
                login,
                "account.email.verification_sent",
                "requested",
                json!({ "reason": "hourly verification link cap reached" }),
            );
            return;
        }
        Ok(_) => {}
        Err(error) => {
            audit(
                state,
                login,
                "account.email.verification_sent",
                "failed",
                json!({ "reason": format!("could not count the sent links: {error}") }),
            );
            return;
        }
    }
    let link = match state
        .account_email
        .issue(LinkPurpose::Verify, login, address, now_ms)
    {
        Ok(link) => link,
        Err(error) => {
            audit(
                state,
                login,
                "account.email.verification_sent",
                "failed",
                json!({ "reason": format!("could not issue the link: {error}") }),
            );
            return;
        }
    };
    let Some(url) = state
        .mailer
        .link(&format!("verify-email?token={}", link.secret))
    else {
        audit(
            state,
            login,
            "account.email.verification_sent",
            "failed",
            json!({ "reason": "mail is not configured", "docs": DOCS }),
        );
        return;
    };
    let body = format!(
        "Confirm this address for the jeryu account {login}.\n\
         \n\
         Follow this link within a day:\n\
         \n\
         {url}\n\
         \n\
         The link works once. Until it is followed the address is not confirmed\n\
         and cannot be used to recover the account.\n"
    );
    let outcome = state
        .mailer
        .send(OutgoingMail {
            to: address.to_string(),
            subject: "Confirm your jeryu email address".to_string(),
            body,
            html: None,
        })
        .await;
    match outcome {
        Ok(()) => audit(
            state,
            login,
            "account.email.verification_sent",
            "completed",
            json!({ "transport": state.mailer.transport_name() }),
        ),
        Err(error) => audit(
            state,
            login,
            "account.email.verification_sent",
            "failed",
            json!({ "reason": format!("could not send the message: {error}") }),
        ),
    }
}

pub(super) const DOCS: &str = "docs/account-email.md";

/// Every unspendable link is this one refusal, so a caller cannot tell an
/// expired link from a replayed one from a guess.
pub(super) fn link_refused(code: &str) -> AxumResponse {
    api_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        code,
        "that link is no longer valid; ask for a new one",
    )
}

pub(super) fn storage_failed(message: &str, error: &str) -> AxumResponse {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "storage_failed",
        &format!("{message}: {error}"),
    )
}

/// Record a security event, swallowing a log failure: refusing the request
/// because the audit log is full would hand a caller a way to tell which
/// requests reached an account.
///
/// `phase` is core's closed set — `requested`, `completed` or `failed`. A
/// request that reached nothing to send to is `requested`: it happened and
/// went no further, which is not a failure of the forge.
pub(super) fn audit(
    state: &WebState,
    actor: &str,
    action: &str,
    phase: &str,
    detail: serde_json::Value,
) {
    let _ = state
        .core
        .append_audit_as(actor, action, "account_email", phase, detail);
}

/// Per client address, per address asked for, and per route.
fn forgot_rate_limit_exceeded(
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
        format!("password_forgot:{ip}"),
        now,
        FORGOT_IP_MAX,
        HOUR_SECS,
    ) {
        return true;
    }
    if let Some(address) = address
        && limits_hit_for(
            &mut limits,
            format!("password_forgot:address:{}", address_key(address)),
            now,
            FORGOT_ADDRESS_MAX,
            HOUR_SECS,
        )
    {
        return true;
    }
    limits_hit_for(
        &mut limits,
        "password_forgot:route".to_string(),
        now,
        FORGOT_ROUTE_MAX,
        FORGOT_ROUTE_WINDOW_SECS,
    )
}

/// Per client address, for the two routes that spend a link.
fn spend_rate_limit_exceeded(
    state: &WebState,
    peer: Option<&ConnectInfo<SocketAddr>>,
    headers: &HeaderMap,
    action: &str,
) -> bool {
    let ip = client_ip_for_limits(peer.map(|connect| connect.0), headers);
    let now = Utc::now();
    let mut limits = state
        .auth_rate_limits
        .lock()
        .expect("auth rate-limit mutex poisoned");
    limits_hit_for(
        &mut limits,
        format!("{action}:{ip}"),
        now,
        SPEND_IP_MAX,
        HOUR_SECS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store inside a directory the test owns, so the sqlite file and its
    /// `-wal`/`-shm` siblings are removed when the test ends, including on
    /// panic. Dereferences to the store, so a test reads like it holds one.
    struct TestStore {
        store: AccountEmailStore,
        _dir: tempfile::TempDir,
    }

    impl std::ops::Deref for TestStore {
        type Target = AccountEmailStore;

        fn deref(&self) -> &Self::Target {
            &self.store
        }
    }

    fn store() -> TestStore {
        let dir = tempfile::tempdir().expect("account email store directory");
        let store = AccountEmailStore::open(&dir.path().join("account-email.sqlite"))
            .expect("open the account email store");
        TestStore { store, _dir: dir }
    }

    #[test]
    fn addresses_are_normalized_the_way_core_normalizes_them() {
        assert_eq!(
            normalize_address("  Ada@Example.COM "),
            Some("ada@example.com".to_string())
        );
        for refused in [
            "",
            "ada",
            "ada@",
            "@example.com",
            "ada@example",
            "ada@@example.com",
            ".ada@example.com",
            "ada.@example.com",
            "a..da@example.com",
            "ada@-example.com",
            "ada@example-.com",
            "ada example@example.com",
        ] {
            assert_eq!(normalize_address(refused), None, "{refused:?} was accepted");
        }
    }

    /// Two secrets must not collide, and neither is stored.
    #[test]
    fn a_secret_is_sixty_four_hex_characters_and_fresh_each_time() {
        let first = random_secret();
        let second = random_secret();
        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(digest(&first), first);
    }

    #[test]
    fn a_claim_stores_the_address_unverified() {
        let store = store();
        assert!(store.claim("ada", "ada@example.com", 1_000).unwrap());
        let stored = store.get("ada").unwrap().expect("the address is stored");
        assert_eq!(stored.address, "ada@example.com");
        assert!(!stored.is_verified());
        assert_eq!(store.verified_holder("ada@example.com").unwrap(), None);
    }

    #[test]
    fn one_address_belongs_to_one_account() {
        let store = store();
        assert!(store.claim("ada", "ada@example.com", 1_000).unwrap());
        assert!(!store.claim("bob", "ada@example.com", 2_000).unwrap());
        // The same account may re-claim its own address.
        assert!(store.claim("ada", "ada@example.com", 3_000).unwrap());
        assert_eq!(store.get("bob").unwrap(), None);
    }

    #[test]
    fn an_expired_unverified_claim_is_reclaimable_and_old_links_are_retired() {
        let store = store();
        let address = "owner@example.com";
        assert!(store.claim("squatter", address, 1_000).unwrap());
        let link = store
            .issue(
                LinkPurpose::Verify,
                "squatter",
                address,
                1_000 + VERIFY_TTL_MS - 1,
            )
            .unwrap();
        assert!(
            !store
                .claim("owner", address, 1_000 + VERIFY_TTL_MS - 1)
                .unwrap()
        );
        assert!(
            store
                .claim("owner", address, 1_000 + VERIFY_TTL_MS)
                .unwrap()
        );
        assert_eq!(store.get("squatter").unwrap(), None);
        assert_eq!(store.get("owner").unwrap().unwrap().address, address);
        assert_eq!(
            store
                .spend(LinkPurpose::Verify, &link.secret, 1_000 + VERIFY_TTL_MS + 1)
                .unwrap(),
            None,
            "even a recently resent old link must not prove the retired claim"
        );
    }

    #[test]
    fn a_verified_claim_never_expires_for_another_account() {
        let store = store();
        let address = "owner@example.com";
        assert!(store.claim("owner", address, 1_000).unwrap());
        assert!(store.mark_verified("owner", address, 2_000).unwrap());
        assert!(
            !store
                .claim("other", address, 1_000 + VERIFY_TTL_MS * 30)
                .unwrap()
        );
        assert_eq!(
            store.verified_holder(address).unwrap().as_deref(),
            Some("owner")
        );
    }

    #[test]
    fn verifying_marks_only_the_address_the_link_was_issued_for() {
        let store = store();
        store.claim("ada", "ada@example.com", 1_000).unwrap();
        assert!(
            !store
                .mark_verified("ada", "other@example.com", 2_000)
                .unwrap()
        );
        assert!(
            store
                .mark_verified("ada", "ada@example.com", 2_000)
                .unwrap()
        );
        assert_eq!(
            store.verified_holder("ada@example.com").unwrap().as_deref(),
            Some("ada")
        );
    }

    #[test]
    fn a_claim_retires_the_links_of_the_address_it_replaces() {
        let store = store();
        store.claim("ada", "ada@example.com", 1_000).unwrap();
        let link = store
            .issue(LinkPurpose::Verify, "ada", "ada@example.com", 1_000)
            .unwrap();
        store.claim("ada", "ada@elsewhere.com", 2_000).unwrap();
        assert_eq!(
            store
                .spend(LinkPurpose::Verify, &link.secret, 3_000)
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_link_is_spendable_once() {
        let store = store();
        store.claim("ada", "ada@example.com", 1_000).unwrap();
        let link = store
            .issue(LinkPurpose::Reset, "ada", "ada@example.com", 1_000)
            .unwrap();
        let spent = store
            .spend(LinkPurpose::Reset, &link.secret, 2_000)
            .unwrap()
            .expect("the first spend succeeds");
        assert_eq!(spent.login, "ada");
        assert_eq!(spent.address, "ada@example.com");
        assert_eq!(
            store
                .spend(LinkPurpose::Reset, &link.secret, 3_000)
                .unwrap(),
            None,
            "a replay must find nothing"
        );
    }

    #[test]
    fn an_expired_link_and_a_guess_refuse_the_same_way() {
        let store = store();
        store.claim("ada", "ada@example.com", 1_000).unwrap();
        let link = store
            .issue(LinkPurpose::Reset, "ada", "ada@example.com", 1_000)
            .unwrap();
        assert_eq!(
            store
                .spend(LinkPurpose::Reset, &link.secret, 1_000 + RESET_TTL_MS + 1)
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .spend(LinkPurpose::Reset, &random_secret(), 2_000)
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_verification_link_cannot_be_spent_as_a_reset() {
        let store = store();
        store.claim("ada", "ada@example.com", 1_000).unwrap();
        let link = store
            .issue(LinkPurpose::Verify, "ada", "ada@example.com", 1_000)
            .unwrap();
        assert_eq!(
            store
                .spend(LinkPurpose::Reset, &link.secret, 2_000)
                .unwrap(),
            None
        );
        assert!(
            store
                .spend(LinkPurpose::Verify, &link.secret, 2_000)
                .unwrap()
                .is_some()
        );
    }

    /// Asking twice must leave only the newer link spendable.
    #[test]
    fn issuing_again_retires_the_previous_link_of_that_purpose() {
        let store = store();
        store.claim("ada", "ada@example.com", 1_000).unwrap();
        let first = store
            .issue(LinkPurpose::Reset, "ada", "ada@example.com", 1_000)
            .unwrap();
        let second = store
            .issue(LinkPurpose::Reset, "ada", "ada@example.com", 2_000)
            .unwrap();
        assert_eq!(
            store
                .spend(LinkPurpose::Reset, &first.secret, 3_000)
                .unwrap(),
            None
        );
        assert!(
            store
                .spend(LinkPurpose::Reset, &second.secret, 3_000)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn the_durable_send_cap_counts_what_was_issued_in_the_window() {
        let store = store();
        store.claim("ada", "ada@example.com", 1_000).unwrap();
        for at in [1_000, 2_000, 3_000] {
            store
                .issue(LinkPurpose::Reset, "ada", "ada@example.com", at)
                .unwrap();
        }
        assert_eq!(
            store.issued_since(LinkPurpose::Reset, "ada", 0).unwrap(),
            3,
            "every issue is counted, not only the spendable one"
        );
        assert_eq!(
            store
                .issued_since(LinkPurpose::Reset, "ada", 2_500)
                .unwrap(),
            1
        );
        assert_eq!(
            store.issued_since(LinkPurpose::Verify, "ada", 0).unwrap(),
            0
        );
    }

    #[test]
    fn dropping_outstanding_links_leaves_nothing_spendable() {
        let store = store();
        store.claim("ada", "ada@example.com", 1_000).unwrap();
        let link = store
            .issue(LinkPurpose::Verify, "ada", "ada@example.com", 1_000)
            .unwrap();
        store.drop_outstanding("ada", 2_000).unwrap();
        assert_eq!(
            store
                .spend(LinkPurpose::Verify, &link.secret, 2_000)
                .unwrap(),
            None
        );
    }

    /// The database must hold no spendable secret.
    #[test]
    fn the_stored_row_holds_only_a_digest_of_the_link() {
        let store = store();
        store.claim("ada", "ada@example.com", 1_000).unwrap();
        let link = store
            .issue(LinkPurpose::Reset, "ada", "ada@example.com", 1_000)
            .unwrap();
        let stored: String = store
            .conn()
            .query_row(
                "SELECT token_hash FROM account_email_tokens WHERE login = 'ada'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(stored, link.secret);
        assert_eq!(stored, digest(&link.secret));
        assert!(format!("{link:?}").contains("[REDACTED]"));
    }

    /// A limiter map that printed addresses would be an address directory.
    #[test]
    fn the_rate_limit_key_holds_a_digest_rather_than_the_address() {
        let key = address_key("ada@example.com");
        assert_eq!(key.len(), 32);
        assert!(!key.contains("ada"));
        assert_eq!(key, address_key("ada@example.com"));
        assert_ne!(key, address_key("bob@example.com"));
    }
}
