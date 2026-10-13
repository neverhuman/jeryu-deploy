//! Whether anybody may sign up, and where to send them when they may not
//! (`JERYU_SIGNUP`, `JERYU_WAITLIST_URL`).
//!
//! Sign-up is closed unless an operator opens it. A forge is reachable by
//! anyone who finds its address, and an account on it is a principal that can
//! own repositories and run work, so the safe default is that accounts are
//! made by an administrator (`POST /api/v1/admin/users`) and people who want
//! one join a waitlist.
//!
//! - `JERYU_SIGNUP=email` opens the email-first flow in [`super::signup`].
//!   Anything else — unset, empty, `closed`, or a value this binary does not
//!   know — is closed; an unknown value is named once on the startup log.
//! - `JERYU_WAITLIST_URL` is where a person joins the waitlist, an absolute
//!   `https` URL served elsewhere. An invalid one is ignored and named once
//!   on the startup log. Unset, the web app falls back to the forge's own
//!   `POST /api/v1/waitlist`.
//!
//! `GET /api/v1/auth/signup/policy` serves both, open, so the web app can
//! draw the right surface before anybody signs in. Operator notes:
//! `docs/account-email.md`, `docs/configuration.md`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use serde::Serialize;
use serde_json::json;

use super::WebState;

const SIGNUP_ENV: &str = "JERYU_SIGNUP";
const WAITLIST_URL_ENV: &str = "JERYU_WAITLIST_URL";

/// How a person gets an account.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SignupMode {
    /// Only an administrator makes accounts; everybody else joins a waitlist.
    #[default]
    Closed,
    /// Anybody may sign up by proving an email address first.
    Email,
}

/// The sign-up policy, read once at startup.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SignupPolicy {
    pub mode: SignupMode,
    pub waitlist_url: Option<String>,
}

impl SignupPolicy {
    /// Email-first sign-up and no external waitlist, for a test of that flow.
    #[cfg(test)]
    pub(crate) fn email() -> Self {
        Self {
            mode: SignupMode::Email,
            waitlist_url: None,
        }
    }

    /// Read `JERYU_SIGNUP` and `JERYU_WAITLIST_URL`, naming any value that
    /// was not understood on the startup log.
    // A test builds its policy with `parse`, so it never reads the host's.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn from_env() -> Self {
        let policy = Self::parse(
            std::env::var(SIGNUP_ENV).ok().as_deref(),
            std::env::var(WAITLIST_URL_ENV).ok().as_deref(),
        );
        for complaint in &policy.1 {
            eprintln!("jeryu-api sign-up: {complaint}");
        }
        policy.0
    }

    /// The policy for two raw values, and a complaint for each one ignored.
    pub(crate) fn parse(mode: Option<&str>, waitlist_url: Option<&str>) -> (Self, Vec<String>) {
        let mut complaints = Vec::new();
        let mode = match mode.map(str::trim).unwrap_or_default() {
            "email" => SignupMode::Email,
            "" | "closed" => SignupMode::Closed,
            other => {
                complaints.push(format!(
                    "{SIGNUP_ENV}={other:?} is not closed or email; sign-up stays closed"
                ));
                SignupMode::Closed
            }
        };
        let waitlist_url = match waitlist_url.map(str::trim).unwrap_or_default() {
            "" => None,
            raw if valid_waitlist_url(raw) => Some(raw.to_string()),
            raw => {
                complaints.push(format!(
                    "{WAITLIST_URL_ENV}={raw:?} is not an absolute https URL; it is ignored"
                ));
                None
            }
        };
        (Self { mode, waitlist_url }, complaints)
    }

    pub(crate) fn is_open(&self) -> bool {
        self.mode == SignupMode::Email
    }
}

/// An absolute `https` URL: a host of label characters with an optional
/// port, nothing a browser would read as credentials, and no whitespace or
/// control character anywhere. Checked by hand so the policy needs no URL
/// parser of its own.
fn valid_waitlist_url(raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix("https://") else {
        return false;
    };
    if raw.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    let host_ok = host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    let port_ok = port.is_none_or(|port| !port.is_empty() && port.parse::<u16>().is_ok());
    host_ok && port_ok
}

/// `GET /api/v1/auth/signup/policy` — how a person gets an account here.
pub(super) async fn get_policy(State(state): State<Arc<WebState>>) -> AxumResponse {
    Json(state.signup_policy.clone()).into_response()
}

/// The refusal both sign-up routes answer while sign-up is closed, carrying
/// where to go instead.
pub(super) fn closed(policy: &SignupPolicy) -> AxumResponse {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "code": "signup_closed",
            "message": "sign-up is by invitation; join the waitlist",
            "waitlistUrl": policy.waitlist_url,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_up_is_closed_unless_it_is_opened_by_name() {
        for raw in [None, Some(""), Some("  "), Some("closed")] {
            let (policy, complaints) = SignupPolicy::parse(raw, None);
            assert_eq!(policy.mode, SignupMode::Closed, "{raw:?}");
            assert!(complaints.is_empty(), "{raw:?}: {complaints:?}");
        }
        let (policy, complaints) = SignupPolicy::parse(Some("open"), None);
        assert_eq!(policy.mode, SignupMode::Closed);
        assert_eq!(complaints.len(), 1, "an unknown value is named once");
        let (policy, complaints) = SignupPolicy::parse(Some(" email "), None);
        assert!(policy.is_open());
        assert!(complaints.is_empty());
    }

    #[test]
    fn only_an_absolute_https_waitlist_url_is_kept() {
        let (policy, complaints) =
            SignupPolicy::parse(None, Some("https://waitlist.example.com/join?from=forge"));
        assert_eq!(
            policy.waitlist_url.as_deref(),
            Some("https://waitlist.example.com/join?from=forge")
        );
        assert!(complaints.is_empty());
        let (policy, _) = SignupPolicy::parse(None, Some("https://waitlist.example.com:8443/"));
        assert!(policy.waitlist_url.is_some(), "a port is allowed");
        for raw in [
            "http://waitlist.example.com/join",
            "/join",
            "waitlist.example.com",
            "https://user:secret@waitlist.example.com/",
            "javascript:alert(1)",
            "https://",
            "https://waitlist.example.com/a b",
        ] {
            let (policy, complaints) = SignupPolicy::parse(None, Some(raw));
            assert_eq!(policy.waitlist_url, None, "{raw}");
            assert_eq!(complaints.len(), 1, "{raw}");
        }
    }
}
