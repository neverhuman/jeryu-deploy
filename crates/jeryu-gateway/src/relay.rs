//! One relay request through the gateway: `gateway-open`, request body,
//! upstream call with the vault credential, response streamed back. The live
//! connection generation is re-checked before every body line in both
//! directions, so a disconnect or credential replacement cuts the response.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::RelayError;
use crate::capability::{Mode, Verifier};
use crate::frames::{GatewayLine, Header, MAX_CHUNK_BYTES, MAX_LINE_BYTES, NodeLine};
use crate::policy::{UpstreamTable, check_headers};

/// Relay requests in flight per session; the next `gateway-open` is busy.
pub const MAX_IN_FLIGHT: usize = 4;
/// No frame on a request for this long ends it with `relay_idle_timeout`.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Request bodies are buffered before the upstream call; this bounds them.
pub const MAX_REQUEST_BODY: usize = 8 * 1024 * 1024;

/// What the gateway asks the vault about, on every line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub owner_id: String,
    pub provider: String,
    pub connection_id: String,
    pub connection_generation: u64,
}

/// The header the gateway adds upstream. The value never leaves the gateway.
pub struct UpstreamAuth {
    pub header: String,
    pub value: Zeroizing<String>,
}

/// The vault side of the gateway: confirms a binding is the live connection
/// generation and yields the credential to inject. Implemented over the core
/// vault in the control plane; tests use an in-memory source.
pub trait CredentialSource: Send + Sync + 'static {
    fn authorize(&self, binding: &Binding) -> Result<UpstreamAuth, RelayError>;
}

pub struct Gateway {
    verifier: Verifier,
    table: UpstreamTable,
    credentials: Arc<dyn CredentialSource>,
    client: reqwest::Client,
    slots: Arc<parking_lot::Mutex<HashMap<String, usize>>>,
    clock: fn() -> u64,
}

impl Gateway {
    pub fn new(
        verifier: Verifier,
        table: UpstreamTable,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Self, RelayError> {
        // Redirects are never followed: a redirect is upstream_host_denied.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| RelayError::UpstreamHostDenied)?;
        Ok(Self {
            verifier,
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

    /// Replace the clock (tests).
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> u64) -> Self {
        self.clock = clock;
        self
    }

    /// Relay one request. `input` is the node's NDJSON body; the returned
    /// stream is the NDJSON response, one line per item.
    pub fn relay<S, E>(
        self: &Arc<Self>,
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
            let mut lines = LineReader::new(input);
            let mut request_id = String::new();
            let outcome = gateway
                .run(&session_id, &provider, &mut lines, &tx, &mut request_id)
                .await;
            if let Err(error) = outcome {
                let id = if request_id.is_empty() {
                    "unknown".to_string()
                } else {
                    request_id
                };
                let line = GatewayLine::Error {
                    request_id: id,
                    error,
                }
                .to_line();
                let _ = tx.send(Bytes::from(line)).await;
            }
        });
        futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|line| (line, rx))
        })
    }

    async fn run<S, E>(
        &self,
        session_id: &str,
        provider: &str,
        lines: &mut LineReader<S>,
        tx: &mpsc::Sender<Bytes>,
        request_id_out: &mut String,
    ) -> Result<(), RelayError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
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
            return Err(RelayError::LeaseBindingMismatch);
        }
        // Signature first: nothing from an unverified capability is trusted.
        self.verifier
            .verify_binding(&capability, session_id, provider)?;
        let binding = Binding {
            owner_id: capability.owner_id.clone(),
            provider: provider.to_string(),
            connection_id: capability.connection_id.clone(),
            connection_generation: capability.connection_generation,
        };
        let auth = self.credentials.authorize(&binding)?;
        if capability.mode == Mode::QualifiedRamToken {
            return Err(RelayError::ProviderModeUnqualified);
        }
        if authorization.is_some() {
            return Err(RelayError::GuestAuthorizationRejected);
        }
        self.verifier.verify_validity(&capability, (self.clock)())?;
        let (url, upstream) = self.table.resolve(provider, &path)?;
        check_headers(&headers)?;
        let method = match method.as_str() {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            _ => return Err(RelayError::RelayStreamInvalid),
        };
        let _slot = Slot::take(&self.slots, session_id)?;

        let body = self.read_body(&request_id, &binding, lines).await?;
        let mut request = self.client.request(method, url).body(body);
        for Header { name, value } in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        for (name, value) in &upstream.fixed_headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request = request.header(auth.header.as_str(), auth.value.as_str());
        drop(auth);
        let response = tokio::time::timeout(IDLE_TIMEOUT, request.send())
            .await
            .map_err(|_| RelayError::RelayIdleTimeout)?
            .map_err(|_| RelayError::UpstreamHostDenied)?;
        if response.status().is_redirection() {
            return Err(RelayError::UpstreamHostDenied);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= 256)
            .map(|value| Header {
                name: "content-type".into(),
                value: value.into(),
            });
        let head = GatewayLine::Head {
            request_id: request_id.clone(),
            status: response.status().as_u16(),
            headers: content_type.into_iter().collect(),
        };
        send(tx, &head).await?;
        let mut stream = response.bytes_stream();
        let mut seq = 0_u64;
        loop {
            let chunk = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                Err(_) => return Err(RelayError::RelayIdleTimeout),
                Ok(None) => break,
                Ok(Some(Err(_))) => return Err(RelayError::RelayStreamInvalid),
                Ok(Some(Ok(chunk))) => chunk,
            };
            for piece in chunk.chunks(MAX_CHUNK_BYTES) {
                // A disconnect or replacement mid-response stops the body here.
                self.credentials.authorize(&binding).map(drop)?;
                seq += 1;
                let line = GatewayLine::ResponseBody {
                    request_id: request_id.clone(),
                    seq,
                    bytes_b64: crate::frames::encode_chunk(piece),
                };
                send(tx, &line).await?;
            }
        }
        send(
            tx,
            &GatewayLine::ResponseEnd {
                request_id,
                seq_end: seq,
            },
        )
        .await
    }

    async fn read_body<S, E>(
        &self,
        request_id: &str,
        binding: &Binding,
        lines: &mut LineReader<S>,
    ) -> Result<Vec<u8>, RelayError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
    {
        let mut body = Vec::new();
        let mut expected = 1_u64;
        loop {
            match lines.next_line().await? {
                NodeLine::RequestBody {
                    request_id: id,
                    seq,
                    bytes_b64,
                } => {
                    if id != request_id || seq != expected {
                        return Err(RelayError::RelayStreamInvalid);
                    }
                    self.credentials.authorize(binding).map(drop)?;
                    let chunk = crate::frames::decode_chunk(&bytes_b64)?;
                    if body.len() + chunk.len() > MAX_REQUEST_BODY {
                        return Err(RelayError::RelayStreamInvalid);
                    }
                    body.extend_from_slice(&chunk);
                    expected += 1;
                }
                NodeLine::RequestEnd {
                    request_id: id,
                    seq_end,
                } => {
                    if id != request_id || seq_end != expected - 1 {
                        return Err(RelayError::RelayStreamInvalid);
                    }
                    return Ok(body);
                }
                NodeLine::GatewayOpen { .. } => return Err(RelayError::RelayStreamInvalid),
            }
        }
    }
}

