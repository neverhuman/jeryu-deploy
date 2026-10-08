//! The node-facing mTLS listener (connection-v1 §6). Only a client presenting
//! a certificate from the enrolled node CA completes the handshake, and only
//! TLS 1.3 is offered. The leaf certificate's digest becomes the request's
//! [`PeerIdentity`], which phase 0 maps to one enrolled node and that node's
//! capability key. This is the only place the gateway router is mounted.

use std::sync::Arc;
use std::time::Duration;

use rustls::RootCertStore;
pub use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;

use crate::{Gateway, PeerIdentity};

/// Connections served at once; the next one waits in the kernel backlog.
pub const MAX_CONNECTIONS: usize = 64;
/// A peer that has not finished the TLS handshake by then is dropped.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the listener's TLS material was refused. Never carries key bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TlsSetupError {
    #[error(
        "gateway_tls_certificate_invalid: the server certificate chain is not PEM certificates"
    )]
    Certificate,
    #[error("gateway_tls_key_invalid: the server private key is not one PEM private key")]
    PrivateKey,
    #[error("gateway_tls_client_ca_invalid: the node CA is not PEM certificates")]
    ClientCa,
    #[error("gateway_tls_config_invalid: rustls refused the certificate, key, or CA")]
    Config,
}

/// The listener's TLS configuration. Only [`server_config`] builds one, so a
/// listener can never run without a required node client certificate.
#[derive(Clone)]
pub struct NodeTls(Arc<ServerConfig>);

/// TLS 1.3, server certificate `cert_pem` with `key_pem`, and a required client
/// certificate chaining to `client_ca_pem`.
pub fn server_config(
    cert_pem: &[u8],
    key_pem: &[u8],
    client_ca_pem: &[u8],
) -> Result<NodeTls, TlsSetupError> {
    let certificates = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| TlsSetupError::Certificate)?;
    if certificates.is_empty() {
        return Err(TlsSetupError::Certificate);
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|_| TlsSetupError::PrivateKey)?;
    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_slice_iter(client_ca_pem) {
        roots
            .add(ca.map_err(|_| TlsSetupError::ClientCa)?)
            .map_err(|_| TlsSetupError::ClientCa)?;
    }
    if roots.is_empty() {
        return Err(TlsSetupError::ClientCa);
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|_| TlsSetupError::ClientCa)?;
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| TlsSetupError::Config)?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificates, key)
        .map_err(|_| TlsSetupError::Config)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(NodeTls(Arc::new(config)))
}

/// Serve the gateway router on `listener` until the task is dropped.
pub async fn serve(
    listener: TcpListener,
    tls: NodeTls,
    gateway: Arc<Gateway>,
) -> std::io::Result<()> {
    let acceptor = TlsAcceptor::from(tls.0);
    let app = crate::router(gateway);
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let slot = slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| std::io::Error::other("gateway listener closed"))?;
        let (socket, _) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let Ok(Ok(tls)) =
                tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(socket)).await
            else {
                return;
            };
            // The verifier required a client certificate; the relay maps its
            // digest to exactly one enrolled node.
            let Some(peer) = tls
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|chain| chain.first())
                .map(|leaf| PeerIdentity::verified(leaf.as_ref()))
            else {
                return;
            };
            let app = app.layer(axum::Extension(peer));
            let service = hyper_util::service::TowerToHyperService::new(app);
            // The relay bounds each request with its own idle timeout, so a
            // streaming response may outlive any fixed connection deadline.
            let _ = hyper::server::conn::http1::Builder::new()
                .max_buf_size(64 * 1024)
                .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                .await;
        });
    }
}
