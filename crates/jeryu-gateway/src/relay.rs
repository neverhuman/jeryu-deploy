//! One relay request through the gateway: `gateway-open`, phase 0, the
//! connection-v1 §4 checks in order, then the upstream call with the vault
//! credential, streaming both bodies. The credential is retrieved once, after
//! every open precondition. Every later line re-checks the assignment and the
//! connection metadata without retrieving it again.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::RelayError;
use crate::authority::{self, NodeRegistry, PeerIdentity, SessionAuthority};
use crate::body::{Accepted, request_body};
use crate::capability::{Capability, Mode, verify_validity};
use crate::client::{GatewayInitError, hardened, upstream_client};
use crate::frames::{GatewayLine, NodeLine};
use crate::policy::{UpstreamTable, check_headers};
use crate::stream::{LineReader, Slot, now_unix_millis};
use crate::upstream::{Prepared, forward, send};

/// Relay requests in flight per session; the next `gateway-open` is busy.
pub const MAX_IN_FLIGHT: usize = 4;
/// No frame on a request for this long, in either direction, ends it with
/// `relay_idle_timeout`. That includes a node that stops reading the response.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// A request body streams upstream; this bounds its total size.
pub const MAX_REQUEST_BODY: usize = 8 * 1024 * 1024;
/// While a request is open, the assignment and connection are re-checked at
/// least this often, even when no line moves in either direction.
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(1);

/// What the gateway asks the vault about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub owner_id: String,
    pub provider: String,
    pub connection_id: String,
    pub connection_generation: u64,
}

impl Binding {
    fn of(capability: &Capability) -> Self {
        Self {
            owner_id: capability.owner_id.clone(),
            provider: capability.provider.clone(),
            connection_id: capability.connection_id.clone(),
            connection_generation: capability.connection_generation,
        }
    }
}

/// The header the gateway adds upstream. The value never leaves the gateway.
pub struct UpstreamAuth {
    pub header: String,
    pub value: Zeroizing<String>,
}

/// The vault side of the gateway. Implemented over the core vault in the
/// control plane; tests use an in-memory source.
pub trait CredentialSource: Send + Sync + 'static {
    /// Metadata only (connection-v1 §4 precedence 2 to 5): the owner holds
    /// the connection, it is connected at this generation, and its mode is
    /// enabled. Never reads, unseals, or refreshes secret material.
    fn check(&self, binding: &Binding) -> Result<(), RelayError>;
    /// The credential for one accepted request. Called once per request,
    /// after every open precondition has passed.
    fn credential(&self, binding: &Binding) -> Result<UpstreamAuth, RelayError>;
}

pub struct Gateway {
    pub(crate) registry: NodeRegistry,
    pub(crate) authority: Arc<dyn SessionAuthority>,
    table: UpstreamTable,
    pub(crate) credentials: Arc<dyn CredentialSource>,
    pub(crate) client: reqwest::Client,
    slots: crate::stream::Slots,
    pub(crate) clock: fn() -> u64,
}

impl Gateway {
    pub fn new(
        registry: NodeRegistry,
        authority: Arc<dyn SessionAuthority>,
        table: UpstreamTable,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Self, GatewayInitError> {
        // No proxy, no redirects, public-only DNS: see `client::hardened`.
        let client = upstream_client(hardened(reqwest::Client::builder()))?;
        Ok(Self {
            registry,
            authority,
            table,
            credentials,
            client,
            slots: Arc::default(),
            clock: now_unix_millis,
        })
    }

    /// Relay requests currently holding an in-flight slot for `session_id`.
    #[must_use]
    pub fn in_flight(&self, session_id: &str) -> usize {
        self.slots.lock().get(session_id).copied().unwrap_or(0)
    }

    /// Replace the clock. Test builds only: production always reads the
    /// wall clock.
    #[cfg(any(test, feature = "test-harness"))]
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> u64) -> Self {
        self.clock = clock;
        self
    }

