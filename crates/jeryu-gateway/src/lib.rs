//! `jeryu-gateway`: the control-plane end of the K3 credential relay
//! (contracts connection-v1 §6 and k3-relay-v1).
//!
//! A node relays a guest's provider request as NDJSON over mTLS to
//! `POST /internal/gateway/{session_id}/{provider}`. The gateway verifies the
//! node-signed capability, asks the vault ([`CredentialSource`]) whether the
//! connection generation is live, adds the credential upstream, and streams
//! the response back. The guest never holds the credential. Real providers
//! stay disabled until a path template is enrolled ([`UpstreamTable`]).

use serde::{Deserialize, Serialize};

pub mod capability;
pub mod frames;
pub mod listener;
pub mod policy;
pub mod relay;
mod route;
mod stream;

pub use capability::{Capability, Mode, OwnerKind, Verifier};
pub use policy::{Upstream, UpstreamTable};
pub use relay::{Binding, CredentialSource, Gateway, UpstreamAuth};
pub use route::router;

/// connection-v1 error codes that a relay can end with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayError {
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
