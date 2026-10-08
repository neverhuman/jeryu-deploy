//! CI-A connect-once relay through the gateway to this crate's mock
//! provider. The mock never adds a credential, so `{"injected": true}` proves
//! the gateway did (k3-relay-v1, "Order the CI-A fake must show"). The
//! credential is retrieved once per accepted request; every later line only
//! re-checks metadata.

mod common;

use std::sync::atomic::Ordering;

use common::*;
use futures_util::StreamExt;
use jeryu_gateway::frames::GatewayLine;
use jeryu_gateway::{RelayError, UpstreamTable};
use serde_json::{Value, json};

#[tokio::test]
async fn the_gateway_injects_the_vault_credential_once() {
    let harness = Harness::new().await;
    let capability = harness.capability(|_| {});
    let mut lines = vec![open_line("r-1", &capability, None)];
    lines.push(body_line("r-1", 1, br#"{"model":"#));
    lines.push(body_line("r-1", 2, br#""mock"}"#));
    lines.push(end_line("r-1", 2));
    let response = harness.relay_all(lines).await;
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

    let (checks, credentials) = harness.calls();
    assert_eq!(credentials, 1, "one retrieval for one request");
    assert!(
        checks >= 4,
        "open, two body lines, end, and the response: {checks}"
    );
    let counters = harness.counters().await;
    assert_eq!(counters["chat_authorized"], 1);
    assert_eq!(counters["outbound_dial_attempts"], 0);
}

#[tokio::test]
async fn bad_frames_are_refused() {
    let harness = Harness::new().await;
    let capability = harness.capability(|_| {});
    let mut oversized = vec![open_line("r-3", &capability, None)];
    oversized.push(line(
        json!({"type": "request-body", "request_id": "r-3", "seq": 1,
        "bytes_b64": jeryu_gateway::frames::encode_chunk(&vec![b'x'; 32 * 1024 + 1])}),
    ));
    assert_eq!(
        error_of(&harness.relay_all(oversized).await),
        Some(RelayError::RelayStreamInvalid)
    );

    let mut reopened = vec![open_line("r-4", &capability, None)];
    reopened.push(open_line("r-4", &capability, None));
    assert_eq!(
        error_of(&harness.relay_all(reopened).await),
        Some(RelayError::RelayStreamInvalid),
        "a second gateway-open for one request"
    );

    // One transport chunk far past the line limit, with no newline, is
    // refused without being buffered whole.
    let flood = bytes::Bytes::from(vec![b'{'; 4 * 1024 * 1024]);
    assert_eq!(
        error_of(&harness.relay_all(vec![flood]).await),
        Some(RelayError::RelayStreamInvalid)
    );
}

#[tokio::test]
async fn expiry_is_checked_only_at_open() {
    let harness = Harness::new().await;
    // Opened in time; the clock passes expires_at while the body arrives.
    let capability = harness.capability(|c| c.expires_at_unix_millis = c.issued_at_unix_millis + 5);
    let (tx, output) = harness.relay_open();
    tx.send(Ok(open_line("r-2", &capability, None)))
        .await
        .expect("open");
    harness.until_in_flight(1).await;
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
    assert!(has_body(&lines));
}

#[tokio::test]
async fn a_generation_bump_mid_request_ends_it_without_more_body() {
    let harness = Harness::new().await;
    let capability = harness.capability(|_| {});
    let (tx, output) = harness.relay_open();
    tx.send(Ok(open_line("r-1", &capability, None)))
        .await
        .expect("open");
    tx.send(Ok(body_line("r-1", 1, b"{")))
        .await
        .expect("body 1");
    harness.until_in_flight(1).await;
    // The credential is replaced between request-body frames.
    harness.vault.generation.store(2, Ordering::SeqCst);
    tx.send(Ok(body_line("r-1", 2, b"}")))
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
    assert!(!has_body(&lines));
    assert_eq!(harness.calls().1, 1, "the fence re-checked metadata only");
}

#[tokio::test]
async fn a_new_lease_mid_request_ends_it() {
    let harness = Harness::new().await;
    let capability = harness.capability(|_| {});
    let (tx, output) = harness.relay_open();
    tx.send(Ok(open_line("r-1", &capability, None)))
        .await
        .expect("open");
    tx.send(Ok(body_line("r-1", 1, b"{")))
        .await
        .expect("body 1");
    harness.until_in_flight(1).await;
    // The control plane re-leases the session (a new epoch) mid-request.
    if let Some(assignment) = harness.sessions.0.lock().as_mut() {
        assignment.lease_epoch = 2;
    }
    tx.send(Ok(body_line("r-1", 2, b"}")))
        .await
        .expect("body 2");
    let lines: Vec<GatewayLine> = output
        .map(|line| serde_json::from_slice(&line).expect("line"))
        .collect()
        .await;
    assert_eq!(error_of(&lines), Some(RelayError::LeaseBindingMismatch));
    assert!(!has_body(&lines));
}

/// The periodic re-check fences a request even when no line moves: here
/// the node goes quiet mid-body, so neither direction has a frame to check
/// on, and the connection is replaced meanwhile.
#[tokio::test]
async fn a_quiet_request_is_still_fenced_by_the_periodic_recheck() {
    let harness = Harness::new().await;
    let capability = harness.capability(|_| {});
    let (tx, output) = harness.relay_open();
    tx.send(Ok(open_line("r-1", &capability, None)))
        .await
        .expect("open");
    tx.send(Ok(body_line("r-1", 1, b"{")))
        .await
        .expect("body 1");
    harness.until_in_flight(1).await;
    harness.vault.generation.store(2, Ordering::SeqCst);
    let lines: Vec<GatewayLine> = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        output
            .map(|line| serde_json::from_slice(&line).expect("line"))
            .collect(),
    )
    .await
    .expect("fenced within a few re-check intervals");
    assert_eq!(
        error_of(&lines),
        Some(RelayError::ConnectionGenerationStale)
    );
    harness.until_in_flight(0).await;
    assert_eq!(harness.calls().1, 1);
    drop(tx);
}

/// A node that hangs up mid-request cancels the upstream call and frees the
/// slot, without waiting for the idle timeout.
#[tokio::test]
async fn a_consumer_that_hangs_up_frees_the_slot() {
    let harness = Harness::new().await;
    let capability = harness.capability(|_| {});
    let (tx, output) = harness.relay_open();
    tx.send(Ok(open_line("r-1", &capability, None)))
        .await
        .expect("open");
    tx.send(Ok(body_line("r-1", 1, b"{")))
        .await
        .expect("body 1");
    harness.until_in_flight(1).await;
    drop(output);
    harness.until_in_flight(0).await;
    assert_eq!(
        harness.counters().await["chat_authorized"],
        0,
        "the upstream call was cut"
    );
    drop(tx);
}

#[tokio::test]
async fn four_requests_may_be_in_flight_and_the_fifth_is_busy() {
    let harness = Harness::new().await;
    let mut held = Vec::new();
    for index in 0..4 {
        let (tx, output) = harness.relay_open();
        let capability = harness.capability(|_| {});
        tx.send(Ok(open_line(&format!("r-{index}"), &capability, None)))
            .await
            .expect("open");
        held.push((tx, output));
    }
    harness.until_in_flight(4).await;
    let capability = harness.capability(|_| {});
    let fifth = harness
        .relay_all(vec![open_line("r-5", &capability, None)])
        .await;
    assert_eq!(error_of(&fifth), Some(RelayError::ConnectionBusy));
    assert_eq!(harness.calls().1, 4, "the busy request retrieved nothing");
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
