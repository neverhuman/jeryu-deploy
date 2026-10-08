//! CI-A connect-once relay: a node-signed capability, the jeryu-gateway, and
//! this crate's mock provider. The mock never adds a credential, so a
//! `{"injected": true}` answer proves the gateway did (k3-relay-v1, "Order the
//! CI-A fake must show").

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures_util::StreamExt;
use jeryu_gateway::frames::{GatewayLine, encode_chunk};
use jeryu_gateway::{
    Binding, Capability, CredentialSource, Gateway, Mode, OwnerKind, RelayError, Upstream,
    UpstreamAuth, UpstreamTable, Verifier,
};
use jeryu_mock_provider::{MockConfig, MockProvider};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// The vault, for this test: one connection whose generation the test moves.
struct FakeVault {
    generation: AtomicU64,
    access_token: String,
}

impl CredentialSource for FakeVault {
    fn authorize(&self, binding: &Binding) -> Result<UpstreamAuth, RelayError> {
        if binding.owner_id != "user/alice" || binding.connection_id != "pc_1" {
            return Err(RelayError::ConnectionOwnerMismatch);
        }
        let live = self.generation.load(Ordering::SeqCst);
        if live == 0 {
            return Err(RelayError::ProviderNotConnected);
        }
        if binding.connection_generation != live {
            return Err(RelayError::ConnectionGenerationStale);
        }
        Ok(UpstreamAuth {
            header: "authorization".into(),
            value: zeroize::Zeroizing::new(format!("Bearer {}", self.access_token)),
        })
    }
}

struct Harness {
    _provider: MockProvider,
    provider_url: String,
    gateway: Arc<Gateway>,
    vault: Arc<FakeVault>,
    node_key: Ed25519KeyPair,
}

static NOW: AtomicU64 = AtomicU64::new(1_000_000);

fn clock() -> u64 {
    NOW.load(Ordering::SeqCst)
}

impl Harness {
    async fn new() -> Self {
        let provider = MockProvider::start(MockConfig::default())
            .await
            .expect("mock");
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
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("key");
        let node_key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("pair");
        let public: [u8; 32] = node_key.public_key().as_ref().try_into().expect("32");
        let vault = Arc::new(FakeVault {
            generation: AtomicU64::new(1),
            access_token: tokens["access_token"].as_str().expect("access").to_string(),
        });
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
        let gateway = Gateway::new(Verifier::new(vec![public]), table, source)
            .expect("gateway")
            .with_clock(clock);
        Self {
            _provider: provider,
            provider_url: base_url,
            gateway: Arc::new(gateway),
            vault,
            node_key,
        }
    }

    fn capability(&self, edit: impl FnOnce(&mut Capability)) -> Capability {
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
        capability.signature = self
            .node_key
            .sign(&message)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        capability
    }
}

