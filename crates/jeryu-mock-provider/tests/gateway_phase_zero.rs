//! connection-v1 §4 phase 0 at the gateway: every refusal below names the
//! case and proves the vault was not asked anything (zero metadata checks,
//! zero credential retrievals) and nothing reached the provider. After phase
//! 0, refusals may check metadata but still retrieve no credential.

mod common;

use common::*;
use jeryu_gateway::{Mode, OwnerKind, RelayError};

/// Relay one open (plus an empty body) and return the error and the calls.
async fn refusal(
    harness: &Harness,
    peer: Option<jeryu_gateway::PeerIdentity>,
    capability: &jeryu_gateway::Capability,
    authorization: Option<&str>,
) -> (Option<RelayError>, (usize, usize)) {
    let mut lines = vec![open_line("r-1", capability, authorization)];
    lines.extend(body_lines("r-1", b"{}"));
    let answer = harness.relay_from(peer, lines).await;
    (error_of(&answer), harness.calls())
}

async fn assert_nothing_upstream(harness: &Harness) {
    let counters = harness.counters().await;
    assert_eq!(counters["chat_authorized"], 0, "no upstream call");
    assert_eq!(
        counters["chat_rejected"], 0,
        "nothing was even attempted upstream"
    );
}

const UNVERIFIED: Option<RelayError> = Some(RelayError::CapabilityUnverified);

#[tokio::test]
async fn phase_zero_refusals_never_reach_the_vault() {
    let harness = Harness::new().await;
    let good = harness.capability(|_| {});
    let a = Some(harness.node_a.peer);
    let b = Some(harness.node_b.peer);

    // No verified mTLS peer: the router mounted anywhere but the listener.
    assert_eq!(
        refusal(&harness, None, &good, None).await,
        (UNVERIFIED, (0, 0))
    );
    // The assigned node's capability, presented by another enrolled node.
    assert_eq!(
        refusal(&harness, b, &good, None).await,
        (UNVERIFIED, (0, 0))
    );
    // Signed by another enrolled node's key, presented by the assigned node.
    let foreign = harness.capability_by(&harness.node_b, |_| {});
    assert_eq!(
        refusal(&harness, a, &foreign, None).await,
        (UNVERIFIED, (0, 0))
    );
    // Another enrolled node, its own key, a session it is not assigned.
    let unassigned = harness.capability_by(&harness.node_b, |_| {});
    assert_eq!(
        refusal(&harness, b, &unassigned, None).await,
        (UNVERIFIED, (0, 0))
    );
    // An unsigned change to a signed capability.
    let mut forged = good.clone();
    forged.connection_generation = 9;
    assert_eq!(
        refusal(&harness, a, &forged, None).await,
        (UNVERIFIED, (0, 0))
    );
    // Signed, but not what the stored lease says.
    for edit in [
        (|c: &mut jeryu_gateway::Capability| c.lease_epoch = 0) as fn(&mut _),
        |c| c.lease_id = "lease-0".into(),
        |c| c.session_generation = 2,
        |c| c.connection_id = "pc_2".into(),
        |c| c.connection_generation = 2,
        |c| c.mode = Mode::QualifiedRamToken,
    ] {
        let stale = harness.capability(edit);
        assert_eq!(
            refusal(&harness, a, &stale, None).await,
            (UNVERIFIED, (0, 0))
        );
    }
    // An owner-kind prefix that does not match the owner.
    let kind = harness.capability(|c| c.owner_kind = OwnerKind::ServiceAccount);
    assert_eq!(
        refusal(&harness, a, &kind, None).await,
        (UNVERIFIED, (0, 0))
    );
    assert_nothing_upstream(&harness).await;
}

#[tokio::test]
async fn an_unassigned_or_expired_lease_never_reaches_the_vault() {
    let harness = Harness::new().await;
    let good = harness.capability(|_| {});
    let a = Some(harness.node_a.peer);
    let mut moved = assignment();
    moved.runner_id = "node-b".into();
    *harness.sessions.0.lock() = Some(moved);
    assert_eq!(
        refusal(&harness, a, &good, None).await,
        (UNVERIFIED, (0, 0))
    );
    let mut expired = assignment();
    expired.lease_expires_at_unix_millis = clock();
    *harness.sessions.0.lock() = Some(expired);
    assert_eq!(
        refusal(&harness, a, &good, None).await,
        (UNVERIFIED, (0, 0))
    );
    *harness.sessions.0.lock() = None;
    assert_eq!(
        refusal(&harness, a, &good, None).await,
        (UNVERIFIED, (0, 0))
    );
    assert_nothing_upstream(&harness).await;
}