async fn send(tx: &mpsc::Sender<Bytes>, line: &GatewayLine) -> Result<(), RelayError> {
    tx.send(Bytes::from(line.to_line()))
        .await
        .map_err(|_| RelayError::RelayStreamInvalid)
}

/// A held in-flight slot for one session; released on drop.
struct Slot {
    slots: Arc<parking_lot::Mutex<HashMap<String, usize>>>,
    session_id: String,
}

impl Slot {
    fn take(
        slots: &Arc<parking_lot::Mutex<HashMap<String, usize>>>,
        session_id: &str,
    ) -> Result<Self, RelayError> {
        let mut map = slots.lock();
        let count = map.entry(session_id.to_string()).or_default();
        if *count >= MAX_IN_FLIGHT {
            return Err(RelayError::ConnectionBusy);
        }
        *count += 1;
        Ok(Self {
            slots: slots.clone(),
            session_id: session_id.to_string(),
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut map = self.slots.lock();
        if let Some(count) = map.get_mut(&self.session_id) {
            *count -= 1;
            if *count == 0 {
                map.remove(&self.session_id);
            }
        }
    }
}

/// Splits a byte stream into NDJSON lines, bounded and idle-timed.
struct LineReader<S> {
    input: S,
    buffer: Vec<u8>,
}

impl<S, E> LineReader<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    fn new(input: S) -> Self {
        Self {
            input,
            buffer: Vec::new(),
        }
    }

    async fn next_line(&mut self) -> Result<NodeLine, RelayError> {
        loop {
            if let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=end).collect();
                return crate::frames::parse_node_line(&line[..line.len() - 1]);
            }
            if self.buffer.len() > MAX_LINE_BYTES {
                return Err(RelayError::RelayStreamInvalid);
            }
            match tokio::time::timeout(IDLE_TIMEOUT, self.input.next()).await {
                Err(_) => return Err(RelayError::RelayIdleTimeout),
                Ok(None) | Ok(Some(Err(_))) => return Err(RelayError::RelayStreamInvalid),
                Ok(Some(Ok(bytes))) => self.buffer.extend_from_slice(&bytes),
            }
        }
    }
}

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}
