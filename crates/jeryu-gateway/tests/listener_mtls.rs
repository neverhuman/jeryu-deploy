//! The node-facing listener only talks to the node CA: no certificate, or one
//! from a stranger's CA, never gets an HTTP answer. The leaf certificate is
//! the caller's identity for phase 0: the enrolled node's call passes it,
//! while another certificate from the same CA is `capability_unverified`.
//! Certificates are minted per run, so no key material lives in the
//! repository.

use std::sync::Arc;

use jeryu_gateway::listener::{self, TlsSetupError};
use jeryu_gateway::{
    Assignment, Binding, Capability, CertificateFingerprint, CredentialSource, EnrolledNode,
    Gateway, LeaseBinding, Mode, NodeRegistry, OwnerKind, RelayError, SessionAuthority,
    UpstreamAuth, UpstreamTable,
};
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair};
use ring::signature::{Ed25519KeyPair, KeyPair as _};

struct NoVault;

impl CredentialSource for NoVault {
    fn check(&self, _: &Binding) -> Result<(), RelayError> {
        Err(RelayError::ProviderNotConnected)
    }

    fn credential(&self, _: &Binding) -> Result<UpstreamAuth, RelayError> {
        Err(RelayError::ProviderNotConnected)
    }
}

/// One session, assigned to `node-1`, leased far into the future.
struct OneSession;

impl SessionAuthority for OneSession {
    fn assignment(&self, session_id: &str) -> Option<Assignment> {
        (session_id == "sess-1").then(|| Assignment {
            runner_id: "node-1".into(),
            session_id: "sess-1".into(),
            session_generation: 1,
            lease_id: "lease-1".into(),
            lease_epoch: 1,
            owner_id: "user/alice".into(),
            lease_expires_at_unix_millis: u64::MAX,
            bindings: vec![LeaseBinding {
                provider: "claude".into(),
                connection_id: "pc_1".into(),
                connection_generation: 1,
                mode: Mode::GatewayHelper,
            }],
        })
    }
}

fn der_of(pem: &str) -> Vec<u8> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    CertificateDer::from_pem_slice(pem.as_bytes())
        .expect("der")
        .as_ref()
        .to_vec()
}

fn open_line(key: &Ed25519KeyPair) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    let mut capability = Capability {
        capability_id: "cap-1".into(),
        connection_id: "pc_1".into(),
        connection_generation: 1,
        provider: "claude".into(),
        mode: Mode::GatewayHelper,
        owner_kind: OwnerKind::User,
        owner_id: "user/alice".into(),
        session_id: "sess-1".into(),
        session_generation: 1,
        lease_id: "lease-1".into(),
        lease_epoch: 1,
        audience: "jeryu.gateway".into(),
        issued_at_unix_millis: now,
        expires_at_unix_millis: now + 30_000,
        signature: String::new(),
    };
    let message = jeryu_gateway::capability::signing_bytes(&capability).expect("bytes");
    capability.signature = key
        .sign(&message)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let open = serde_json::json!({
        "type": "gateway-open", "request_id": "r-1", "provider": "claude",
        "method": "POST", "path": "/v1/messages", "headers": [],
        "capability": capability,
    });
    format!(
        "{open}
"
    )
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

async fn start(
    server_ca: &Ca,
    node_ca: &Ca,
    enrolled: &[(CertificateFingerprint, [u8; 32])],
) -> std::net::SocketAddr {
    let (cert, key) = leaf(
        server_ca,
        &["localhost"],
        ExtendedKeyUsagePurpose::ServerAuth,
    );
    let tls = listener::server_config(cert.as_bytes(), key.as_bytes(), node_ca.pem.as_bytes())
        .expect("tls config");
    let mut registry = NodeRegistry::default();
    for (peer, key) in enrolled {
        registry.enroll(
            *peer,
            EnrolledNode {
                runner_id: "node-1".into(),
                capability_key: *key,
            },
        );
    }
    let gateway = Gateway::new(
        registry,
        Arc::new(OneSession),
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
async fn the_peer_certificate_is_the_phase_zero_identity() {
    let node_ca = ca("jeryu node ca");
    let server_ca = ca("jeryu gateway ca");
    let stranger_ca = ca("stranger ca");
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("key");
    let node_key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("pair");
    let public: [u8; 32] = node_key.public_key().as_ref().try_into().expect("32");
    let node_cert = leaf(&node_ca, &["node-1"], ExtendedKeyUsagePurpose::ClientAuth);
    let peer = CertificateFingerprint::of_der(&der_of(&node_cert.0));
    let addr = start(&server_ca, &node_ca, &[(peer, public)]).await;
    let url = format!(
        "https://localhost:{}/internal/gateway/sess-1/claude",
        addr.port()
    );

    // The enrolled node's own certificate and key pass phase 0; the call
    // reaches the vault's metadata check, which this vault refuses.
    let node = client(&server_ca, Some(node_cert));
    let answer = node
        .post(&url)
        .body(open_line(&node_key))
        .send()
        .await
        .expect("enrolled node is served");
    assert_eq!(answer.status(), 200);
    let body = answer.text().await.expect("body");
    assert!(body.contains("provider_not_connected"), "{body}");

    // Same CA, same signing key, a certificate that is not enrolled.
    let other = client(
        &server_ca,
        Some(leaf(
            &node_ca,
            &["node-2"],
            ExtendedKeyUsagePurpose::ClientAuth,
        )),
    );
    let body = other
        .post(&url)
        .body(open_line(&node_key))
        .send()
        .await
        .expect("CA-valid peer is served")
        .text()
        .await
        .expect("body");
    assert!(body.contains("capability_unverified"), "{body}");

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
