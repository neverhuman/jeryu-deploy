//! The receipt `POST /api/v1/waitlist` sends to a newly listed address.
//!
//! The negative proofs: a repeat of a listed address sends nothing, the
//! visitor's name and note never reach the message, and a forge without mail
//! still answers the join exactly as before.

use std::time::Duration;

use axum::http::{Method as HttpMethod, StatusCode};
use serde_json::json;

use super::account_email_tests::{audited_forge, forge_with_mailer, json_reply};
use crate::web::mail::Mailer;

/// The receipt leaves on its own task after the response; wait for it.
async fn sent_after_join(mailer: &Mailer, at_least: usize) -> Vec<(String, String)> {
    for _ in 0..200 {
        let sent = mailer.sent();
        if sent.len() >= at_least {
            return sent;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    mailer.sent()
}

async fn join(router: &axum::Router, body: serde_json::Value) {
    let (status, reply) = json_reply(
        router,
        HttpMethod::POST,
        "/api/v1/waitlist",
        None,
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{reply}");
    assert_eq!(reply, json!({ "result": "received" }));
}

#[tokio::test]
async fn a_new_address_gets_one_receipt_and_a_repeat_gets_none() {
    let forge = forge_with_mailer(Mailer::recording());
    join(
        &forge.router,
        json!({
            "email": "grace@example.com",
            "name": "Visit https://spam.example now",
            "note": "a note only the operator reads"
        }),
    )
    .await;
    let sent = sent_after_join(&forge.mailer, 1).await;
    assert_eq!(sent.len(), 1, "one receipt for a new address: {sent:?}");
    let (to, rendered) = &sent[0];
    assert_eq!(to, "grace@example.com");
    assert!(
        rendered.contains("Subject: You're on the JeRyu waitlist"),
        "{rendered}"
    );
    assert!(!rendered.contains("spam.example"), "{rendered}");
    assert!(!rendered.contains("only the operator reads"), "{rendered}");

    join(
        &forge.router,
        json!({ "email": "GRACE@example.com", "name": "again" }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        forge.mailer.sent().len(),
        1,
        "a repeat of a listed address must not mail it again"
    );
}

#[tokio::test]
async fn a_forge_without_mail_still_lists_the_address_and_records_why() {
    let (forge, _dir) = audited_forge(Mailer::unconfigured());
    join(&forge.router, json!({ "email": "linus@example.com" })).await;
    let mut records = Vec::new();
    for _ in 0..200 {
        records = forge.core.list_audit("waitlist").expect("read the audit");
        if records
            .iter()
            .any(|record| record.action == "waitlist.confirmation")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        records.iter().any(|record| {
            record.action == "waitlist.confirmation" && record.phase == "requested"
        }),
        "the unsent receipt must be recorded: {records:?}"
    );
    assert_eq!(forge.core.list_waitlist().len(), 1);
}

#[tokio::test]
async fn a_sent_receipt_is_recorded_without_the_address() {
    let (forge, _dir) = audited_forge(Mailer::recording());
    join(&forge.router, json!({ "email": "barbara@example.com" })).await;
    sent_after_join(&forge.mailer, 1).await;
    let mut records = Vec::new();
    for _ in 0..200 {
        records = forge.core.list_audit("waitlist").expect("read the audit");
        if records
            .iter()
            .any(|record| record.action == "waitlist.confirmation")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let record = records
        .iter()
        .find(|record| record.action == "waitlist.confirmation")
        .expect("the receipt is recorded");
    assert_eq!(record.phase, "completed", "{records:?}");
    assert!(
        !format!("{record:?}").contains("barbara@example.com"),
        "the audit detail carries no address: {record:?}"
    );
}
