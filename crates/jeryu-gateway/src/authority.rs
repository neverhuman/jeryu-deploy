//! Phase 0 (connection-v1 §4): who is calling, and is this call theirs to
//! make. Every check here runs before the vault is asked anything. Any
//! failure is `capability_unverified`, and nothing in a capability that has
//! not passed is trusted.
//!
//! - The verified mTLS peer selects one enrolled node and that node's
//!   capability key. A signature under any other enrolled key fails.
//! - The control-plane store names that same node as the runner assigned to
//!   the session now, with the signed session generation, lease id, and lease
//!   epoch, and a lease that is still current.
//! - The signed connection id, generation, provider, and mode match the
//!   binding stored for that lease, and the signed owner is the lease owner.

use std::collections::HashMap;

use crate::RelayError;
use crate::capability::{Capability, Mode, owner_matches_kind, signed_by};

/// The verified mTLS client certificate of the caller, as SHA-256 of its DER.
/// Only the node-facing listener creates one, after the TLS handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerIdentity {
    certificate_sha256: [u8; 32],
}

impl PeerIdentity {
    #[must_use]
    pub fn from_certificate_der(der: &[u8]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, der);
        let mut certificate_sha256 = [0_u8; 32];
        certificate_sha256.copy_from_slice(digest.as_ref());
        Self { certificate_sha256 }
    }

    #[must_use]
    pub fn from_sha256(certificate_sha256: [u8; 32]) -> Self {
        Self { certificate_sha256 }
    }
}

/// One node enrolled to call the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrolledNode {
    pub runner_id: String,
    /// The Ed25519 key the node signs capabilities with. Not the
    /// control-plane lease key.
    pub capability_key: [u8; 32],
}

/// Enrolled nodes by client certificate. Empty means no node may relay.
#[derive(Debug, Clone, Default)]
pub struct NodeRegistry {
    nodes: HashMap<PeerIdentity, EnrolledNode>,
}

impl NodeRegistry {
    pub fn enroll(&mut self, peer: PeerIdentity, node: EnrolledNode) {
        self.nodes.insert(peer, node);
    }

    fn node(&self, peer: &PeerIdentity) -> Option<&EnrolledNode> {
        self.nodes.get(peer)
    }
}

/// One provider binding of a stored signed lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseBinding {
    pub provider: String,
    pub connection_id: String,
    pub connection_generation: u64,
    pub mode: Mode,
}

/// What the control plane stored for a session: the runner it is assigned
/// to now and the signed lease that runner holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub runner_id: String,
    pub session_id: String,
    pub session_generation: u64,
    pub lease_id: String,
    pub lease_epoch: u64,
    pub owner_id: String,
    pub lease_expires_at_unix_millis: u64,
    pub bindings: Vec<LeaseBinding>,
}

/// The control plane's session store, as phase 0 reads it.
pub trait SessionAuthority: Send + Sync + 'static {
    fn assignment(&self, session_id: &str) -> Option<Assignment>;
}

/// No session is assigned anywhere: every relay fails phase 0. The control
/// plane uses this until VM sessions are stored.
pub struct NoSessions;

impl SessionAuthority for NoSessions {
    fn assignment(&self, _: &str) -> Option<Assignment> {
        None
    }
}

/// Phase 0 for one `gateway-open`. Returns the runner the call belongs to.
pub(crate) fn verify(
    registry: &NodeRegistry,
    authority: &dyn SessionAuthority,
    peer: Option<&PeerIdentity>,
    capability: &Capability,
    route: (&str, &str),
    now_unix_millis: u64,
) -> Result<String, RelayError> {
    let unverified = RelayError::CapabilityUnverified;
    let node = peer
        .and_then(|peer| registry.node(peer))
        .ok_or(unverified)?;
    if !signed_by(capability, &node.capability_key) {
        return Err(unverified);
    }
    let (session_id, provider) = route;
    if capability.session_id != session_id
        || capability.provider != provider
        || !owner_matches_kind(capability)
    {
        return Err(unverified);
    }
    current(authority, &node.runner_id, capability, now_unix_millis)?;
    Ok(node.runner_id.clone())
}

/// The stored assignment still names `runner_id`, the signed lease fields,
/// and the signed binding, and the lease is still current. Phase 0 runs it on
/// open; the relay runs it again on every later line, where a failure is
/// `lease_binding_mismatch` (the call was authentic when it opened).
pub(crate) fn current(
    authority: &dyn SessionAuthority,
    runner_id: &str,
    capability: &Capability,
    now_unix_millis: u64,
) -> Result<(), RelayError> {
    let unverified = RelayError::CapabilityUnverified;
    let assignment = authority
        .assignment(&capability.session_id)
        .ok_or(unverified)?;
    let lease_matches = assignment.runner_id == runner_id
        && assignment.session_id == capability.session_id
        && assignment.session_generation == capability.session_generation
        && assignment.lease_id == capability.lease_id
        && assignment.lease_epoch == capability.lease_epoch
        && assignment.owner_id == capability.owner_id
        && now_unix_millis < assignment.lease_expires_at_unix_millis;
    let binding_matches = assignment.bindings.iter().any(|binding| {
        binding.provider == capability.provider
            && binding.connection_id == capability.connection_id
            && binding.connection_generation == capability.connection_generation
            && binding.mode == capability.mode
    });
    if lease_matches && binding_matches {
        Ok(())
    } else {
        Err(unverified)
    }
}
