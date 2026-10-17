//! The one message a waitlist join sends: a receipt to a newly listed address.
//!
//! Only a `Created` join mails, and only when no other listed address reaches
//! the same mailbox (`ada+1@` and `ada@`, or Gmail's dotted spellings), so the
//! public form is not a way to mail one inbox again and again. Receipts also
//! stop for the hour once [`RECEIPTS_PER_HOUR`] addresses joined in it, which
//! bounds what the form can send from the operator's address in total.
//!
//! The message is fixed text. The name and note a visitor typed never reach
//! it: anyone can enter anyone's address, so echoing their words would let a
//! stranger send a message of their choosing from the forge's sender. The
//! recipient goes through [`normalize_address`] like every other message the
//! forge sends, so nothing but a plain address reaches the headers.
//!
//! The send runs after the response on its own task. The route answers a new
//! and a repeat address with the same bytes, and waiting on the host's mail
//! program would hand the caller the difference in time instead.
//!
//! Outcomes are audited under their own subject, `waitlist_mail`, never the
//! address itself.

use std::sync::Arc;

use chrono::{Duration, Utc};
use serde_json::json;

use super::WebState;
use super::account_email::normalize_address;
use super::mail::OutgoingMail;

const SUBJECT_KIND: &str = "waitlist_mail";
const ACTION: &str = "waitlist.confirmation";
const SUBJECT: &str = "You're on the JeRyu waitlist";
/// New addresses in the last hour past which no further receipt is sent.
pub(super) const RECEIPTS_PER_HOUR: usize = 60;

/// Start the receipt for `address` without holding the request.
pub(super) fn send_receipt_later(state: &Arc<WebState>, address: String) {
    let state = Arc::clone(state);
    tokio::spawn(async move { send_receipt(&state, &address).await });
}

/// Send the receipt if it is due, and record what happened. No outcome
/// reaches the caller.
pub(super) async fn send_receipt(state: &WebState, address: &str) {
    let Some(address) = normalize_address(address) else {
        record(state, "failed", json!({ "reason": "not a plain address" }));
        return;
    };
    if let Some(reason) = held_back(state, &address) {
        record(state, "requested", json!({ "reason": reason }));
        return;
    }
    let Some(origin) = state.mailer.link("") else {
        record(
            state,
            "requested",
            json!({ "reason": "mail is not configured" }),
        );
        return;
    };
    let outcome = state
        .mailer
        .send(OutgoingMail {
            to: address,
            subject: SUBJECT.to_string(),
            body: receipt_body(&origin),
        })
        .await;
    match outcome {
        Ok(()) => record(
            state,
            "completed",
            json!({ "transport": state.mailer.transport_name() }),
        ),
        Err(error) => record(
            state,
            "failed",
            json!({ "reason": format!("could not send the message: {error}") }),
        ),
    }
}

/// Why this address gets no receipt, if it does not.
fn held_back(state: &WebState, address: &str) -> Option<&'static str> {
    let signups = state.core.list_waitlist();
    let mailbox = mailbox_key(address);
    if signups
        .iter()
        .any(|signup| signup.email != address && mailbox_key(&signup.email) == mailbox)
    {
        return Some("another listed address reaches the same mailbox");
    }
    let hour_ago = Utc::now() - Duration::hours(1);
    let recent = signups
        .iter()
        .filter(|signup| signup.created_at > hour_ago)
        .count();
    (recent > RECEIPTS_PER_HOUR).then_some("hourly receipt ceiling reached")
}

/// The mailbox an address delivers to: no `+tag`, and for Gmail no dots and
/// one domain. Used only to compare addresses, never to send.
pub(super) fn mailbox_key(address: &str) -> String {
    let Some((local, domain)) = address.split_once('@') else {
        return address.to_string();
    };
    let local = local.split_once('+').map_or(local, |(base, _)| base);
    match domain {
        "gmail.com" | "googlemail.com" => format!("{}@gmail.com", local.replace('.', "")),
        _ => format!("{local}@{domain}"),
    }
}

fn receipt_body(origin: &str) -> String {
    format!(
        "Thanks for joining the JeRyu waitlist.\n\
         \n\
         We will write to this address once more, when your invitation is\n\
         ready. Joining did not create an account, and there is nothing else\n\
         to do for now.\n\
         \n\
         {origin}\n\
         \n\
         If you did not join, ignore this message: nothing was opened in your\n\
         name.\n"
    )
}

/// A failed audit write is swallowed, as on the account routes: the visitor
/// already has their answer.
fn record(state: &WebState, phase: &str, detail: serde_json::Value) {
    let _ = state
        .core
        .append_audit_as("local", ACTION, SUBJECT_KIND, phase, detail);
}

#[cfg(test)]
mod tests {
    use super::{mailbox_key, receipt_body};

    #[test]
    fn the_receipt_names_the_forge_and_no_account() {
        let body = receipt_body("https://forge.example/");
        assert!(body.contains("JeRyu waitlist"), "{body}");
        assert!(body.contains("did not create an account"), "{body}");
        assert!(body.contains("https://forge.example/"), "{body}");
    }

    #[test]
    fn aliases_of_one_mailbox_share_a_key() {
        assert_eq!(mailbox_key("ada+news@example.com"), "ada@example.com");
        assert_eq!(mailbox_key("a.d.a+x@googlemail.com"), "ada@gmail.com");
        assert_eq!(mailbox_key("a.da@gmail.com"), "ada@gmail.com");
        // Dots matter outside Gmail.
        assert_ne!(
            mailbox_key("a.da@example.com"),
            mailbox_key("ada@example.com")
        );
    }
}
