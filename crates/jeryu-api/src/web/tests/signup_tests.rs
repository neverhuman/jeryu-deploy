//! `/api/v1/auth/signup/start` and `/api/v1/auth/signup` through the router.
//!
//! The proofs that matter: no account is created without a spent link, the
//! start answer is the same whether or not an account holds the address, a
//! refused login or password does not cost the person their link, and the
//! account that comes out holds its address already proved.

use super::account_email_tests::{
    Forge, audited_forge, forge, forge_with_mailer, json_reply, newest_link_secret, send,
    verify_address,
};
use super::*;
use crate::web::mail::Mailer;
use axum::http::{Method as HttpMethod, StatusCode, header};

const SIGNUP_PASSWORD: &str = "signup-password-1";

async fn start(forge: &Forge, email: &str) -> (StatusCode, Value) {
    json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/signup/start",
        None,
        Some(&json!({ "email": email })),
    )
    .await
}

async fn signup(forge: &Forge, body: &Value) -> (StatusCode, Value) {
    json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/signup",
        None,
        Some(body),
    )
    .await
}

/// Start, read the mailed link, and return its secret.
async fn mailed_signup_secret(forge: &Forge, email: &str) -> String {
    let (status, body) = start(forge, email).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body, json!({ "result": "sent" }));
    newest_link_secret(&forge.mailer)
}

