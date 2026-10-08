//! The gateway's upstream HTTPS client, and the startup error it can raise.

use std::sync::Arc;

/// The gateway could not start. This is a startup failure, not a relay
/// outcome, so it is not a `RelayError` wire code: it names what failed and
/// carries the underlying cause (for example, no CA store on the host for the
/// HTTPS client).
#[derive(Debug, thiserror::Error)]
pub enum GatewayInitError {
    #[error("gateway_client_unavailable: the upstream HTTPS client could not be built: {0}")]
    Client(String),
}

/// The upstream controls every gateway client carries. Redirects are never
/// followed (a redirect is `upstream_host_denied`). Provider hosts resolve to
/// public addresses only, and the gateway dials them itself: proxy
/// environment variables are ignored, so a proxy can never stand in for the
/// public-address check. Trust roots stay whatever `builder` already has.
pub(crate) fn hardened(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(crate::upstream_dns::PublicOnly))
}

/// Build the upstream client, keeping the builder's own error and its causes.
pub(crate) fn upstream_client(
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::Client, GatewayInitError> {
    builder.build().map_err(|error| {
        let mut cause = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(inner) = source {
            cause.push_str(": ");
            cause.push_str(&inner.to_string());
            source = inner.source();
        }
        GatewayInitError::Client(cause)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trust root of the test's own, so the success path never reads the
    /// host's CA store (the Firecracker TEST guest has none).
    fn own_root() -> reqwest::Certificate {
        let key = rcgen::KeyPair::generate().expect("key");
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let cert = params.self_signed(&key).expect("ca");
        reqwest::Certificate::from_pem(cert.pem().as_bytes()).expect("root")
    }

    /// A client that cannot be built is reported as such, with its cause,
    /// and never as `upstream_host_denied`. The failing builder carries every
    /// production control, so only the header value differs.
    #[test]
    fn a_client_build_failure_names_its_cause() {
        let broken = hardened(reqwest::Client::builder().user_agent("bad\nagent"));
        let error = upstream_client(broken).expect_err("an invalid header value fails build()");
        let text = error.to_string();
        assert!(text.starts_with("gateway_client_unavailable: "), "{text}");
        assert!(text.contains("builder error"), "the cause survives: {text}");
        assert!(!text.contains("upstream_host_denied") && !text.contains("UpstreamHostDenied"));
    }

    /// The hardened builder builds, with no proxy, no redirects and the
    /// public resolver, when it is given its trust roots explicitly.
    #[test]
    fn the_hardened_client_builds_without_a_system_ca_store() {
        let builder = hardened(reqwest::Client::builder().tls_certs_only([own_root()]));
        assert!(upstream_client(builder).is_ok());
    }
}
