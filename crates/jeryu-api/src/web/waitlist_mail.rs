//! The one message a waitlist join sends: a receipt to a newly listed address.
//!
//! Only a `Created` join mails. A repeat of a listed address sends nothing, so
//! the public form cannot be used to mail one address over and over.
//!
//! The message is fixed text. The name and note a visitor typed never reach
//! it: anyone can enter anyone's address, so echoing their words would let a
//! stranger send a message of their choosing from the forge's sender.
//!
//! The send runs after the response on its own task. The route answers a new
//! and a repeat address with the same bytes, and waiting on the host's mail
//! program would hand the caller the difference in time instead.

use std::sync::Arc;

use serde_json::json;

use super::WebState;
use super::mail::OutgoingMail;

const ACTION: &str = "waitlist.confirmation";
const SUBJECT: &str = "You're on the JeRyu waitlist";

/// Start the receipt for `address` without holding the request.
pub(super) fn send_receipt_later(state: &Arc<WebState>, address: String) {
    let state = Arc::clone(state);
    tokio::spawn(async move { send_receipt(&state, &address).await });
}

/// Send the receipt and record what happened. No outcome reaches the caller.
pub(super) async fn send_receipt(state: &WebState, address: &str) {
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
            to: address.to_string(),
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
        .append_audit_as("local", ACTION, "waitlist", phase, detail);
}

#[cfg(test)]
mod tests {
    use super::receipt_body;

    #[test]
    fn the_receipt_names_the_forge_and_no_account() {
        let body = receipt_body("https://forge.example/");
        assert!(body.contains("JeRyu waitlist"), "{body}");
        assert!(body.contains("did not create an account"), "{body}");
        assert!(body.contains("https://forge.example/"), "{body}");
    }
}
