//! Direct tests for the helpers `gate` leans on: the repo-id decoders, the
//! forwarded-for trust rule, the admin allow-list, the auth rate limiter and
//! the session cookie flags.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};

use axum::http::{HeaderMap, HeaderValue, Method, header};
use chrono::{Duration, Utc};
use jeryu_core::ForgeCore;

use super::{
    AUTH_LIMIT_MAX, AUTH_LIMIT_WINDOW_SECS, HOST_SESSION_COOKIE, LOCAL_SESSION_COOKIE,
    admin_only_path, admin_only_request, client_ip_with, cookie_header, expired_cookie_header,
    hex_val, parse_trusted_proxies, percent_decode, rate_limit_hit, repo_id_from_path,
    session_token_from_headers,
};
use crate::web::WebState;

fn ip(raw: &str) -> IpAddr {
    raw.parse().expect("test ip")
}

fn peer(raw: &str) -> Option<SocketAddr> {
    Some(SocketAddr::new(ip(raw), 40_000))
}

fn xff(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", HeaderValue::from_str(value).unwrap());
    headers
}

fn state(secure_cookies: bool) -> WebState {
    let mut state = WebState::new(ForgeCore::new());
    state.secure_cookies = secure_cookies;
    state
}

#[test]
fn hex_val_accepts_only_hex_digits() {
    assert_eq!(hex_val(b'0'), Some(0));
    assert_eq!(hex_val(b'9'), Some(9));
    assert_eq!(hex_val(b'a'), Some(10));
    assert_eq!(hex_val(b'F'), Some(15));
    for byte in [b'g', b'G', b'/', b':', b'@', b'`', b' ', b'%', 0x00, 0xff] {
        assert_eq!(hex_val(byte), None, "byte {byte:#x}");
    }
}

#[test]
fn percent_decode_decodes_escapes_and_keeps_malformed_ones() {
    assert_eq!(percent_decode("alice%2Fjeryu"), "alice/jeryu");
    assert_eq!(percent_decode("alice%2fjeryu"), "alice/jeryu");
    assert_eq!(percent_decode("plain"), "plain");
    assert_eq!(percent_decode(""), "");
    // Not hex, or truncated: left verbatim.
    assert_eq!(percent_decode("a%zzb"), "a%zzb");
    assert_eq!(percent_decode("a%2"), "a%2");
    assert_eq!(percent_decode("a%"), "a%");
    assert_eq!(percent_decode("%"), "%");
    assert_eq!(percent_decode("%41"), "A");
    assert_eq!(percent_decode("%41x"), "Ax");
    // Decoding is single-pass: a double-encoded escape decodes once.
    assert_eq!(percent_decode("%252e%252e"), "%2e%2e");
}

#[test]
fn percent_decode_exposes_traversal_but_never_panics_on_bad_utf8() {
    assert_eq!(percent_decode("%2e%2e%2f%2e%2e"), "../..");
    assert_eq!(percent_decode("..%2F..%2Fetc"), "../../etc");
    assert_eq!(percent_decode("a%00b"), "a\0b");
    // Overlong UTF-8 for '/' and a lone continuation byte become U+FFFD,
    // never a real slash.
    let overlong = percent_decode("%c0%afx");
    assert!(!overlong.contains('/'), "{overlong:?}");
    assert!(overlong.contains('\u{fffd}'));
    assert_eq!(percent_decode("%80x"), "\u{fffd}x");
}

#[test]
fn percent_decode_handles_long_input() {
    let raw = "%41".repeat(10_000) + "x";
    let decoded = percent_decode(&raw);
    assert_eq!(decoded.len(), 10_001);
    assert!(decoded.starts_with("AAAA") && decoded.ends_with('x'));
}

#[test]
fn repo_id_from_path_takes_one_decoded_segment() {
    assert_eq!(
        repo_id_from_path("/api/v1/repos/alice%2Fjeryu/tree"),
        Some("alice/jeryu".to_string())
    );
    assert_eq!(
        repo_id_from_path("/api/v1/repos/jeryu"),
        Some("jeryu".to_string())
    );
    // A literal slash ends the segment; an encoded traversal is decoded and
    // handed to the repo lookup as a name, which cannot match a repository.
    assert_eq!(
        repo_id_from_path("/api/v1/repos/../admin/users"),
        Some("..".to_string())
    );
    assert_eq!(
        repo_id_from_path("/api/v1/repos/%2e%2e%2fadmin/tree"),
        Some("../admin".to_string())
    );
    assert_eq!(repo_id_from_path("/api/v1/repos/"), None);
    assert_eq!(repo_id_from_path("/api/v1/repos//tree"), None);
    assert_eq!(repo_id_from_path("/api/v1/repos"), None);
    assert_eq!(repo_id_from_path("/api/v1/reposalice"), None);
    assert_eq!(repo_id_from_path("/api/v2/repos/alice"), None);
    let long = "a".repeat(64 * 1024);
    assert_eq!(
        repo_id_from_path(&format!("/api/v1/repos/{long}/tree")),
        Some(long)
    );
}

