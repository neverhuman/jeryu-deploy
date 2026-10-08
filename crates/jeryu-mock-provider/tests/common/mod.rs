//! Shared CI-A harness for the provider gateway: the mock provider, a node
//! registry with two enrolled nodes, a mutable control-plane assignment, and
//! a vault that counts every metadata check and every credential retrieval.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use bytes::Bytes;
use futures_util::StreamExt;
use jeryu_gateway::frames::{GatewayLine, encode_chunk};
use jeryu_gateway::{
    Assignment, Binding, Capability, CredentialSource, EnrolledNode, Gateway, LeaseBinding, Mode,
    NodeRegistry, OwnerKind, PeerIdentity, RelayError, SessionAuthority, Upstream, UpstreamAuth,
    UpstreamTable,
};
use jeryu_mock_provider::{MockConfig, MockProvider};
use parking_lot::Mutex;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// The vault, for these tests: one connection whose generation the test
/// moves, with a count of every call.
pub struct CountingVault {
    pub generation: AtomicU64,
    pub owner: Mutex<String>,
    pub disabled: Mutex<bool>,
    pub access_token: String,
    pub checks: AtomicUsize,
    pub credentials: AtomicUsize,
}

impl CountingVault {
    fn metadata(&self, binding: &Binding) -> Result<(), RelayError> {
        if binding.owner_id != *self.owner.lock() || binding.connection_id != "pc_1" {
            return Err(RelayError::ConnectionOwnerMismatch);
        }
        let live = self.generation.load(Ordering::SeqCst);
        if live == 0 {
            return Err(RelayError::ProviderNotConnected);
        }
        if binding.connection_generation != live {
            return Err(RelayError::ConnectionGenerationStale);
        }
        if *self.disabled.lock() {
            return Err(RelayError::ProviderModeDisabled);
        }
        Ok(())
    }
}

impl CredentialSource for CountingVault {
    fn check(&self, binding: &Binding) -> Result<(), RelayError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        self.metadata(binding)
    }

    fn credential(&self, binding: &Binding) -> Result<UpstreamAuth, RelayError> {
        self.credentials.fetch_add(1, Ordering::SeqCst);
        self.metadata(binding)?;
        Ok(UpstreamAuth {
            header: "authorization".into(),
            value: zeroize::Zeroizing::new(format!("Bearer {}", self.access_token)),
        })
    }
}

/// The control plane's session store, editable by the test.
pub struct Sessions(pub Mutex<Option<Assignment>>);

impl SessionAuthority for Sessions {
    fn assignment(&self, session_id: &str) -> Option<Assignment> {
        self.0
            .lock()
            .clone()
            .filter(|assignment| assignment.session_id == session_id)
    }
}

pub static NOW: AtomicU64 = AtomicU64::new(1_000_000);

pub fn clock() -> u64 {
    NOW.load(Ordering::SeqCst)
}

pub struct Node {
    pub peer: PeerIdentity,
    key: Ed25519KeyPair,
}

fn node(registry: &mut NodeRegistry, digest: u8, runner_id: &str) -> Node {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("key");
    let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("pair");
    let peer = PeerIdentity::from_sha256([digest; 32]);
    registry.enroll(
        peer,
        EnrolledNode {
            runner_id: runner_id.into(),
            capability_key: key.public_key().as_ref().try_into().expect("32"),
        },
    );
    Node { peer, key }
}

pub struct Harness {
    _provider: MockProvider,
    pub provider_url: String,
    pub gateway: Arc<Gateway>,
    pub vault: Arc<CountingVault>,
    pub sessions: Arc<Sessions>,
    /// The node the session is assigned to.
    pub node_a: Node,
    /// Another enrolled node, not assigned to the session.
    pub node_b: Node,
}

pub fn assignment() -> Assignment {
    Assignment {
        runner_id: "node-a".into(),
        session_id: "sess-1".into(),
        session_generation: 1,
        lease_id: "lease-1".into(),
        lease_epoch: 1,
        owner_id: "user/alice".into(),
        lease_expires_at_unix_millis: clock() + 3_600_000,
        bindings: vec![LeaseBinding {
            provider: "claude".into(),
            connection_id: "pc_1".into(),
            connection_generation: 1,
            mode: Mode::GatewayHelper,
        }],
    }
}

impl Harness {
    pub async fn new() -> Self {
        let provider = MockProvider::start(MockConfig::default())
            .await
            .expect("mock");
        let access_token = mint_access_token(&provider).await;
        let mut registry = NodeRegistry::default();
        let node_a = node(&mut registry, 0xa1, "node-a");
        let node_b = node(&mut registry, 0xb2, "node-b");
        let vault = Arc::new(CountingVault {
            generation: AtomicU64::new(1),
            owner: Mutex::new("user/alice".into()),
            disabled: Mutex::new(false),
            access_token,
            checks: AtomicUsize::new(0),
            credentials: AtomicUsize::new(0),
        });
        let sessions = Arc::new(Sessions(Mutex::new(Some(assignment()))));
        let mut table = UpstreamTable::production();
        let base_url = format!("http://{}", provider.addr);
        table
            .loopback(
                "claude",
                Upstream {
                    base_url: base_url.clone(),
                    path_prefixes: vec!["/v1/messages".into()],
                    fixed_headers: Vec::new(),
                },
            )
            .expect("loopback upstream");
        let source: Arc<dyn CredentialSource> = vault.clone();
        let authority: Arc<dyn SessionAuthority> = sessions.clone();
        let gateway = Gateway::new(registry, authority, table, source)
            .expect("gateway")
            .with_clock(clock);
        Self {
            _provider: provider,
            provider_url: base_url,
            gateway: Arc::new(gateway),
            vault,
            sessions,
            node_a,
            node_b,
        }
    }