    /// Relay one request from the mTLS `peer`. `input` is the node's NDJSON
    /// body; the returned stream is the NDJSON response, one line per item.
    pub fn relay<S, E>(
        self: &Arc<Self>,
        peer: Option<PeerIdentity>,
        session_id: &str,
        provider: &str,
        input: S,
    ) -> impl Stream<Item = Bytes> + use<S, E>
    where
        S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
        E: Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<Bytes>(16);
        let gateway = self.clone();
        let (session_id, provider) = (session_id.to_string(), provider.to_string());
        tokio::spawn(async move {
            let mut request_id = String::new();
            let lines = LineReader::new(input);
            let outcome = gateway
                .run(peer, &session_id, &provider, lines, &tx, &mut request_id)
                .await;
            if let Err(error) = outcome {
                if request_id.is_empty() {
                    request_id = "unknown".to_string();
                }
                let _ = send(&tx, &GatewayLine::Error { request_id, error }).await;
            }
        });
        futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|line| (line, rx))
        })
    }

    async fn run<S, E>(
        self: &Arc<Self>,
        peer: Option<PeerIdentity>,
        session_id: &str,
        provider: &str,
        mut lines: LineReader<S>,
        tx: &mpsc::Sender<Bytes>,
        request_id_out: &mut String,
    ) -> Result<(), RelayError>
    where
        S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
        E: Send + 'static,
    {
        if !matches!(provider, "claude" | "codex" | "grok") {
            return Err(RelayError::ProviderUnknown);
        }
        let NodeLine::GatewayOpen {
            request_id,
            provider: line_provider,
            method,
            path,
            headers,
            authorization,
            capability,
        } = lines.next_line().await?
        else {
            return Err(RelayError::RelayStreamInvalid);
        };
        *request_id_out = request_id.clone();
        if line_provider != provider {
            return Err(RelayError::RelayStreamInvalid);
        }
        let now = (self.clock)();
        // Phase 0: nothing below runs, and the vault is not asked, unless the
        // peer, its key, the assignment, and the stored lease all match.
        let (runner_id, lease_expires) = authority::verify(
            &self.registry,
            self.authority.as_ref(),
            peer.as_ref(),
            &capability,
            (session_id, provider),
            now,
        )?;
        let binding = Binding::of(&capability);
        // Precedence 2 to 5, metadata only.
        self.credentials.check(&binding)?;
        if capability.mode == Mode::QualifiedRamToken {
            return Err(RelayError::ProviderModeUnqualified);
        }
        if authorization.is_some() {
            return Err(RelayError::GuestAuthorizationRejected);
        }
        verify_validity(&capability, now, lease_expires)?;
        let (url, upstream) = self.table.resolve(provider, &path)?;
        check_headers(&headers)?;
        let method = match method.as_str() {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            _ => return Err(RelayError::RelayStreamInvalid),
        };
        let _slot = Slot::take(&self.slots, session_id)?;
        // Every open precondition has passed: retrieve the credential, once.
        let auth = self.credentials.credential(&binding)?;

        let accepted = Arc::new(Accepted::new(self.clone(), runner_id, *capability, binding));
        let failure = Arc::default();
        let body = request_body(
            lines,
            request_id.clone(),
            accepted.clone(),
            Arc::clone(&failure),
        );
        let prepared = Prepared {
            method,
            url,
            headers,
            fixed_headers: upstream.fixed_headers.clone(),
            auth,
        };
        // The request ends at the first of: done, a failed periodic re-check
        // (even while upstream is idle or the node stops reading), or the
        // node hanging up. Dropping the forward future cancels the upstream
        // call and frees the slot.
        tokio::select! {
            outcome = forward(&self.client, prepared, body, &failure, &accepted, &request_id, tx) => outcome,
            error = watch(&accepted) => Err(error),
            () = tx.closed() => Err(RelayError::RelayStreamInvalid),
        }
    }
}

/// Re-check the accepted request every [`RECHECK_INTERVAL`] until it fails.
/// The store and vault traits are synchronous, so each check runs on the
/// blocking pool: a durable store doing I/O cannot stall the runtime.
async fn watch(accepted: &Arc<Accepted>) -> RelayError {
    loop {
        tokio::time::sleep(RECHECK_INTERVAL).await;
        let accepted = accepted.clone();
        match tokio::task::spawn_blocking(move || accepted.recheck()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return error,
            Err(_) => return RelayError::LeaseBindingMismatch,
        }
    }
}