#[test]
fn trusted_proxy_list_skips_blank_and_invalid_entries() {
    assert_eq!(
        parse_trusted_proxies(" 10.0.0.1 , ,not-an-ip,::1,"),
        vec![ip("10.0.0.1"), ip("::1")]
    );
    assert!(parse_trusted_proxies("").is_empty());
    assert!(parse_trusted_proxies("10.0.0.0/8").is_empty());
}

#[test]
fn forwarded_for_from_an_untrusted_peer_is_ignored() {
    let trusted = [ip("10.0.0.1")];
    let spoofed = xff("203.0.113.9");
    assert_eq!(
        client_ip_with(peer("198.51.100.7"), &spoofed, &trusted),
        ip("198.51.100.7")
    );
    assert_eq!(
        client_ip_with(peer("198.51.100.7"), &spoofed, &[]),
        ip("198.51.100.7")
    );
    assert_eq!(
        client_ip_with(None, &spoofed, &trusted),
        ip("0.0.0.0"),
        "no peer falls back to the unspecified address, not the header"
    );
}

#[test]
fn forwarded_for_from_a_trusted_peer_uses_the_first_hop() {
    let trusted = [ip("10.0.0.1")];
    assert_eq!(
        client_ip_with(peer("10.0.0.1"), &xff(" 203.0.113.9 , 10.0.0.1"), &trusted),
        ip("203.0.113.9")
    );
    // A garbage or absent header keeps the proxy's own address.
    assert_eq!(
        client_ip_with(peer("10.0.0.1"), &xff("not-an-ip"), &trusted),
        ip("10.0.0.1")
    );
    assert_eq!(
        client_ip_with(peer("10.0.0.1"), &HeaderMap::new(), &trusted),
        ip("10.0.0.1")
    );
}

/// Every route under an admin prefix in the real route table is admin-only,
/// and the everyday routes are not.
#[test]
fn admin_allow_list_matches_the_route_table() {
    let source = include_str!("../../web.rs");
    let routes: Vec<&str> = source
        .split('"')
        .filter(|part| part.starts_with("/api/v1/"))
        .collect();
    assert!(routes.len() > 50, "route table not found: {}", routes.len());

    let admin_prefixes = [
        "/api/v1/admin/",
        "/api/v1/control-plane/",
        "/api/v1/workcells",
        "/api/v1/agent-runs",
        "/api/v1/fleet/",
        "/api/v1/codegraph/tool-build/",
        "/api/v1/tool-finder/",
    ];
    let mut admin_routes = 0;
    for route in &routes {
        let expected = admin_prefixes.iter().any(|p| route.starts_with(p));
        assert_eq!(admin_only_path(route), expected, "{route}");
        if expected {
            admin_routes += 1;
            for method in [Method::GET, Method::POST, Method::DELETE] {
                assert!(admin_only_request(&method, route), "{method} {route}");
            }
        }
    }
    assert!(admin_routes >= 20, "only {admin_routes} admin routes found");

    for route in [
        "/api/v1/repos",
        "/api/v1/repos/:id/tree",
        "/api/v1/repos/:id/agent-runs",
        "/api/v1/auth/me",
        "/api/v1/work",
        "/api/v1/version",
    ] {
        assert!(routes.contains(&route), "{route} missing from route table");
        assert!(!admin_only_request(&Method::GET, route), "{route}");
    }
    // Look-alikes of the admin prefix are not admitted by the allow-list.
    assert!(!admin_only_path("/api/v1/administrators"));
    assert!(!admin_only_path("/api/v1/admin"));

    // Shift reads are open, shift writes admin-only; event reads admin-only.
    assert!(!admin_only_request(&Method::GET, "/api/v1/shift/todos"));
    assert!(admin_only_request(&Method::POST, "/api/v1/shift/todos"));
    assert!(admin_only_request(&Method::GET, "/api/v1/events"));
    assert!(!admin_only_request(&Method::POST, "/api/v1/events"));
    assert!(admin_only_request(&Method::GET, "/api/v1/attention"));
    assert!(admin_only_request(&Method::GET, "/api/v1/pins/"));
}