/// Negative authorization proof: a non-owner is forbidden. A capability
/// naming another user than the lease owner fails phase 0 before the vault.
/// A lease whose owner does not hold the connection is forbidden by the
/// vault's metadata check, and no credential is retrieved.
#[tokio::test]
async fn a_non_owner_is_forbidden_without_a_credential_retrieval() {
    let harness = Harness::new().await;
    let a = Some(harness.node_a.peer);
    let other_user = harness.capability(|c| c.owner_id = "user/mallory".into());
    assert_eq!(
        refusal(&harness, a, &other_user, None).await,
        (UNVERIFIED, (0, 0))
    );

    let mut mallory = assignment();
    mallory.owner_id = "user/mallory".into();
    *harness.sessions.0.lock() = Some(mallory);
    let leased = harness.capability(|c| c.owner_id = "user/mallory".into());
    let (error, (checks, credentials)) = refusal(&harness, a, &leased, None).await;
    assert_eq!(error, Some(RelayError::ConnectionOwnerMismatch));
    assert_eq!((checks, credentials), (1, 0));
    assert_nothing_upstream(&harness).await;
}

#[tokio::test]
async fn open_preconditions_after_phase_zero_retrieve_no_credential() {
    let harness = Harness::new().await;
    let a = Some(harness.node_a.peer);
    // An authentic, matching, expired capability stays capability_expired.
    let expired = harness.capability(|c| {
        c.issued_at_unix_millis -= 40_000;
        c.expires_at_unix_millis -= 40_000;
    });
    let (error, (_, credentials)) = refusal(&harness, a, &expired, None).await;
    assert_eq!(
        (error, credentials),
        (Some(RelayError::CapabilityExpired), 0)
    );
    // Issued in the future, or outliving the stored lease: still expired.
    let future = harness.capability(|c| {
        c.issued_at_unix_millis += 5_000;
        c.expires_at_unix_millis += 5_000;
    });
    let (error, (_, credentials)) = refusal(&harness, a, &future, None).await;
    assert_eq!(
        (error, credentials),
        (Some(RelayError::CapabilityExpired), 0)
    );
    let mut short = assignment();
    short.lease_expires_at_unix_millis = clock() + 10_000;
    *harness.sessions.0.lock() = Some(short);
    let overshoot = harness.capability(|_| {});
    let (error, (_, credentials)) = refusal(&harness, a, &overshoot, None).await;
    assert_eq!(
        (error, credentials),
        (Some(RelayError::CapabilityExpired), 0)
    );
    *harness.sessions.0.lock() = Some(assignment());
    // Guest Authorization in gateway_helper mode.
    let good = harness.capability(|_| {});
    let (error, (_, credentials)) = refusal(&harness, a, &good, Some("Bearer stolen")).await;
    assert_eq!(
        (error, credentials),
        (Some(RelayError::GuestAuthorizationRejected), 0)
    );
    // A mode the vault reports disabled.
    *harness.vault.disabled.lock() = true;
    let (error, (_, credentials)) = refusal(&harness, a, &good, None).await;
    assert_eq!(
        (error, credentials),
        (Some(RelayError::ProviderModeDisabled), 0)
    );
    *harness.vault.disabled.lock() = false;
    // qualified_ram_token, even when the stored lease names it.
    let mut ram = assignment();
    ram.bindings[0].mode = Mode::QualifiedRamToken;
    *harness.sessions.0.lock() = Some(ram);
    let ram_capability = harness.capability(|c| c.mode = Mode::QualifiedRamToken);
    let (error, (_, credentials)) = refusal(&harness, a, &ram_capability, None).await;
    assert_eq!(
        (error, credentials),
        (Some(RelayError::ProviderModeUnqualified), 0)
    );
    // A disconnected connection.
    *harness.sessions.0.lock() = Some(assignment());
    harness
        .vault
        .generation
        .store(0, std::sync::atomic::Ordering::SeqCst);
    let (error, (_, credentials)) = refusal(&harness, a, &good, None).await;
    assert_eq!(
        (error, credentials),
        (Some(RelayError::ProviderNotConnected), 0)
    );
    assert_nothing_upstream(&harness).await;
}