fn open_line(request_id: &str, capability: &Capability, authorization: Option<&str>) -> Bytes {
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

fn line(value: Value) -> Bytes {
    let mut bytes = serde_json::to_vec(&value).expect("json");
    bytes.push(b'\n');
    Bytes::from(bytes)
}

fn body_lines(request_id: &str, body: &[u8]) -> Vec<Bytes> {
    vec![
        line(
            json!({"type": "request-body", "request_id": request_id, "seq": 1,
            "bytes_b64": encode_chunk(body)}),
        ),
        line(json!({"type": "request-end", "request_id": request_id, "seq_end": 1})),
    ]
}

/// Relay a fully known request and collect every response line.
async fn relay_all(harness: &Harness, lines: Vec<Bytes>) -> Vec<GatewayLine> {
    let input = futures_util::stream::iter(lines.into_iter().map(Ok::<_, std::io::Error>));
    harness
        .gateway
        .relay("sess-1", "claude", input)
        .map(|line| serde_json::from_slice::<GatewayLine>(&line).expect("gateway line"))
        .collect()
        .await
}

/// Wait until the gateway holds `count` in-flight slots for the test session:
/// a slot is taken only after `gateway-open` has been validated, so this is
/// the deterministic "the open was accepted" signal (no timing guesses).
async fn until_in_flight(gateway: &Gateway, count: usize) {
    for _ in 0..500 {
        if gateway.in_flight("sess-1") == count {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("gateway never reached {count} in-flight requests");
}

fn error_of(lines: &[GatewayLine]) -> Option<RelayError> {
    lines.iter().find_map(|line| match line {
        GatewayLine::Error { error, .. } => Some(*error),
        _ => None,
    })
}

#[tokio::test]
async fn the_gateway_injects_the_vault_credential() {
    let harness = Harness::new().await;
    let capability = harness.capability(|_| {});
    let mut lines = vec![open_line("r-1", &capability, None)];
    lines.extend(body_lines("r-1", br#"{"model":"mock"}"#));
    let response = relay_all(&harness, lines).await;
    let GatewayLine::Head { status, .. } = &response[0] else {
        panic!("expected head, got {response:?}");
    };
    assert_eq!(*status, 200);
    let body: Vec<u8> = response
        .iter()
        .filter_map(|line| match line {
            GatewayLine::ResponseBody { bytes_b64, .. } => {
                Some(jeryu_gateway::frames::decode_chunk(bytes_b64).expect("chunk"))
            }
            _ => None,
        })
        .flatten()
        .collect();
    let body: Value = serde_json::from_slice(&body).expect("provider json");
    assert_eq!(
        body["injected"], true,
        "the mock saw the vault's access token"
    );
    assert!(matches!(
        response.last(),
        Some(GatewayLine::ResponseEnd { .. })
    ));

    let counters: Value = reqwest::get(format!("{}/counters", harness.provider_url))
        .await
        .expect("counters")
        .json()
        .await
        .expect("json");
    assert_eq!(counters["chat_authorized"], 1);
    assert_eq!(counters["outbound_dial_attempts"], 0);
}

#[tokio::test]
async fn guest_authorization_forgery_and_bad_frames_are_refused() {
    let harness = Harness::new().await;
    let capability = harness.capability(|_| {});
    let with_auth = vec![open_line("r-1", &capability, Some("Bearer stolen"))];
    assert_eq!(
        error_of(&relay_all(&harness, with_auth).await),
        Some(RelayError::GuestAuthorizationRejected)
    );

    let mut forged = capability.clone();
    forged.connection_generation = 9;
    assert_eq!(
        error_of(&relay_all(&harness, vec![open_line("r-2", &forged, None)]).await),
        Some(RelayError::LeaseBindingMismatch),
        "an unsigned change never reaches the vault"
    );

    let mut oversized = vec![open_line("r-3", &capability, None)];
    oversized.push(line(
        json!({"type": "request-body", "request_id": "r-3", "seq": 1,
        "bytes_b64": encode_chunk(&vec![b'x'; 32 * 1024 + 1])}),
    ));
    assert_eq!(
        error_of(&relay_all(&harness, oversized).await),
        Some(RelayError::RelayStreamInvalid)
    );

    let mut reopened = vec![open_line("r-4", &capability, None)];
    reopened.push(open_line("r-4", &capability, None));
    assert_eq!(
        error_of(&relay_all(&harness, reopened).await),
        Some(RelayError::RelayStreamInvalid),
        "a second gateway-open for one request"
    );
}

#[tokio::test]
async fn expiry_is_checked_only_at_open() {
    let harness = Harness::new().await;
    let expired = harness.capability(|c| {
        c.issued_at_unix_millis -= 40_000;
        c.expires_at_unix_millis -= 40_000;
    });
    let mut lines = vec![open_line("r-1", &expired, None)];
    lines.extend(body_lines("r-1", b"{}"));
    assert_eq!(
        error_of(&relay_all(&harness, lines).await),
        Some(RelayError::CapabilityExpired)
    );

    // Opened in time; the clock passes expires_at while the body arrives.
    let capability = harness.capability(|c| c.expires_at_unix_millis = c.issued_at_unix_millis + 5);
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let input =
        futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|i| (i, rx)) });
    let output = harness.gateway.relay("sess-1", "claude", Box::pin(input));
    tx.send(Ok(open_line("r-2", &capability, None)))
        .await
        .expect("open");
    // The open is accepted (it holds a slot) before the clock passes expiry.
    until_in_flight(&harness.gateway, 1).await;
    NOW.fetch_add(10_000, Ordering::SeqCst);
    for body in body_lines("r-2", b"{}") {
        tx.send(Ok(body)).await.expect("body");
    }
    let lines: Vec<GatewayLine> = output
        .map(|line| serde_json::from_slice(&line).expect("line"))
        .collect()
        .await;
    assert!(
        error_of(&lines).is_none(),
        "accepted before expiry: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| matches!(l, GatewayLine::ResponseBody { .. }))
    );
}

#[tokio::test]
async fn a_generation_bump_mid_request_ends_it_without_more_body() {
    let harness = Harness::new().await;
    let capability = harness.capability(|_| {});
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let input =
        futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|i| (i, rx)) });
    let output = harness.gateway.relay("sess-1", "claude", Box::pin(input));
    tx.send(Ok(open_line("r-1", &capability, None)))
        .await
        .expect("open");
    tx.send(Ok(line(
        json!({"type": "request-body", "request_id": "r-1", "seq": 1,
        "bytes_b64": encode_chunk(b"{")}),
    )))
    .await
    .expect("body 1");
    until_in_flight(&harness.gateway, 1).await;
    // The credential is replaced between request-body frames.
    harness.vault.generation.store(2, Ordering::SeqCst);
    tx.send(Ok(line(
        json!({"type": "request-body", "request_id": "r-1", "seq": 2,
        "bytes_b64": encode_chunk(b"}")}),
    )))
    .await
    .expect("body 2");
    let lines: Vec<GatewayLine> = output
        .map(|line| serde_json::from_slice(&line).expect("line"))
        .collect()
        .await;
    assert_eq!(
        error_of(&lines),
        Some(RelayError::ConnectionGenerationStale)
    );
    assert!(
        !lines
            .iter()
            .any(|l| matches!(l, GatewayLine::ResponseBody { .. }))
    );
}

