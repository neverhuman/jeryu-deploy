//! Hand each newly listed waitlist address to another system, for an operator
//! who keeps one list (and one notification channel) across several products.
//!
//! Off unless configured. The forge keeps its own copy either way: this only
//! tells the other side, after the visitor already has their answer.
//!
//! - `JERYU_WAITLIST_FORWARD_URL`: an `https` URL (or `http` to a loopback
//!   address) that takes one JSON `POST` per new address.
//! - `JERYU_WAITLIST_FORWARD_SECRET_FILE`: a file holding the shared secret.
//! - `JERYU_WAITLIST_PRODUCT`: the product slug sent along (default `jeryu`).
//!
//! The request carries `X-Waitlist-Timestamp` (unix seconds) and
//! `X-Waitlist-Signature: sha256=<hex>`, an HMAC-SHA256 of
//! `"<timestamp>.<body>"` under the secret, so the receiver can refuse
//! anything it did not come from here or that is replayed later. The body is
//! `{"product","email","name","note","joinedAt","origin"}`.
//!
//! A repeat join sends nothing. A failed send is retried twice, then left:
//! every outcome is audited under `waitlist_forward`, never with the address,
//! and the row stays on the forge's own list.

use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use jeryu_core::WaitlistSignup;
use serde_json::json;
use sha2::Sha256;

use super::WebState;

const URL_ENV: &str = "JERYU_WAITLIST_FORWARD_URL";
const SECRET_FILE_ENV: &str = "JERYU_WAITLIST_FORWARD_SECRET_FILE";
const PRODUCT_ENV: &str = "JERYU_WAITLIST_PRODUCT";
const DEFAULT_PRODUCT: &str = "jeryu";
const SUBJECT: &str = "waitlist_forward";
const ACTION: &str = "waitlist.forwarded";
const ATTEMPTS: u32 = 3;
const TIMEOUT: Duration = Duration::from_secs(10);

type HmacSha256 = Hmac<Sha256>;

/// Where new addresses go, and how the request is signed.
#[derive(Clone)]
pub(crate) struct WaitlistForward {
    url: String,
    secret: Vec<u8>,
    product: String,
    origin: Option<String>,
    client: reqwest::Client,
    /// Waits between attempts; short in a test.
    backoff: Vec<Duration>,
}

impl std::fmt::Debug for WaitlistForward {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The secret never prints.
        formatter
            .debug_struct("WaitlistForward")
            .field("url", &self.url)
            .field("product", &self.product)
            .finish_non_exhaustive()
    }
}

impl WaitlistForward {
    /// Read the settings once. Anything missing or invalid turns forwarding
    /// off, and a set-but-invalid value is named on one startup line.
    pub(crate) fn from_env() -> Option<Arc<Self>> {
        let url = env_value(URL_ENV)?;
        if !url_ok(&url) {
            eprintln!(
                "jeryu-api waitlist: {URL_ENV} must be an https URL (or http to loopback); forwarding is off"
            );
            return None;
        }
        let Some(secret_file) = env_value(SECRET_FILE_ENV) else {
            eprintln!(
                "jeryu-api waitlist: {URL_ENV} is set without {SECRET_FILE_ENV}; forwarding is off"
            );
            return None;
        };
        let secret = match std::fs::read_to_string(&secret_file) {
            Ok(raw) if raw.trim().len() >= 32 => raw.trim().as_bytes().to_vec(),
            Ok(_) => {
                eprintln!(
                    "jeryu-api waitlist: the secret in {SECRET_FILE_ENV} is shorter than 32 characters; forwarding is off"
                );
                return None;
            }
            Err(error) => {
                eprintln!(
                    "jeryu-api waitlist: could not read {SECRET_FILE_ENV}: {error}; forwarding is off"
                );
                return None;
            }
        };
        let product = env_value(PRODUCT_ENV).unwrap_or_else(|| DEFAULT_PRODUCT.to_string());
        if !product_ok(&product) {
            eprintln!(
                "jeryu-api waitlist: {PRODUCT_ENV} must be a lowercase slug; forwarding is off"
            );
            return None;
        }
        let origin =
            env_value("JERYU_MAIL_BASE_URL").map(|url| url.trim_end_matches('/').to_string());
        Self::build(
            url,
            secret,
            product,
            origin,
            vec![Duration::from_secs(2), Duration::from_secs(10)],
        )
        .map(Arc::new)
    }

    fn build(
        url: String,
        secret: Vec<u8>,
        product: String,
        origin: Option<String>,
        backoff: Vec<Duration>,
    ) -> Option<Self> {
        let client = reqwest::Client::builder().timeout(TIMEOUT).build().ok()?;
        Some(Self {
            url,
            secret,
            product,
            origin,
            client,
            backoff,
        })
    }

    /// Test-only: forward to `url` with `secret`, retrying without waiting.
    #[cfg(test)]
    pub(crate) fn for_test(url: &str, secret: &str) -> Arc<Self> {
        Arc::new(
            Self::build(
                url.to_string(),
                secret.as_bytes().to_vec(),
                DEFAULT_PRODUCT.to_string(),
                Some("https://forge.example".to_string()),
                vec![Duration::ZERO, Duration::ZERO],
            )
            .expect("a client"),
        )
    }

