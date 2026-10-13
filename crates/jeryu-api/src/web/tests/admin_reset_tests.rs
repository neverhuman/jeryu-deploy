//! `POST /api/v1/admin/users/:login/reset-password` and the address columns
//! of `GET /api/v1/admin/users` through the router.
//!
//! The proofs that matter: the temporary password goes to the person's
//! mailbox and never into the answer, nothing changes when there is nowhere
//! to send it, and only a mailed password that is used and replaced proves
//! the address.

use super::account_email_tests::{
    Forge, audited_forge, forge, forge_with_mailer, json_reply, verify_address,
};
use super::*;
use crate::web::mail::Mailer;
use axum::body::Body;
use axum::http::{Method as HttpMethod, Request, StatusCode, header};
use tower::ServiceExt;

const ADA_PASSWORD: &str = "first-password-1";
const CHOSEN_PASSWORD: &str = "chosen-password-3";

/// An administrator's token on `forge`.
fn admin_token(forge: &Forge) -> String {
    forge
        .core
        .create_account("root", "root-password-1", UserRole::Admin)
        .expect("create the administrator");
    forge
        .core
        .create_personal_access_token("root", "t", None)
        .expect("issue the administrator's token")
        .secret
}

async fn reset(forge: &Forge, admin: &str, body: Option<&Value>) -> (StatusCode, Value) {
    json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/admin/users/ada/reset-password",
        Some(admin),
        body,
    )
    .await
}

async fn users(forge: &Forge, admin: &str) -> Value {
    let (status, body) = json_reply(
        &forge.router,
        HttpMethod::GET,
        "/api/v1/admin/users",
        Some(admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array()
        .expect("a user list")
        .iter()
        .find(|row| row["login"] == "ada")
        .cloned()
        .expect("ada is listed")
}

/// The temporary password in the newest message.
fn mailed_password(mailer: &Mailer) -> String {
    let (_, rendered) = mailer.sent().pop().expect("a message was sent");
    let start = rendered
        .find("Temporary password: ")
        .expect("the message carries the password")
        + "Temporary password: ".len();
    rendered[start..]
        .lines()
        .next()
        .expect("the password line")
        .trim()
        .to_string()
}

/// Sign in with a password; `(status, cookie, body)`.
async fn sign_in(forge: &Forge, password: &str) -> (StatusCode, String, Value) {
    let response = forge
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "login": "ada", "password": password }).to_string(),
                ))
                .expect("build the request"),
        )
        .await
        .expect("route");
    let status = response.status();
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .to_string();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read the body");
    (
        status,
        cookie,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Sign in with `current` and replace it with [`CHOSEN_PASSWORD`].
async fn change_password(forge: &Forge, current: &str) {
    let (status, cookie, body) = sign_in(forge, current).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let csrf = body["csrfToken"]
        .as_str()
        .expect("a CSRF token")
        .to_string();
    let response = forge
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method(HttpMethod::POST)
                .uri("/api/v1/auth/password")
                .header(header::COOKIE, cookie)
                .header("x-jeryu-csrf", csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "currentPassword": current, "newPassword": CHOSEN_PASSWORD })
                        .to_string(),
                ))
                .expect("build the request"),
        )
        .await
        .expect("route");
    assert_eq!(response.status(), StatusCode::OK);
}