    /// A capability for the stored lease, signed by `signer`, edited first.
    pub fn capability_by(&self, signer: &Node, edit: impl FnOnce(&mut Capability)) -> Capability {
        let now = clock();
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
        edit(&mut capability);
        let message = jeryu_gateway::capability::signing_bytes(&capability).expect("bytes");
        capability.signature = signer
            .key
            .sign(&message)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        capability
    }

    /// A capability from the assigned node.
    pub fn capability(&self, edit: impl FnOnce(&mut Capability)) -> Capability {
        self.capability_by(&self.node_a, edit)
    }

    pub fn calls(&self) -> (usize, usize) {
        (
            self.vault.checks.load(Ordering::SeqCst),
            self.vault.credentials.load(Ordering::SeqCst),
        )
    }

    /// Relay a fully known request from `peer` and collect every line.
    pub async fn relay_from(
        &self,
        peer: Option<PeerIdentity>,
        lines: Vec<Bytes>,
    ) -> Vec<GatewayLine> {
        let input = futures_util::stream::iter(lines.into_iter().map(Ok::<_, std::io::Error>));
        self.gateway
            .relay(peer, "sess-1", "claude", input)
            .map(|line| serde_json::from_slice::<GatewayLine>(&line).expect("gateway line"))
            .collect()
            .await
    }

    pub async fn relay_all(&self, lines: Vec<Bytes>) -> Vec<GatewayLine> {
        self.relay_from(Some(self.node_a.peer), lines).await
    }

    /// An open relay from the assigned node: send lines with the sender,
    /// read the gateway's answer from the stream.
    pub fn relay_open(
        &self,
    ) -> (
        mpsc::Sender<Result<Bytes, std::io::Error>>,
        impl futures_util::Stream<Item = Bytes>,
    ) {
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
        let input =
            futures_util::stream::unfold(
                rx,
                |mut rx| async move { rx.recv().await.map(|i| (i, rx)) },
            );
        let output =
            self.gateway
                .relay(Some(self.node_a.peer), "sess-1", "claude", Box::pin(input));
        (tx, output)
    }

    pub async fn counters(&self) -> Value {
        reqwest::get(format!("{}/counters", self.provider_url))
            .await
            .expect("counters")
            .json()
            .await
            .expect("json")
    }

    /// Wait until the gateway holds `count` in-flight slots for the session.
    /// A slot is taken only after every open check passed, so this is the
    /// deterministic "the open was accepted" signal.
    pub async fn until_in_flight(&self, count: usize) {
        for _ in 0..500 {
            if self.gateway.in_flight("sess-1") == count {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("gateway never reached {count} in-flight requests");
    }
}

async fn mint_access_token(provider: &MockProvider) -> String {
    let http = reqwest::Client::new();
    let post = |path: &str, body: Value| http.post(provider.url(path)).json(&body).send();
    let device: Value = post("/oauth/device", json!({"provider": "claude"}))
        .await
        .expect("device")
        .json()
        .await
        .expect("json");
    post(
        "/oauth/device/approve",
        json!({"user_code": device["user_code"]}),
    )
    .await
    .expect("approve");
    let grant = "urn:ietf:params:oauth:grant-type:device_code";
    let device_code = device["device_code"].as_str().expect("code").to_string();
    let tokens: Value = http
        .post(provider.url("/oauth/token"))
        .form(&[("grant_type", grant), ("device_code", device_code.as_str())])
        .send()
        .await
        .expect("token")
        .json()
        .await
        .expect("json");
    tokens["access_token"].as_str().expect("access").to_string()
}

pub fn open_line(request_id: &str, capability: &Capability, authorization: Option<&str>) -> Bytes {
    let mut open = json!({
        "type": "gateway-open", "request_id": request_id, "provider": "claude",
        "method": "POST", "path": "/v1/messages",
        "headers": [{"name": "content-type", "value": "application/json"}],
        "capability": capability,
    });
    if let Some(authorization) = authorization {
        open["authorization"] = json!(authorization);
    }
    line(open)
}

pub fn line(value: Value) -> Bytes {
    let mut bytes = serde_json::to_vec(&value).expect("json");
    bytes.push(b'\n');
    Bytes::from(bytes)
}

pub fn body_line(request_id: &str, seq: u64, body: &[u8]) -> Bytes {
    line(
        json!({"type": "request-body", "request_id": request_id, "seq": seq,
        "bytes_b64": encode_chunk(body)}),
    )
}

pub fn end_line(request_id: &str, seq_end: u64) -> Bytes {
    line(json!({"type": "request-end", "request_id": request_id, "seq_end": seq_end}))
}

pub fn body_lines(request_id: &str, body: &[u8]) -> Vec<Bytes> {
    vec![body_line(request_id, 1, body), end_line(request_id, 1)]
}

pub fn error_of(lines: &[GatewayLine]) -> Option<RelayError> {
    lines.iter().find_map(|line| match line {
        GatewayLine::Error { error, .. } => Some(*error),
        _ => None,
    })
}

pub fn has_body(lines: &[GatewayLine]) -> bool {
    lines
        .iter()
        .any(|line| matches!(line, GatewayLine::ResponseBody { .. }))
}
