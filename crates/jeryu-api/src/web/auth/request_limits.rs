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

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
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

/// What a caller has left in the current window. Every limited answer carries
/// it, so a client that is close to the cap can slow down before it is
/// refused, and a refused one knows exactly how long to wait.
#[derive(Clone, Copy, Debug)]
pub(super) struct Budget {
    pub(super) limit: u32,
    pub(super) remaining: u32,
    /// When the window resets, as a Unix timestamp (the `X-RateLimit-Reset`
    /// shape GitHub clients already read).
    pub(super) reset_epoch: i64,
    /// Seconds until the window resets, at least one: `Retry-After: 0` reads
    /// as "retry now", which is the one thing the caller must not do.
    pub(super) retry_after_secs: i64,
}

impl Budget {
    fn new(limit: u32, attempts: u32, reset_at: DateTime<Utc>, now: DateTime<Utc>) -> Self {
        Self {
            limit,
            remaining: limit.saturating_sub(attempts),
            reset_epoch: reset_at.timestamp(),
            // Rounded up: a `Retry-After` a fraction of a second short sends
            // the caller straight back into the same window.
            retry_after_secs: ((reset_at - now).num_milliseconds() + 999)
                .saturating_div(1000)
                .clamp(1, LIMIT_WINDOW_SECS),
        }
    }

    /// The budget for a limit nothing has been counted against yet, used where
    /// the bucket is gone (its window elapsed) by the time a reply is shaped.
    fn full(limit: u32, now: DateTime<Utc>) -> Self {
        Self::new(
            limit,
            0,
            now + chrono::Duration::seconds(LIMIT_WINDOW_SECS),
            now,
        )
    }
}

/// Counts one request against `key` like [`hit`], and reports what the caller
/// has left in the window.
pub(super) fn hit_with_budget(
    limits: &mut BTreeMap<String, RateLimitBucket>,
    key: String,
    now: DateTime<Utc>,
    max: u32,
) -> (bool, Budget) {
    if max == 0 {
        return (false, Budget::full(max, now));
    }
    sweep(limits, now);
    let bucket = open_bucket(limits, key, now);
    bucket.attempts = bucket.attempts.saturating_add(1);
    let over = bucket.attempts > max;
    (
        over,
        Budget::new(max, bucket.attempts, bucket.reset_at, now),
    )
}

/// What `key` has left without counting this request.
pub(super) fn budget_for(
    limits: &BTreeMap<String, RateLimitBucket>,
    key: &str,
    now: DateTime<Utc>,
    max: u32,
) -> Budget {
    match limits.get(key).filter(|bucket| bucket.reset_at > now) {
        Some(bucket) => Budget::new(max, bucket.attempts, bucket.reset_at, now),
        None => Budget::full(max, now),
    }
}

/// Stamps the rate-limit budget on a reply. A limit of zero is a disabled cap,
/// which has no budget to report.
pub(super) fn stamp(response: &mut AxumResponse, budget: &Budget) {
    if budget.limit == 0 {
        return;
    }
    let headers = response.headers_mut();
    for (name, value) in [
        ("x-ratelimit-limit", budget.limit.to_string()),
        ("x-ratelimit-remaining", budget.remaining.to_string()),
        ("x-ratelimit-reset", budget.reset_epoch.to_string()),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static(name), value);
        }
    }
}

/// `429 rate_limited` with the budget and a `Retry-After` that says how long
/// the window still has to run.
pub(super) fn too_many(message: &str) -> AxumResponse {
    refused(message, &Budget::full(0, Utc::now()))
}

/// [`too_many`] for a caller whose remaining budget is known.
pub(super) fn refused(message: &str, budget: &Budget) -> AxumResponse {
    let mut response = api_error(StatusCode::TOO_MANY_REQUESTS, "rate_limited", message);
    let retry_after = if budget.limit == 0 {
        LIMIT_WINDOW_SECS
    } else {
        budget.retry_after_secs
    };
    if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    stamp(&mut response, budget);
    response
}

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
        assert!(hit(&mut limits, "k".into(), now, 3));
        let later = now + chrono::Duration::seconds(LIMIT_WINDOW_SECS);
        assert!(!hit(&mut limits, "k".into(), later, 3));
        for _ in 0..10 {
            assert!(!hit(&mut limits, "z".into(), now, 0));
        }
    }

    #[test]
    fn a_refusal_carries_retry_after_and_the_budget() {
        let mut limits = BTreeMap::new();
        let now = Utc::now();
        let (over, budget) = hit_with_budget(&mut limits, "k".into(), now, 2);
        assert!(!over);
        assert_eq!((budget.limit, budget.remaining), (2, 1));
        let (over, budget) = hit_with_budget(&mut limits, "k".into(), now, 2);
        assert!(!over);
        assert_eq!(budget.remaining, 0);
        let (over, budget) = hit_with_budget(&mut limits, "k".into(), now, 2);
        assert!(over);
        assert_eq!(budget.remaining, 0);
        assert!((1..=LIMIT_WINDOW_SECS).contains(&budget.retry_after_secs));
        let response = refused("too many", &budget);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let headers = response.headers();
        assert_eq!(
            headers
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some(budget.retry_after_secs.to_string().as_str())
        );
        assert_eq!(
            headers
                .get("x-ratelimit-limit")
                .and_then(|v| v.to_str().ok()),
            Some("2")
        );
        assert_eq!(
            headers
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok()),
            Some("0")
        );
        assert!(headers.contains_key("x-ratelimit-reset"));
    }

    /// A 429 from a disabled or unmetered cap still says how long to wait.
    #[test]
    fn a_refusal_without_a_known_budget_still_carries_retry_after() {
        let response = too_many("too many failed credentials");
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some(LIMIT_WINDOW_SECS.to_string().as_str())
        );
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