#[test]
fn rate_limit_trips_after_the_max_and_resets_with_the_window() {
    let mut limits = BTreeMap::new();
    let start = Utc::now();
    let key = || "login:198.51.100.7:alice".to_string();
    for attempt in 1..=AUTH_LIMIT_MAX {
        assert!(
            !rate_limit_hit(&mut limits, key(), start),
            "attempt {attempt}"
        );
    }
    assert!(rate_limit_hit(&mut limits, key(), start));
    let almost = start + Duration::seconds(AUTH_LIMIT_WINDOW_SECS - 1);
    assert!(rate_limit_hit(&mut limits, key(), almost));

    let after = start + Duration::seconds(AUTH_LIMIT_WINDOW_SECS);
    assert!(
        !rate_limit_hit(&mut limits, key(), after),
        "a new window starts clean"
    );
    assert_eq!(limits[&key()].attempts, 1);
    assert_eq!(
        limits[&key()].reset_at,
        after + Duration::seconds(AUTH_LIMIT_WINDOW_SECS)
    );
}

/// Behind a trusted proxy each forwarded client gets its own bucket, so one
/// noisy client cannot lock out everyone sharing the proxy.
#[test]
fn rate_limit_keys_by_forwarded_client_behind_a_trusted_proxy() {
    let trusted = [ip("10.0.0.1")];
    let key_for = |forwarded: &str, from: &str| {
        let client = client_ip_with(peer(from), &xff(forwarded), &trusted);
        format!("login:{client}:alice")
    };
    let mut limits = BTreeMap::new();
    let now = Utc::now();
    for _ in 0..=AUTH_LIMIT_MAX {
        rate_limit_hit(&mut limits, key_for("203.0.113.1", "10.0.0.1"), now);
    }
    assert!(rate_limit_hit(
        &mut limits,
        key_for("203.0.113.1", "10.0.0.1"),
        now
    ));
    assert!(!rate_limit_hit(
        &mut limits,
        key_for("203.0.113.2", "10.0.0.1"),
        now
    ));

    // An untrusted peer rotating spoofed headers stays in one bucket.
    let mut limits = BTreeMap::new();
    for n in 0..AUTH_LIMIT_MAX {
        let spoofed = format!("203.0.113.{n}");
        assert!(!rate_limit_hit(
            &mut limits,
            key_for(&spoofed, "198.51.100.7"),
            now
        ));
    }
    assert!(rate_limit_hit(
        &mut limits,
        key_for("203.0.113.250", "198.51.100.7"),
        now
    ));
}

#[test]
fn session_cookie_flags_follow_secure_mode() {
    let secure = cookie_header(&state(true), "tok", Some(60)).unwrap();
    assert_eq!(
        secure.to_str().unwrap(),
        format!("{HOST_SESSION_COOKIE}=tok; Path=/; HttpOnly; SameSite=Lax; Max-Age=60; Secure")
    );
    let local = cookie_header(&state(false), "tok", None).unwrap();
    assert_eq!(
        local.to_str().unwrap(),
        format!("{LOCAL_SESSION_COOKIE}=tok; Path=/; HttpOnly; SameSite=Lax")
    );
    let expired = expired_cookie_header(&state(true)).unwrap();
    assert_eq!(
        expired.to_str().unwrap(),
        format!("{HOST_SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0; Secure")
    );
    let expired = expired_cookie_header(&state(false)).unwrap();
    assert!(!expired.to_str().unwrap().contains("Secure"));
    assert!(
        cookie_header(&state(false), "bad\ntoken", None).is_err(),
        "a token with a control byte cannot be smuggled into Set-Cookie"
    );
}

#[test]
fn session_token_is_read_from_either_cookie_name() {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        HeaderValue::from_static("theme=dark;  __Host-jeryu-session=abc; jeryu-session=def"),
    );
    assert_eq!(session_token_from_headers(&headers), Some("abc".into()));
    headers.insert(
        header::COOKIE,
        HeaderValue::from_static("jeryu-session=def"),
    );
    assert_eq!(session_token_from_headers(&headers), Some("def".into()));
    headers.insert(
        header::COOKIE,
        HeaderValue::from_static("xjeryu-session=nope; jeryu-sessionx=nope"),
    );
    assert_eq!(session_token_from_headers(&headers), None);
    assert_eq!(session_token_from_headers(&HeaderMap::new()), None);
}