#[tokio::test]
async fn four_requests_may_be_in_flight_and_the_fifth_is_busy() {
    let harness = Harness::new().await;
    let mut held = Vec::new();
    for index in 0..4 {
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
        let input =
            futures_util::stream::unfold(
                rx,
                |mut rx| async move { rx.recv().await.map(|i| (i, rx)) },
            );
        let output = harness.gateway.relay("sess-1", "claude", Box::pin(input));
        let capability = harness.capability(|_| {});
        tx.send(Ok(open_line(&format!("r-{index}"), &capability, None)))
            .await
            .expect("open");
        held.push((tx, output));
    }
    until_in_flight(&harness.gateway, 4).await;
    let capability = harness.capability(|_| {});
    let fifth = relay_all(&harness, vec![open_line("r-5", &capability, None)]).await;
    assert_eq!(error_of(&fifth), Some(RelayError::ConnectionBusy));
    drop(held);
}

#[tokio::test]
async fn production_hosts_relay_nothing_until_a_path_is_enrolled() {
    let table = UpstreamTable::production();
    for provider in ["claude", "codex", "grok"] {
        assert_eq!(
            table.resolve(provider, "/v1/messages").unwrap_err(),
            RelayError::UpstreamHostDenied
        );
    }
}

/// Negative authorization proof: a node-signed capability for a non-owner
/// (another user, or a service account claiming a user's connection) is
/// forbidden by the vault check and never reaches the provider.
#[tokio::test]
async fn a_non_owner_capability_is_forbidden_and_nothing_reaches_the_provider() {
    let harness = Harness::new().await;
    let other_user = harness.capability(|c| c.owner_id = "user/mallory".into());
    let mut lines = vec![open_line("r-1", &other_user, None)];
    lines.extend(body_lines("r-1", b"{}"));
    assert_eq!(
        error_of(&relay_all(&harness, lines).await),
        Some(RelayError::ConnectionOwnerMismatch)
    );
    let service = harness.capability(|c| {
        c.owner_kind = OwnerKind::ServiceAccount;
        c.owner_id = "service/redteam".into();
    });
    assert_eq!(
        error_of(&relay_all(&harness, vec![open_line("r-2", &service, None)]).await),
        Some(RelayError::ConnectionOwnerMismatch)
    );
    let counters: Value = reqwest::get(format!("{}/counters", harness.provider_url))
        .await
        .expect("counters")
        .json()
        .await
        .expect("json");
    assert_eq!(
        counters["chat_authorized"], 0,
        "the wrong user got no upstream call"
    );
    assert_eq!(
        counters["chat_rejected"], 0,
        "nothing was even attempted upstream"
    );
}