/// The owner's flow: an account with no address, the administrator supplies
/// one, the person gets the password by mail, must change it, and changing it
/// proves the address.
#[tokio::test]
async fn a_mailed_reset_reaches_the_person_and_the_forced_change_proves_the_address() {
    let forge = forge();
    let admin = admin_token(&forge);
    let row = users(&forge, &admin).await;
    assert_eq!(row["email"], Value::Null, "{row}");
    assert_eq!(row["emailVerified"], false, "{row}");

    let (status, body) = reset(&forge, &admin, Some(&json!({ "email": "Ada@Example.com" }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        body,
        json!({ "result": "sent", "login": "ada", "email": "ada@example.com" }),
        "the answer carries no password: {body}"
    );
    let sent = forge.mailer.sent();
    let (to, rendered) = sent.last().expect("the reset was mailed");
    assert_eq!(to, "ada@example.com");
    assert!(
        rendered.contains("Subject: Your jeryu password was reset"),
        "{rendered}"
    );
    assert!(rendered.contains("Login: ada"), "{rendered}");
    assert!(rendered.contains("https://jeryu.test/"), "{rendered}");
    let temporary = mailed_password(&forge.mailer);
    assert!(!body.to_string().contains(&temporary));

    let row = users(&forge, &admin).await;
    assert_eq!(row["email"], "ada@example.com", "{row}");
    assert_eq!(row["emailVerified"], false, "not proved until it is used");

    let (status, _, _) = sign_in(&forge, ADA_PASSWORD).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the old password is gone");
    let (status, _, body) = sign_in(&forge, &temporary).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mustChangePassword"], true, "{body}");

    change_password(&forge, &temporary).await;
    let row = users(&forge, &admin).await;
    assert_eq!(row["emailVerified"], true, "{row}");
    let (status, _, body) = sign_in(&forge, CHOSEN_PASSWORD).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mustChangePassword"], false, "{body}");
}

/// With no address anywhere the reset is refused before anything changes.
#[tokio::test]
async fn an_account_without_an_address_needs_one_supplied() {
    let forge = forge();
    let admin = admin_token(&forge);
    for body in [None, Some(json!({}))] {
        let (status, reply) = reset(&forge, &admin, body.as_ref()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
        assert_eq!(reply["code"], "email_required", "{reply}");
    }
    assert!(forge.mailer.sent().is_empty());
    let (status, _, _) = sign_in(&forge, ADA_PASSWORD).await;
    assert_eq!(status, StatusCode::OK, "the password is unchanged");
}

/// A proved address is where the reset goes; another one is refused.
#[tokio::test]
async fn a_proved_address_wins_over_a_different_supplied_one() {
    let forge = forge();
    let admin = admin_token(&forge);
    verify_address(&forge, "ada@example.com").await;
    let (status, reply) = reset(
        &forge,
        &admin,
        Some(&json!({ "email": "other@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert_eq!(reply["code"], "conflict", "{reply}");
    assert_eq!(
        reply["message"], "the account already has a verified address",
        "{reply}"
    );
    let (status, reply) = reset(&forge, &admin, None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{reply}");
    assert_eq!(reply["email"], "ada@example.com");
}

/// The stored unproved address is used when none is supplied.
#[tokio::test]
async fn a_stored_unproved_address_is_used() {
    let forge = forge();
    let admin = admin_token(&forge);
    let now_ms = chrono::Utc::now().timestamp_millis();
    forge
        .store
        .claim("ada", "ada@example.com", now_ms)
        .expect("claim the address");
    let (status, reply) = reset(&forge, &admin, Some(&json!({}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{reply}");
    assert_eq!(reply["email"], "ada@example.com");
}

#[tokio::test]
async fn an_address_held_by_another_account_or_malformed_is_refused() {
    let forge = forge();
    let admin = admin_token(&forge);
    let now_ms = chrono::Utc::now().timestamp_millis();
    forge
        .store
        .claim("root", "taken@example.com", now_ms)
        .expect("claim the address for another account");
    let (status, reply) = reset(
        &forge,
        &admin,
        Some(&json!({ "email": "taken@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert_eq!(reply["code"], "conflict");
    let (status, reply) = reset(&forge, &admin, Some(&json!({ "email": "nope" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
    assert_eq!(reply["code"], "invalid_input");
    assert!(forge.mailer.sent().is_empty());
}

/// Without mail the reset still happens and the administrator is shown the
/// one-time password to hand over: the account must change it, and nothing
/// was mailed, so changing it proves no address.
#[tokio::test]
async fn an_unconfigured_mailer_shows_the_password_to_the_administrator() {
    let forge = forge_with_mailer(Mailer::unconfigured());
    let admin = admin_token(&forge);
    // No address is needed without mail.
    let (status, reply) = reset(&forge, &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["result"], "shown", "{reply}");
    assert_eq!(reply["login"], "ada", "{reply}");
    assert_eq!(reply["email"], Value::Null, "{reply}");
    assert!(reply.get("mailError").is_none(), "{reply}");
    let password = reply["password"]
        .as_str()
        .expect("the password is shown")
        .to_string();

    // A supplied address is stored unproved.
    let (status, reply) = reset(&forge, &admin, Some(&json!({ "email": "ada@example.com" }))).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["email"], "ada@example.com", "{reply}");
    let password_again = reply["password"].as_str().expect("the password is shown");
    assert_ne!(
        password_again, password,
        "each reset issues a fresh password"
    );
    let (status, _, _) = sign_in(&forge, &password).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the first one was replaced"
    );
    let (status, _, body) = sign_in(&forge, password_again).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mustChangePassword"], true, "{body}");

    change_password(&forge, password_again).await;
    let stored = forge
        .store
        .get("ada")
        .expect("read the address")
        .expect("the address is stored");
    assert_eq!(stored.address, "ada@example.com");
    assert!(
        !stored.is_verified(),
        "nothing was mailed, so nothing is proved"
    );
    assert!(forge.mailer.sent().is_empty());
}

#[tokio::test]
async fn only_an_administrator_resets() {
    let forge = forge();
    let (status, reply) = reset(&forge, &forge.ada, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{reply}");
    assert_eq!(reply["code"], "permission_denied");
}

/// A password change that did not replace a mailed password proves nothing.
#[tokio::test]
async fn an_ordinary_password_change_proves_nothing() {
    let forge = forge();
    let now_ms = chrono::Utc::now().timestamp_millis();
    forge
        .store
        .claim("ada", "ada@example.com", now_ms)
        .expect("claim the address");
    change_password(&forge, ADA_PASSWORD).await;
    let stored = forge
        .store
        .get("ada")
        .expect("read the address")
        .expect("the address is stored");
    assert!(!stored.is_verified());
}

/// Claiming another address after the mailed reset clears the marker, so the
/// forced change does not prove the new address.
#[tokio::test]
async fn a_new_address_after_the_reset_is_not_proved_by_it() {
    let forge = forge();
    let admin = admin_token(&forge);
    let (status, _) = reset(&forge, &admin, Some(&json!({ "email": "ada@example.com" }))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let temporary = mailed_password(&forge.mailer);
    let now_ms = chrono::Utc::now().timestamp_millis();
    forge
        .store
        .claim("ada", "elsewhere@example.com", now_ms)
        .expect("claim another address");
    change_password(&forge, &temporary).await;
    let stored = forge
        .store
        .get("ada")
        .expect("read the address")
        .expect("the address is stored");
    assert_eq!(stored.address, "elsewhere@example.com");
    assert!(!stored.is_verified());
}

/// A message that did not leave: the reset stands and the administrator is
/// shown the password with the reason, and the password proves nothing if it
/// is ever used.
#[tokio::test]
async fn a_failed_send_shows_the_password_and_proves_nothing() {
    let forge = forge_with_mailer(Mailer::failing());
    let admin = admin_token(&forge);
    let (status, reply) = reset(&forge, &admin, Some(&json!({ "email": "ada@example.com" }))).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["result"], "shown", "{reply}");
    assert_eq!(reply["email"], "ada@example.com", "{reply}");
    assert_eq!(reply["mailError"], "could not send the message", "{reply}");
    let password = reply["password"].as_str().expect("the password is shown");
    let (status, _, body) = sign_in(&forge, password).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mustChangePassword"], true, "{body}");
    assert!(
        !forge
            .store
            .prove_by_mailed_reset("ada", 1)
            .expect("read the marker"),
        "no marker was recorded for an unsent password"
    );
}

/// A message that left is the only answer without the password, and the audit
/// trail tells a mailed reset from a shown one without carrying the secret.
#[tokio::test]
async fn a_mailed_reset_answers_without_the_password_and_is_audited_apart() {
    let (forge, _dir) = audited_forge(Mailer::recording());
    let admin = admin_token(&forge);
    let (status, reply) = reset(&forge, &admin, Some(&json!({ "email": "ada@example.com" }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{reply}");
    assert_eq!(reply["result"], "sent", "{reply}");
    assert!(reply.get("password").is_none(), "{reply}");
    let mailed = mailed_password(&forge.mailer);
    let records = forge
        .core
        .list_audit("account_email")
        .expect("read the audit trail");
    assert!(
        records.iter().any(|record| {
            record.action == "account.password_reset.admin_mailed" && record.phase == "completed"
        }),
        "{records:?}"
    );
    assert!(
        !records
            .iter()
            .any(|record| record.action == "account.password_reset.admin_shown"),
        "{records:?}"
    );
    let rendered = format!("{records:?}");
    assert!(!rendered.contains(&mailed), "{rendered}");
    assert!(!rendered.contains("ada@example.com"), "{rendered}");
}
