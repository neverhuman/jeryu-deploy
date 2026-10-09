//! Watches for proxied requests the forge cannot attribute.
//!
//! [`super::auth::client_ip`] believes `X-Forwarded-For` only from a peer
//! listed in `JERYU_TRUSTED_PROXIES`. Behind an edge proxy that is not listed
//! there, every per-address limit keys on the proxy's one address: the public
//! waitlist becomes a single global five joins an hour, and the bot token
//! routes share one bucket. The forge cannot tell on its own whether a
//! forwarding header is honest, so it says what it sees: the first such
//! request writes one warning naming the variable to set, and the admin
//! runners and system pages carry the same sentence with a count.

use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Whether `X-Forwarded-For` is being honoured, and what to do when it is not.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProxyTrustStatus {
    /// Addresses parsed out of `JERYU_TRUSTED_PROXIES`.
    pub trusted_proxies: usize,
    /// Requests seen carrying `X-Forwarded-For` from a peer not in that list.
    pub forwarded_from_untrusted_peer: u64,
    /// The last such peer, which is the address every limit keyed on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_peer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    /// What an admin should act on; `null` while nothing is wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// One sentence an admin can act on, naming the peer and the variable.
fn warning(peer: IpAddr) -> String {
    format!(
        "requests arrive with X-Forwarded-For from {peer}, which is not in JERYU_TRUSTED_PROXIES, \
         so the header is ignored and every per-address rate limit keys on {peer}: set \
         JERYU_TRUSTED_PROXIES to the edge proxy's address in the forge's site env file and \
         restart the forge"
    )
}

/// What the process has seen. One instance per forge, plus a fresh one per
/// test: `client_ip` is a free function, so the count cannot live in
/// `WebState`.
#[derive(Debug)]
pub(crate) struct ProxyTrustWatch {
    seen: AtomicU64,
    warned: AtomicBool,
    last: Mutex<Option<(IpAddr, DateTime<Utc>)>>,
}

impl ProxyTrustWatch {
    pub(crate) const fn new() -> Self {
        Self {
            seen: AtomicU64::new(0),
            warned: AtomicBool::new(false),
            last: Mutex::new(None),
        }
    }

    /// Counts a request that carried `X-Forwarded-For` from an untrusted peer.
    /// Returns the warning to write the first time, and nothing after that: a
    /// misconfigured edge sends every request this way, and one line is the
    /// whole message.
    pub(crate) fn observe(
        &self,
        peer: IpAddr,
        headers: &HeaderMap,
        trusted: &[IpAddr],
        now: DateTime<Utc>,
    ) -> Option<String> {
        if trusted.contains(&peer) || !headers.contains_key("x-forwarded-for") {
            return None;
        }
        self.seen.fetch_add(1, Ordering::Relaxed);
        *self.last.lock().expect("proxy-trust mutex poisoned") = Some((peer, now));
        (!self.warned.swap(true, Ordering::Relaxed)).then(|| warning(peer))
    }

    pub(crate) fn status(&self, trusted_proxies: usize) -> ProxyTrustStatus {
        let last = *self.last.lock().expect("proxy-trust mutex poisoned");
        ProxyTrustStatus {
            trusted_proxies,
            forwarded_from_untrusted_peer: self.seen.load(Ordering::Relaxed),
            last_peer: last.map(|(peer, _)| peer.to_string()),
            last_seen: last.map(|(_, at)| at.to_rfc3339()),
            message: last.map(|(peer, _)| warning(peer)),
        }
    }
}

static WATCH: ProxyTrustWatch = ProxyTrustWatch::new();

/// Records what this request says about the edge, writing the one warning.
pub(crate) fn observe(peer: IpAddr, headers: &HeaderMap, trusted: &[IpAddr]) {
    if let Some(line) = WATCH.observe(peer, headers, trusted, Utc::now()) {
        eprintln!("jeryu-api proxy trust: {line}");
    }
}

/// The admin pages' view, for `GET /api/v1/control-plane/runners` and the
/// control-plane snapshot the system page reads.
pub(crate) fn status() -> ProxyTrustStatus {
    WATCH.status(super::auth::trusted_proxy_ips().len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn forwarded() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "198.51.100.8".parse().unwrap());
        headers
    }

    fn peer(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, last))
    }

    fn now() -> DateTime<Utc> {
        "2026-10-09T09:00:00Z".parse().unwrap()
    }

    /// The first forwarded request from an unlisted peer is the warning, and
    /// it names the peer every limit keys on and the variable to set.
    #[test]
    fn an_untrusted_forwarding_peer_warns_once_and_keeps_counting() {
        let watch = ProxyTrustWatch::new();
        let line = watch
            .observe(peer(9), &forwarded(), &[], now())
            .expect("the first forwarded request warns");
        assert!(line.contains("JERYU_TRUSTED_PROXIES"), "{line}");
        assert!(line.contains("203.0.113.9"), "{line}");
        assert_eq!(
            watch.observe(peer(9), &forwarded(), &[], now()),
            None,
            "a misconfigured edge forwards every request; one line is enough"
        );
        let status = watch.status(0);
        assert_eq!(status.forwarded_from_untrusted_peer, 2);
        assert_eq!(status.last_peer.as_deref(), Some("203.0.113.9"));
        assert_eq!(
            status.last_seen.as_deref(),
            Some("2026-10-09T09:00:00+00:00")
        );
        assert_eq!(status.message, Some(line));
        assert_eq!(status.trusted_proxies, 0);
    }

    /// A listed proxy is the configured case, and a request with no forwarding
    /// header says nothing about the edge: neither is worth a warning.
    #[test]
    fn a_trusted_proxy_and_a_direct_request_stay_quiet() {
        let watch = ProxyTrustWatch::new();
        assert_eq!(
            watch.observe(peer(9), &forwarded(), &[peer(9)], now()),
            None
        );
        assert_eq!(watch.observe(peer(9), &HeaderMap::new(), &[], now()), None);
        let status = watch.status(1);
        assert_eq!(status.forwarded_from_untrusted_peer, 0);
        assert_eq!(status.message, None);
        assert_eq!(status.last_peer, None);
        assert_eq!(status.trusted_proxies, 1);
    }
}
