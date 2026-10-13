//! `/api/v1/auth/email*` and `/api/v1/auth/password/{forgot,reset}` through
//! the router.
//!
//! The proofs that matter here are the negative ones: the reset answer is the
//! same bytes whether or not an account holds the address, an unproved
//! address recovers nothing, and an expired or replayed link fails without
//! changing a password. Sign-in for an account that has set no address is
//! untouched, which is what every account looked like before this surface
//! existed.

use super::*;
use crate::web::account_email::{AccountEmailStore, LinkPurpose};
use crate::web::mail::Mailer;
use axum::body::Body;
use axum::http::{HeaderMap, Method as HttpMethod, Request, StatusCode, header};
use jeryu_core::ForgeCore;
use tower::ServiceExt;

const PASSWORD: &str = "first-password-1";
const NEW_PASSWORD: &str = "second-password-2";

pub(super) struct Forge {
    pub(super) router: axum::Router,
    pub(super) store: AccountEmailStore,
    pub(super) mailer: Mailer,
    pub(super) core: ForgeCore,
    /// A personal access token for `ada`, the account under test.
    pub(super) ada: String,
}

pub(super) fn forge() -> Forge {
    forge_with_mailer(Mailer::recording())
}

pub(super) fn forge_with_mailer(mailer: Mailer) -> Forge {
    forge_with(mailer, ForgeCore::new())
}

/// A forge whose core is backed by SQLite, which is what gives it an audit
/// trail: the in-memory core keeps none.
pub(super) fn audited_forge(mailer: Mailer) -> (Forge, tempfile::TempDir) {
    let dir = tempdir().expect("forge dir");
    let core = ForgeCore::open_sqlite(dir.path().join("forge.sqlite")).expect("open the forge");
    (forge_with(mailer, core), dir)
}

fn forge_with(mailer: Mailer, core: ForgeCore) -> Forge {
    core.create_account("ada", PASSWORD, UserRole::User)
        .expect("create the account under test");
    let ada = core
        .create_personal_access_token("ada", "t", None)
        .expect("issue a token for the account under test")
        .secret;
    let mut state = WebState::new(core.clone()).with_auth(true, false, false);
    state.mailer = mailer.clone();
    let store = state.account_email.clone();
    let router = app(state, Path::new("/tmp/jeryu-no-spa"));
    Forge {
        router,
        store,
        mailer,
        core,
        ada,
    }
}

pub(super) async fn send(
    router: &axum::Router,
    method: HttpMethod,
    uri: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .expect("build the request");
    let response = router.clone().oneshot(request).await.expect("route");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read the body")
        .to_vec();
    (status, headers, bytes)
}

pub(super) async fn json_reply(
    router: &axum::Router,
    method: HttpMethod,
    uri: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let (status, _, bytes) = send(router, method, uri, token, body).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The `token=` query value of the one link in the newest message.
pub(super) fn newest_link_secret(mailer: &Mailer) -> String {
    let (_, rendered) = mailer
        .sent()
        .pop()
        .expect("a message was handed to the transport");
    let start = rendered.find("token=").expect("the message carries a link") + "token=".len();
    rendered[start..]
        .split(|ch: char| !ch.is_ascii_hexdigit())
        .next()
        .expect("the link carries a secret")
        .to_string()
}

/// Claim an address and follow its verification link, as a person does.
pub(super) async fn verify_address(forge: &Forge, address: &str) {
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::PUT,
        "/api/v1/auth/email",
        Some(&forge.ada),
        Some(&json!({ "address": address })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let secret = newest_link_secret(&forge.mailer);
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/email/verify",
        None,
        Some(&json!({ "token": secret })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "result": "verified", "verified": true }));
}