    /// The request body for `signup`.
    fn body(&self, signup: &WaitlistSignup) -> String {
        json!({
            "product": self.product,
            "email": signup.email,
            "name": signup.name,
            "note": signup.note,
            "joinedAt": signup.created_at.to_rfc3339(),
            "origin": self.origin,
        })
        .to_string()
    }

    /// `sha256=<hex>` over `"<timestamp>.<body>"`.
    pub(crate) fn signature(secret: &[u8], timestamp: i64, body: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC takes a key of any length");
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(body.as_bytes());
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    async fn post_once(&self, body: &str) -> Result<(), String> {
        let timestamp = chrono::Utc::now().timestamp();
        let response = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("X-Waitlist-Timestamp", timestamp.to_string())
            .header(
                "X-Waitlist-Signature",
                Self::signature(&self.secret, timestamp, body),
            )
            .body(body.to_string())
            .send()
            .await
            // Built here, not from the error: the error would print the URL.
            .map_err(|error| {
                if error.is_timeout() {
                    "the receiver did not answer in time".to_string()
                } else {
                    "the receiver could not be reached".to_string()
                }
            })?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("the receiver answered {}", status.as_u16()))
        }
    }
}

/// Forward `signup` on its own task, so the join's answer never waits on it.
pub(super) fn forward_later(state: &Arc<WebState>, signup: WaitlistSignup) {
    let Some(forward) = state.waitlist_forward.clone() else {
        return;
    };
    let state = Arc::clone(state);
    tokio::spawn(async move { forward_now(&state, &forward, &signup).await });
}

/// Send, retrying a failure, and record the outcome.
pub(super) async fn forward_now(
    state: &WebState,
    forward: &WaitlistForward,
    signup: &WaitlistSignup,
) {
    let body = forward.body(signup);
    let mut last_error = String::new();
    for attempt in 1..=ATTEMPTS {
        match forward.post_once(&body).await {
            Ok(()) => {
                record(state, "completed", json!({ "attempts": attempt }));
                return;
            }
            Err(error) => last_error = error,
        }
        if let Some(wait) = forward.backoff.get(attempt as usize - 1) {
            tokio::time::sleep(*wait).await;
        }
    }
    record(
        state,
        "failed",
        json!({ "attempts": ATTEMPTS, "reason": last_error }),
    );
}

fn record(state: &WebState, phase: &str, detail: serde_json::Value) {
    let _ = state
        .core
        .append_audit_as("local", ACTION, SUBJECT, phase, detail);
}

fn url_ok(url: &str) -> bool {
    let clean = !url.chars().any(|c| c.is_whitespace() || c.is_control());
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://").filter(|rest| loopback(rest)))
    else {
        return false;
    };
    clean && !rest.is_empty()
}

fn loopback(rest: &str) -> bool {
    let host = rest.split(['/', '?']).next().unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(name, _)| name);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

fn product_ok(product: &str) -> bool {
    let mut chars = product.chars();
    product.len() >= 2
        && product.len() <= 32
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signature_is_hmac_over_timestamp_dot_body() {
        // Fixed vector: HMAC-SHA256(key="secret", msg="1700000000.{}").
        let signature = WaitlistForward::signature(b"secret", 1_700_000_000, "{}");
        assert_eq!(
            signature,
            "sha256=b8569b78799ff9e3cbff0fc2d63a33a2b57f3282abd07c37ae5e8e7d79a5f163"
        );
        assert_ne!(
            signature,
            WaitlistForward::signature(b"secret", 1_700_000_001, "{}")
        );
        assert_ne!(
            signature,
            WaitlistForward::signature(b"other", 1_700_000_000, "{}")
        );
    }

    #[test]
    fn only_https_or_loopback_http_is_a_destination() {
        assert!(url_ok(
            "https://lists.example.com/api/integrations/waitlist"
        ));
        assert!(url_ok("http://127.0.0.1:8080/intake"));
        assert!(url_ok("http://localhost/intake"));
        assert!(!url_ok("http://lists.example.com/intake"));
        assert!(!url_ok("https://"));
        assert!(!url_ok("ftp://lists.example.com"));
        assert!(!url_ok("https://lists.example.com/a b"));
    }

    #[test]
    fn a_product_is_a_lowercase_slug() {
        assert!(product_ok("jeryu"));
        assert!(product_ok("acme-web2"));
        assert!(!product_ok("Jeryu"));
        assert!(!product_ok("j"));
        assert!(!product_ok("2fast"));
        assert!(!product_ok("a b"));
    }

    /// Read the way production reads it: forwarding is on only when the URL
    /// is set (an invalid setup is refused, never half-enabled).
    #[test]
    fn from_env_is_off_without_a_url() {
        if env_value(URL_ENV).is_none() {
            assert!(WaitlistForward::from_env().is_none());
        }
        let _ = (SECRET_FILE_ENV, PRODUCT_ENV);
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let forward = WaitlistForward::for_test("https://lists.example.com", "s3cr3t-value");
        assert!(!format!("{forward:?}").contains("s3cr3t-value"));
    }
}