/// The whole flow: start, follow the link, sign up, and the account can then
/// recover its password with the address it proved.
#[tokio::test]
async fn start_then_signup_creates_an_account_with_a_proved_address() {
    let forge = forge();
    let (status, body) = start(&forge, "Grace@Example.com").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let sent = forge.mailer.sent();
    let (to, rendered) = sent.last().expect("a sign-up message was sent");
    assert_eq!(to, "grace@example.com");
    assert!(
        rendered.contains("Subject: Finish creating your jeryu account"),
        "{rendered}"
    );
    assert!(
        rendered.contains("https://jeryu.test/create-account?token="),
        "{rendered}"
    );
    let secret = newest_link_secret(&forge.mailer);

    let (status, headers, bytes) = send(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/signup",
        None,
        Some(&json!({ "token": secret, "login": "grace", "password": SIGNUP_PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers.contains_key(header::SET_COOKIE),
        "sign-up opens a session"
    );
    let body: Value = serde_json::from_slice(&bytes).expect("a JSON body");
    assert_eq!(body["login"], "grace");
    assert_eq!(body["role"], "user");
    assert!(body["csrfToken"].as_str().is_some(), "{body}");

    let stored = forge
        .store
        .get("grace")
        .expect("read the address")
        .expect("the address is stored");
    assert_eq!(stored.address, "grace@example.com");
    assert!(stored.is_verified(), "the address arrives proved");

    let before = forge.mailer.sent().len();
    let (status, _) = json_reply(
        &forge.router,
        HttpMethod::POST,
        "/api/v1/auth/password/forgot",
        None,
        Some(&json!({ "email": "grace@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let sent = forge.mailer.sent();
    assert_eq!(sent.len(), before + 1, "the proved address can recover");
    assert!(
        sent[before]
            .1
            .contains("Subject: Reset your jeryu password")
    );
}

/// A bare login and password, which is what sign-up used to take, is refused
/// with the code that names the repair.
#[tokio::test]
async fn signup_without_a_link_is_refused_with_email_required() {
    let forge = forge();
    for body in [
        json!({ "login": "grace", "password": SIGNUP_PASSWORD }),
        json!({ "token": "  ", "login": "grace", "password": SIGNUP_PASSWORD }),
    ] {
        let (status, reply) = signup(&forge, &body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
        assert_eq!(reply["code"], "email_required", "{reply}");
        assert!(
            reply["message"]
                .as_str()
                .is_some_and(|message| message.contains("/api/v1/auth/signup/start")),
            "{reply}"
        );
    }
    assert!(
        forge.core.get_account("grace").is_err(),
        "no account was created"
    );
}

/// A guess, a replay and an expired link are one refusal.
#[tokio::test]
async fn a_bad_replayed_or_expired_link_is_one_refusal() {
    let forge = forge();
    let (status, reply) = signup(
        &forge,
        &json!({ "token": "0".repeat(64), "login": "grace", "password": SIGNUP_PASSWORD }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
    assert_eq!(reply["code"], "signup_link_invalid");

    let secret = mailed_signup_secret(&forge, "grace@example.com").await;
    let body = json!({ "token": secret, "login": "grace", "password": SIGNUP_PASSWORD });
    assert_eq!(signup(&forge, &body).await.0, StatusCode::OK);
    let replay = json!({ "token": secret, "login": "grace2", "password": SIGNUP_PASSWORD });
    let (status, reply) = signup(&forge, &replay).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
    assert_eq!(reply["code"], "signup_link_invalid");

    let long_ago = chrono::Utc::now().timestamp_millis() - 25 * 60 * 60 * 1000;
    let expired = forge
        .store
        .issue_signup("old@example.com", long_ago)
        .expect("issue an old link");
    let (status, reply) = signup(
        &forge,
        &json!({ "token": expired.secret_for_test(), "login": "old", "password": SIGNUP_PASSWORD }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
    assert_eq!(reply["code"], "signup_link_invalid");
    assert!(forge.core.get_account("old").is_err());
}

/// A taken login answers 409 and the same link still works with another one.
#[tokio::test]
async fn a_taken_login_leaves_the_link_spendable() {
    let forge = forge();
    let secret = mailed_signup_secret(&forge, "grace@example.com").await;
    let (status, reply) = signup(
        &forge,
        &json!({ "token": secret, "login": "ada", "password": SIGNUP_PASSWORD }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert_eq!(reply["code"], "conflict");
    let (status, reply) = signup(
        &forge,
        &json!({ "token": secret, "login": "grace", "password": SIGNUP_PASSWORD }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(
        forge.store.get("ada").expect("read ada's address"),
        None,
        "the existing account was not given the address"
    );
}

/// A password core would refuse is refused before the link is spent.
#[tokio::test]
async fn a_weak_password_does_not_spend_the_link() {
    let forge = forge();
    let secret = mailed_signup_secret(&forge, "grace@example.com").await;
    let (status, reply) = signup(
        &forge,
        &json!({ "token": secret, "login": "grace", "password": "short" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
    assert_eq!(reply["code"], "invalid_input");
    let (status, reply) = signup(
        &forge,
        &json!({ "token": secret, "login": "grace", "password": SIGNUP_PASSWORD }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
}

/// An address an account has proved gets a note naming the account, no link,
/// and the caller sees the same answer as for a fresh address.
#[tokio::test]
async fn a_proved_address_is_told_it_has_an_account_and_gets_no_link() {
    let forge = forge();
    verify_address(&forge, "ada@example.com").await;
    let (fresh_status, fresh_body) = start(&forge, "nobody@example.com").await;
    let before = forge.mailer.sent().len();
    let (status, body) = start(&forge, "ada@example.com").await;
    assert_eq!(status, fresh_status);
    assert_eq!(body, fresh_body, "the answer must not tell the two apart");
    let sent = forge.mailer.sent();
    assert_eq!(sent.len(), before + 1);
    let (to, rendered) = &sent[before];
    assert_eq!(to, "ada@example.com");
    assert!(
        rendered.contains("Subject: Your jeryu account already exists"),
        "{rendered}"
    );
    assert!(rendered.contains("the login is ada"), "{rendered}");
    assert!(
        rendered.contains("https://jeryu.test/forgot-password"),
        "{rendered}"
    );
    assert!(!rendered.contains("token="), "no sign-up link: {rendered}");
}

/// An address another account proved after the link was mailed is refused,
/// and no account is created.
#[tokio::test]
async fn an_address_proved_meanwhile_is_refused() {
    let forge = forge();
    let secret = mailed_signup_secret(&forge, "ada@example.com").await;
    verify_address(&forge, "ada@example.com").await;
    let (status, reply) = signup(
        &forge,
        &json!({ "token": secret, "login": "grace", "password": SIGNUP_PASSWORD }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert_eq!(reply["code"], "conflict");
    assert!(forge.core.get_account("grace").is_err());
}

/// Without mail sign-up is impossible, and the answer says so.
#[tokio::test]
async fn an_unconfigured_mailer_refuses_to_start() {
    let forge = forge_with_mailer(Mailer::unconfigured());
    let (status, reply) = start(&forge, "grace@example.com").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{reply}");
    assert_eq!(reply["code"], "mail_unavailable");
}

#[tokio::test]
async fn a_malformed_address_is_refused() {
    let forge = forge();
    let (status, reply) = start(&forge, "not an address").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
    assert_eq!(reply["code"], "invalid_input");
    assert!(forge.mailer.sent().is_empty());
}

/// One client is held to five starts an hour, whatever address it names.
#[tokio::test]
async fn starting_is_rate_limited_per_client() {
    let forge = forge();
    let mut statuses = Vec::new();
    for at in 0..7 {
        statuses.push(start(&forge, &format!("person{at}@example.com")).await.0);
    }
    assert!(
        statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "{statuses:?}"
    );
    assert!(forge.mailer.sent().len() <= 5);
}

/// The durable per-address cap is counted from the table, so a restart does
/// not reset it.
#[tokio::test]
async fn the_durable_per_address_cap_holds_across_restarts() {
    let forge = forge();
    let now_ms = chrono::Utc::now().timestamp_millis();
    for _ in 0..5 {
        forge
            .store
            .issue_signup("grace@example.com", now_ms)
            .expect("issue a link");
    }
    let (status, _) = start(&forge, "grace@example.com").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(forge.mailer.sent().is_empty());
}

/// Completing a sign-up leaves an audit record, and neither it nor the start
/// record carries the address.
#[tokio::test]
async fn signup_is_audited_without_the_address() {
    let (forge, _dir) = audited_forge(Mailer::recording());
    let secret = mailed_signup_secret(&forge, "grace@example.com").await;
    let (status, _) = signup(
        &forge,
        &json!({ "token": secret, "login": "grace", "password": SIGNUP_PASSWORD }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let records = forge
        .core
        .list_audit("account_email")
        .expect("read the audit trail");
    assert!(
        records.iter().any(
            |record| record.action == "account.signup.completed" && record.phase == "completed"
        ),
        "{records:?}"
    );
    assert!(
        records.iter().any(
            |record| record.action == "account.signup.requested" && record.phase == "completed"
        ),
        "{records:?}"
    );
    let rendered = format!("{records:?}");
    assert!(!rendered.contains("grace@example.com"), "{rendered}");
    assert!(!rendered.contains(&secret), "{rendered}");
}