/// Acceptance 1 and 2, end to end: claim, verify, recover, sign in again.
#[tokio::test]
async fn a_person_verifies_an_address_then_recovers_with_it() {
    let forge = forge();

    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::GET,
        "/api/v1/auth/email",
        Some(&forge.ada),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "address": null, "verified": false }),
        "{body}"
    );

    verify_address(&forge, "Ada@Example.com").await;
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::GET,
        "/api/v1/auth/email",
        Some(&forge.ada),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "address": "ada@example.com", "verified": true }),
        "the verified state is persisted and read back: {body}"
    );

    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/forgot",
        None,
        Some(&json!({ "email": "ADA@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let sent = forge.mailer.sent();
    let (to, rendered) = sent.last().expect("a reset message was sent");
    assert_eq!(to, "ada@example.com");
    assert!(
        rendered.contains("Subject: Reset your jeryu password"),
        "{rendered}"
    );

    let secret = newest_link_secret(&forge.mailer);
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/reset",
        None,
        Some(&json!({ "token": secret, "newPassword": NEW_PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "result": "reset" }), "{body}");

    let signed_in = forge
        .core
        .authenticate_password("ada", NEW_PASSWORD)
        .expect("the new password signs in");
    assert!(
        !signed_in.must_change_password,
        "a person who just chose a password must not be asked to choose another"
    );
    assert!(
        forge.core.authenticate_password("ada", PASSWORD).is_err(),
        "the old password must stop working"
    );
}

/// Acceptance 1: a reset link works once and the second attempt fails safely.
#[tokio::test]
async fn a_replayed_reset_link_fails_and_leaves_the_password_alone() {
    let forge = forge();
    verify_address(&forge, "ada@example.com").await;
    json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/forgot",
        None,
        Some(&json!({ "email": "ada@example.com" })),
    )
    .await;
    let secret = newest_link_secret(&forge.mailer);
    let (status, _) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/reset",
        None,
        Some(&json!({ "token": &secret, "newPassword": NEW_PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/reset",
        None,
        Some(&json!({ "token": &secret, "newPassword": "third-password-33" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "reset_link_invalid", "{body}");
    forge
        .core
        .authenticate_password("ada", NEW_PASSWORD)
        .expect("the replay changed nothing");
}

/// Acceptance 1: an expired link, a link from a secret nobody issued, and a
/// verification link offered as a reset are one refusal.
#[tokio::test]
async fn an_expired_or_unissued_or_wrong_purpose_link_refuses_the_same_way() {
    let forge = forge();
    verify_address(&forge, "ada@example.com").await;

    // Expired: issued directly into the store, far enough in the past that
    // its hour is spent.
    let long_ago = chrono::Utc::now().timestamp_millis() - 7 * 60 * 60 * 1000;
    let expired = forge
        .store
        .issue(LinkPurpose::Reset, "ada", "ada@example.com", long_ago)
        .expect("issue a link in the past");
    let verification = forge
        .store
        .issue(LinkPurpose::Verify, "ada", "ada@example.com", long_ago)
        .expect("issue a verification link in the past");

    for token in [
        expired.secret_for_test(),
        verification.secret_for_test(),
        "0".repeat(64),
        "not-a-token".to_string(),
    ] {
        let (status, body) = json_reply(
            &forge.router,
            HttpMethod::POST,
            "/api/v1/auth/password/reset",
            None,
            Some(&json!({ "token": token, "newPassword": NEW_PASSWORD })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], "reset_link_invalid", "{body}");
    }
    forge
        .core
        .authenticate_password("ada", PASSWORD)
        .expect("no refused link changed the password");
}

/// Acceptance 2: a verification link is single use, and one issued for an
/// address the account has since replaced proves nothing.
#[tokio::test]
async fn a_verification_link_is_single_use_and_bound_to_its_address() {
    let forge = forge();
    let (status, _) = json_reply(
        &forge.router,
        HttpMethod::PUT,
        "/api/v1/auth/email",
        Some(&forge.ada),
        Some(&json!({ "address": "ada@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let first = newest_link_secret(&forge.mailer);

    // The person corrects the address before following the first link.
    let (status, _) = json_reply(
        &forge.router,
        HttpMethod::PUT,
        "/api/v1/auth/email",
        Some(&forge.ada),
        Some(&json!({ "address": "ada@elsewhere.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let second = newest_link_secret(&forge.mailer);

    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/email/verify",
        None,
        Some(&json!({ "token": first })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "verification_link_invalid", "{body}");

    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/email/verify",
        None,
        Some(&json!({ "token": &second })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/email/verify",
        None,
        Some(&json!({ "token": &second })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "verification_link_invalid", "{body}");

    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::GET,
        "/api/v1/auth/email",
        Some(&forge.ada),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "address": "ada@elsewhere.com", "verified": true }),
        "{body}"
    );
}

/// Acceptance 3: the answer is the same bytes for an address that recovers,
/// an address nobody holds, and an address an account holds but has not
/// proved. The unproved case also sends nothing.
#[tokio::test]
async fn the_reset_answer_discloses_no_account_and_an_unproved_address_sends_nothing() {
    let forge = forge();
    let (status, _) = json_reply(
        &forge.router,
        HttpMethod::PUT,
        "/api/v1/auth/email",
        Some(&forge.ada),
        Some(&json!({ "address": "ada@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let after_claim = forge.mailer.sent().len();

    let mut replies = Vec::new();
    for address in ["ada@example.com", "nobody@example.com", "not-an-address"] {
        let (status, mut headers, bytes) = send(
            &forge.router,
            HttpMethod::POST,
            "/api/v1/auth/password/forgot",
            None,
            Some(&json!({ "email": address })),
        )
        .await;
        // The edge mints a fresh request id per request; everything else must
        // match, so it is the one header dropped before comparing.
        headers.remove("x-request-id");
        let mut named: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        named.sort();
        replies.push((status, named, bytes));
    }
    assert_eq!(replies[0].0, StatusCode::ACCEPTED, "{:?}", replies[0]);
    assert_eq!(
        String::from_utf8(replies[0].2.clone()).expect("utf-8"),
        r#"{"result":"sent"}"#
    );
    assert_eq!(
        replies[1], replies[0],
        "an unheld address answered differently"
    );
    assert_eq!(
        replies[2], replies[0],
        "a malformed address answered differently"
    );
    assert_eq!(
        forge.mailer.sent().len(),
        after_claim,
        "an unproved address must not be sent a reset link"
    );
}

/// Acceptance 3: the reset request is rate limited per client address, and
/// the refusal is the ordinary 429 with its budget headers.
#[tokio::test]
async fn reset_requests_are_rate_limited_per_client() {
    let forge = forge();
    verify_address(&forge, "ada@example.com").await;
    let mut statuses = Vec::new();
    for _ in 0..8 {
        let (status, headers, _) = send(
            &forge.router,
            HttpMethod::POST,
            "/api/v1/auth/password/forgot",
            None,
            Some(&json!({ "email": "ada@example.com" })),
        )
        .await;
        statuses.push((status, headers));
    }
    let (limited, headers) = statuses
        .iter()
        .find(|(status, _)| *status == StatusCode::TOO_MANY_REQUESTS)
        .expect("the limiter closed the window");
    assert_eq!(*limited, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key(header::RETRY_AFTER), "{headers:?}");
    assert!(
        forge.mailer.sent().len() < 8,
        "the limiter must stop messages, not only answers"
    );
}

/// Acceptance 3: spending a link is limited too, so a secret cannot be
/// guessed at speed.
#[tokio::test]
async fn spending_a_link_is_rate_limited_per_client() {
    let forge = forge();
    let mut limited = false;
    for _ in 0..12 {
        let (status, _) = json_reply(
            &forge.router,
            HttpMethod::POST,
            "/api/v1/auth/email/verify",
            None,
            Some(&json!({ "token": "0".repeat(64) })),
        )
        .await;
        limited |= status == StatusCode::TOO_MANY_REQUESTS;
    }
    assert!(limited, "guessing a verification link was never limited");
}

/// A password the forge would refuse must not burn the one link.
#[tokio::test]
async fn a_refused_password_leaves_the_link_spendable() {
    let forge = forge();
    verify_address(&forge, "ada@example.com").await;
    json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/forgot",
        None,
        Some(&json!({ "email": "ada@example.com" })),
    )
    .await;
    let secret = newest_link_secret(&forge.mailer);
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/reset",
        None,
        Some(&json!({ "token": &secret, "newPassword": "short" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "invalid_input", "{body}");
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/reset",
        None,
        Some(&json!({ "token": &secret, "newPassword": NEW_PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A reset drops the account's sessions and tokens, as core's administrative
/// reset does, so a stolen session does not survive the recovery.
#[tokio::test]
async fn a_reset_retires_the_accounts_sessions_and_tokens() {
    let forge = forge();
    verify_address(&forge, "ada@example.com").await;
    json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/forgot",
        None,
        Some(&json!({ "email": "ada@example.com" })),
    )
    .await;
    let secret = newest_link_secret(&forge.mailer);
    let (status, _) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/reset",
        None,
        Some(&json!({ "token": secret, "newPassword": NEW_PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::GET,
        "/api/v1/auth/email",
        Some(&forge.ada),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

/// Acceptance 3: the recovery routes answer without a credential, and the
/// account routes do not.
#[tokio::test]
async fn the_account_routes_need_the_account_and_the_link_routes_do_not() {
    let forge = forge();
    for (method, uri) in [
        (HttpMethod::GET, "/api/v1/auth/email"),
        (HttpMethod::POST, "/api/v1/auth/email/verify/resend"),
    ] {
        let (status, body) = json_reply(
            &forge.router,
            method,
            uri,
            None,
            Some(&json!({ "address": "ada@example.com" })),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: {body}");
    }
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::PUT,
        "/api/v1/auth/email",
        None,
        Some(&json!({ "address": "ada@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

/// An address another account already proved cannot be claimed away from it.
#[tokio::test]
async fn one_address_belongs_to_one_account() {
    let forge = forge();
    verify_address(&forge, "ada@example.com").await;
    forge
        .core
        .create_account("bob", PASSWORD, UserRole::User)
        .expect("a second account");
    let bob = forge
        .core
        .create_personal_access_token("bob", "t", None)
        .expect("a token for the second account")
        .secret;
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::PUT,
        "/api/v1/auth/email",
        Some(&bob),
        Some(&json!({ "address": "ada@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "conflict", "{body}");
    assert_eq!(
        forge
            .store
            .verified_holder("ada@example.com")
            .expect("read the holder")
            .as_deref(),
        Some("ada")
    );
}

/// Acceptance 5: the migration backfills nothing, so an account that existed
/// before this surface signs in exactly as it did and simply cannot recover
/// until it proves an address.
#[tokio::test]
async fn an_account_from_before_this_surface_signs_in_and_cannot_recover() {
    let forge = forge();
    forge
        .core
        .authenticate_password("ada", PASSWORD)
        .expect("sign-in is unchanged for an account with no address");
    assert_eq!(
        forge.store.get("ada").expect("read the account address"),
        None,
        "no backfill may invent an address"
    );
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/forgot",
        None,
        Some(&json!({ "email": "ada@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(
        forge.mailer.sent().is_empty(),
        "nothing may be sent for an account with no address"
    );
}

/// An operator who has not wired mail yet must not be told apart from one who
/// has: the answer is identical and the refusal goes to the audit log.
#[tokio::test]
async fn an_unconfigured_mailer_answers_exactly_as_a_configured_one() {
    let (forge, _dir) = audited_forge(Mailer::unconfigured());
    let now_ms = chrono::Utc::now().timestamp_millis();
    forge
        .store
        .claim("ada", "ada@example.com", now_ms)
        .expect("claim the address");
    forge
        .store
        .mark_verified("ada", "ada@example.com", now_ms)
        .expect("prove the address");
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/forgot",
        None,
        Some(&json!({ "email": "ada@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body, json!({ "result": "sent" }), "{body}");
    let records = forge
        .core
        .list_audit("account_email")
        .expect("read the audit trail");
    assert!(
        records.iter().any(|record| {
            record.action == "account.password_reset.requested" && record.phase == "failed"
        }),
        "the unsent message must be recorded: {records:?}"
    );
}

/// The resend path refuses an account with no address, and stops once the
/// account has been sent its hourly allowance.
#[tokio::test]
async fn resending_needs_an_address_and_stops_at_the_hourly_cap() {
    let forge = forge();
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/email/verify/resend",
        Some(&forge.ada),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "conflict", "{body}");

    let (status, _) = json_reply(
        &forge.router,
        HttpMethod::PUT,
        "/api/v1/auth/email",
        Some(&forge.ada),
        Some(&json!({ "address": "ada@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let mut capped = false;
    for _ in 0..8 {
        let (status, _) = json_reply(
            &forge.router,
            HttpMethod::POST,
            "/api/v1/auth/email/verify/resend",
            Some(&forge.ada),
            None,
        )
        .await;
        capped |= status == StatusCode::TOO_MANY_REQUESTS;
    }
    assert!(capped, "the per-account send cap never closed");

    // Once proved, a resend says so instead of sending again.
    let now_ms = chrono::Utc::now().timestamp_millis();
    forge
        .store
        .mark_verified("ada", "ada@example.com", now_ms)
        .expect("prove the address");
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/email/verify/resend",
        Some(&forge.ada),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "result": "verified", "verified": true }));
}

/// Saving the address the account already proved keeps it proved.
#[tokio::test]
async fn re_saving_a_proved_address_keeps_it_proved() {
    let forge = forge();
    verify_address(&forge, "ada@example.com").await;
    let before = forge.mailer.sent().len();
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::PUT,
        "/api/v1/auth/email",
        Some(&forge.ada),
        Some(&json!({ "address": "ADA@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "result": "verified", "verified": true }));
    assert_eq!(
        forge.mailer.sent().len(),
        before,
        "re-saving the same proved address sends nothing"
    );
    assert_eq!(
        forge
            .store
            .verified_holder("ada@example.com")
            .expect("read the holder")
            .as_deref(),
        Some("ada")
    );
}

/// A malformed address is refused on the account's own route, where there is
/// nothing to disclose.
#[tokio::test]
async fn claiming_a_malformed_address_is_refused() {
    let forge = forge();
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::PUT,
        "/api/v1/auth/email",
        Some(&forge.ada),
        Some(&json!({ "address": "not-an-address" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "invalid_input", "{body}");
    assert_eq!(forge.store.get("ada").expect("read the address"), None);
    assert!(forge.mailer.sent().is_empty());
}

/// No answer and no audit record may carry a spendable secret.
#[tokio::test]
async fn no_answer_or_audit_record_carries_the_link() {
    let (forge, _dir) = audited_forge(Mailer::recording());
    verify_address(&forge, "ada@example.com").await;
    let (_, _, bytes) = send(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/forgot",
        None,
        Some(&json!({ "email": "ada@example.com" })),
    )
    .await;
    let secret = newest_link_secret(&forge.mailer);
    assert_eq!(secret.len(), 64);
    let answered = String::from_utf8(bytes).expect("utf-8 body");
    assert!(!answered.contains(&secret), "{answered}");
    let records = format!(
        "{:?}",
        forge
            .core
            .list_audit("account_email")
            .expect("read the audit trail")
    );
    assert!(!records.contains(&secret), "a link reached the audit log");
    assert!(
        !records.contains("ada@example.com"),
        "an address reached the audit log: {records}"
    );
}
