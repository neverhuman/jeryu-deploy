//! `jeryu-gateway`: the control-plane end of the K3 credential relay
//! (contracts connection-v1 §6 and k3-relay-v1).
//!
//! A node relays a guest's provider request as NDJSON over mTLS to
//! `POST /internal/gateway/{session_id}/{provider}` on the node-facing mTLS
//! listener. The gateway runs phase 0 ([`authority`]): the verified peer's own
//! capability key, its current assignment, and the stored lease. Only then
//! does it ask the vault ([`CredentialSource`]) about connection metadata,
//! and it retrieves the credential once, after every open precondition. The
//! guest never holds the credential. Real providers stay disabled until a
//! path template is enrolled ([`UpstreamTable`]).

use serde::{Deserialize, Serialize};

pub mod authority;
mod body;
pub mod capability;
pub mod frames;
pub mod listener;
pub mod policy;
pub mod relay;
mod route;
mod stream;
mod upstream;
mod upstream_dns;

pub use authority::{
    Assignment, CertificateFingerprint, EnrolledNode, LeaseBinding, NoSessions, NodeRegistry,
    PeerIdentity, SessionAuthority,
};
pub use capability::{Capability, Mode, OwnerKind};
pub use policy::{Upstream, UpstreamTable};
pub use relay::{Binding, CredentialSource, Gateway, UpstreamAuth};
pub(crate) use route::router;

/// connection-v1 error codes that a relay can end with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayError {
    /// Phase 0 failed: wrong peer, wrong key, no current assignment, or a
    /// capability that does not match the stored lease. Nothing was read.
    CapabilityUnverified,
    ProviderUnknown,
    ConnectionOwnerMismatch,
    ProviderNotConnected,
    ConnectionGenerationStale,
    ProviderModeDisabled,
    ProviderModeUnqualified,
    RamTokenNotPermitted,
    GuestAuthorizationRejected,
    CapabilityExpired,
    CapabilityAudienceMismatch,
    UpstreamHostDenied,
    ConnectionBusy,
    RefreshPersistFailed,
    LeaseBindingMismatch,
    RelayIdleTimeout,
    RelayStreamInvalid,
}
