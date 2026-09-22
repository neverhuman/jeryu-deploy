//! Per-credential limits in front of the `/api/v1` handlers:
//!
//! - reads (`GET`/`HEAD`) by one bearer token or session are capped per
//!   window, so a runaway polling agent answers 429 instead of hammering the
//!   read model;
//! - credentials that fail to authenticate are counted per client address,
//!   and once that address is over the cap it answers 429 before any further
//!   token is checked.
//!
//! Trusted local-dev requests carry no credential and are never limited.
//! Both caps read an environment override once (`JERYU_READ_RATE_LIMIT`,
//! `JERYU_BAD_TOKEN_RATE_LIMIT`, requests per minute; `0` disables).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response as AxumResponse;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use super::{RateLimitBucket, session_token_from_headers};
use crate::web::api_error;

pub(super) const LIMIT_WINDOW_SECS: i64 = 60;
const DEFAULT_READS_PER_WINDOW: u32 = 600;
const DEFAULT_BAD_TOKENS_PER_WINDOW: u32 = 20;
/// Expired buckets are swept once the map grows past this many keys.
const SWEEP_AT_KEYS: usize = 4096;

pub(super) fn reads_per_window() -> u32 {
    static LIMIT: OnceLock<u32> = OnceLock::new();
    *LIMIT.get_or_init(|| env_limit("JERYU_READ_RATE_LIMIT", DEFAULT_READS_PER_WINDOW))
}

pub(super) fn bad_tokens_per_window() -> u32 {
    static LIMIT: OnceLock<u32> = OnceLock::new();
    *LIMIT.get_or_init(|| env_limit("JERYU_BAD_TOKEN_RATE_LIMIT", DEFAULT_BAD_TOKENS_PER_WINDOW))
}

fn env_limit(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(default)
}

/// A stable, non-reversible key for the credential a request presents, or
/// `None` when it presents none.
pub(super) fn credential_key(headers: &HeaderMap) -> Option<String> {
    let credential = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(jeryu_gitd::auth::extract_bearer_or_basic)
        .or_else(|| session_token_from_headers(headers))?;
    let digest = Sha256::digest(credential.as_bytes());
    Some(hex::encode(&digest[..16]))
}

/// Counts one request against `key`; true once the window holds more than
/// `max`. A `max` of zero never limits.
pub(super) fn hit(
    limits: &mut BTreeMap<String, RateLimitBucket>,
    key: String,
    now: DateTime<Utc>,
    max: u32,
) -> bool {
    if max == 0 {
        return false;
    }
    sweep(limits, now);
    let bucket = open_bucket(limits, key, now);
    bucket.attempts = bucket.attempts.saturating_add(1);
    bucket.attempts > max
}

/// True when `key` is already over `max` in the current window, without
/// counting this request.
pub(super) fn exceeded(
    limits: &BTreeMap<String, RateLimitBucket>,
    key: &str,
    now: DateTime<Utc>,
    max: u32,
) -> bool {
    max != 0
        && limits
            .get(key)
            .is_some_and(|bucket| bucket.reset_at > now && bucket.attempts >= max)
}

pub(super) fn open_bucket(
    limits: &mut BTreeMap<String, RateLimitBucket>,
    key: String,
    now: DateTime<Utc>,
) -> &mut RateLimitBucket {
    let window = chrono::Duration::seconds(LIMIT_WINDOW_SECS);
    let bucket = limits.entry(key).or_insert_with(|| RateLimitBucket {
        attempts: 0,
        reset_at: now + window,
    });
    if bucket.reset_at <= now {
        bucket.attempts = 0;
        bucket.reset_at = now + window;
    }
    bucket
}

fn sweep(limits: &mut BTreeMap<String, RateLimitBucket>, now: DateTime<Utc>) {
    if limits.len() >= SWEEP_AT_KEYS {
        limits.retain(|_, bucket| bucket.reset_at > now);
    }
}

/// `429 rate_limited` with a `Retry-After` of one window.
pub(super) fn too_many(message: &str) -> AxumResponse {
    let mut response = api_error(StatusCode::TOO_MANY_REQUESTS, "rate_limited", message);
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_static(RETRY_AFTER_SECS),
    );
    response
}

const RETRY_AFTER_SECS: &str = "60";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_trips_past_max_and_zero_never_limits() {
        let mut limits = BTreeMap::new();
        let now = Utc::now();
        for _ in 0..3 {
            assert!(!hit(&mut limits, "k".into(), now, 3));
        }
        assert!(exceeded(&limits, "k", now, 3));
        assert!(hit(&mut limits, "k".into(), now, 3));
        let later = now + chrono::Duration::seconds(LIMIT_WINDOW_SECS);
        assert!(!exceeded(&limits, "k", later, 3));
        assert!(!hit(&mut limits, "k".into(), later, 3));
        for _ in 0..10 {
            assert!(!hit(&mut limits, "z".into(), now, 0));
        }
    }

    #[test]
    fn credential_key_hides_the_token_and_ignores_anonymous_requests() {
        assert_eq!(credential_key(&HeaderMap::new()), None);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret-token"),
        );
        let key = credential_key(&headers).unwrap();
        assert_eq!(key.len(), 32);
        assert!(!key.contains("secret"));
    }
}
