//! Upstream name resolution that only returns public addresses. The
//! production table names exact provider hosts; this keeps a hostile or
//! rebinding DNS answer from pointing the gateway, which holds credentials,
//! at loopback, a private network, or link-local metadata. A refusal is
//! `upstream_host_denied`. IP-literal URLs (the test loopback upstream) never
//! reach a resolver.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

pub(crate) struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let found = tokio::net::lookup_host((name.as_str(), 0)).await?;
            let public: Vec<SocketAddr> = found.filter(|addr| is_public(addr.ip())).collect();
            if public.is_empty() {
                return Err("upstream_host_denied: the host has no public address".into());
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

/// Globally routable unicast only: the special-use prefixes below, and any
/// IPv6 form that embeds an IPv4 address, are refused.
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => public_v4(v4),
            None => public_v6(v6),
        },
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0)
        || (a == 198 && (18..20).contains(&b))
        || a >= 240)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let [s0, s1, s2, s3, s4, s5, ..] = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        // unique local fc00::/7, link-local fe80::/10, site-local fec0::/10
        || (s0 & 0xfe00) == 0xfc00
        || (s0 & 0xffc0) == 0xfe80
        || (s0 & 0xffc0) == 0xfec0
        // documentation 2001:db8::/32, Teredo 2001::/32, 6to4 2002::/16
        || (s0 == 0x2001 && (s1 == 0x0db8 || s1 == 0))
        || s0 == 0x2002
        // NAT64 64:ff9b::/96 and local-use 64:ff9b:1::/48
        || (s0 == 0x0064 && s1 == 0xff9b && (s2 == 0 || s2 == 1))
        // IPv4-compatible ::a.b.c.d and IPv4-translated ::ffff:0:a.b.c.d
        || ((s0 | s1 | s2 | s3 | s4) == 0 && (s5 == 0 || s5 == 0xffff))
        || ((s0 | s1 | s2 | s3) == 0 && s4 == 0xffff && s5 == 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_global_unicast_is_public() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "198.18.0.1",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::a00:1",
            "fec0::1",
            "::a00:1",
            "::ffff:0:a00:1",
            "2002:a00:1::1",
            "2001:0:4136:e378::1",
        ] {
            assert!(!is_public(private.parse().expect("ip")), "{private}");
        }
        for public in ["1.1.1.1", "8.8.8.8", "2606:4700::1111", "::ffff:1.1.1.1"] {
            assert!(is_public(public.parse().expect("ip")), "{public}");
        }
    }
}
