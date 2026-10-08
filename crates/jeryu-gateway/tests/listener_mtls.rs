//! The node-facing listener only talks to an enrolled node: a client
//! certificate from the node CA reaches the gateway; no certificate, or one
//! from a stranger's CA, never gets an HTTP answer. Certificates are minted
//! per run, so no key material lives in the repository.

use std::sync::Arc;

use jeryu_gateway::listener::{self, TlsSetupError};
use jeryu_gateway::{
    Binding, CredentialSource, Gateway, RelayError, UpstreamAuth, UpstreamTable, Verifier,
};
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair};

struct NoVault;

impl CredentialSource for NoVault {
    fn authorize(&self, _: &Binding) -> Result<UpstreamAuth, RelayError> {
        Err(RelayError::ProviderNotConnected)
    }
}

struct Ca {
    pem: String,
    issuer: Issuer<'static, KeyPair>,
}

fn ca(name: &str) -> Ca {
    let key = KeyPair::generate().expect("ca key");
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name);
    let cert = params.self_signed(&key).expect("ca cert");
    Ca {
        pem: cert.pem(),
        issuer: Issuer::new(params, key),
    }
}

fn leaf(ca: &Ca, names: &[&str], usage: ExtendedKeyUsagePurpose) -> (String, String) {
    let key = KeyPair::generate().expect("leaf key");
    let mut params = CertificateParams::new(
        names
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>(),
    )
    .expect("params");
    params.extended_key_usages = vec![usage];
    let cert = params.signed_by(&key, &ca.issuer).expect("leaf cert");
    (cert.pem(), key.serialize_pem())
}

async fn start(node_ca: &Ca, server_ca: &Ca) -> std::net::SocketAddr {
    let (cert, key) = leaf(
        server_ca,
        &["localhost"],
        ExtendedKeyUsagePurpose::ServerAuth,
    );
    let tls = listener::server_config(cert.as_bytes(), key.as_bytes(), node_ca.pem.as_bytes())
        .expect("tls config");
    let gateway = Gateway::new(
        Verifier::new(Vec::new()),
        UpstreamTable::production(),
        Arc::new(NoVault),
    )
    .expect("gateway");
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = socket.local_addr().expect("addr");
    tokio::spawn(listener::serve(socket, tls, Arc::new(gateway)));
    addr
}

fn client(server_ca: &Ca, identity: Option<(String, String)>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .tls_certs_only([reqwest::Certificate::from_pem(server_ca.pem.as_bytes()).expect("root")])
        .resolve("localhost", "127.0.0.1:0".parse().expect("addr"));
    if let Some((cert, key)) = identity {
        let pem = format!("{cert}{key}");
        builder = builder.identity(reqwest::Identity::from_pem(pem.as_bytes()).expect("identity"));
    }
    builder.build().expect("client")
}

#[tokio::test]
async fn only_an_enrolled_node_certificate_reaches_the_gateway() {
    let node_ca = ca("jeryu node ca");
    let server_ca = ca("jeryu gateway ca");
    let stranger_ca = ca("stranger ca");
    let addr = start(&node_ca, &server_ca).await;
    let url = format!(
        "https://localhost:{}/internal/gateway/sess-1/claude",
        addr.port()
    );

    let node = client(
        &server_ca,
        Some(leaf(
            &node_ca,
            &["node-1"],
            ExtendedKeyUsagePurpose::ClientAuth,
        )),
    );
    let answer = node
        .post(&url)
        .body("")
        .send()
        .await
        .expect("enrolled node is served");
    assert_eq!(answer.status(), 200);
    let body = answer.text().await.expect("body");
    // The request reached the relay, which refused the empty stream.
    assert!(body.contains("relay_stream_invalid"), "{body}");

    let anonymous = client(&server_ca, None);
    assert!(anonymous.post(&url).body("").send().await.is_err());
    let stranger = client(
        &server_ca,
        Some(leaf(
            &stranger_ca,
            &["node-x"],
            ExtendedKeyUsagePurpose::ClientAuth,
        )),
    );
    assert!(stranger.post(&url).body("").send().await.is_err());
}

#[test]
fn malformed_tls_material_is_refused_with_a_typed_code() {
    let node_ca = ca("jeryu node ca");
    let (cert, key) = leaf(
        &node_ca,
        &["localhost"],
        ExtendedKeyUsagePurpose::ServerAuth,
    );
    let refuse = |cert: &str, key: &str, ca: &str| {
        listener::server_config(cert.as_bytes(), key.as_bytes(), ca.as_bytes()).err()
    };
    assert_eq!(
        refuse("", &key, &node_ca.pem),
        Some(TlsSetupError::Certificate)
    );
    assert_eq!(
        refuse(&cert, "", &node_ca.pem),
        Some(TlsSetupError::PrivateKey)
    );
    assert_eq!(refuse(&cert, &key, ""), Some(TlsSetupError::ClientCa));
    assert!(refuse(&cert, &key, &node_ca.pem).is_none());
    let message = TlsSetupError::PrivateKey.to_string();
    assert!(message.starts_with("gateway_tls_key_invalid"));
}
