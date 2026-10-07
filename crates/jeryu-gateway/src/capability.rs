//! Gateway capabilities (connection-v1 §6): a short-lived, signed statement
//! from an enrolled node that one admitted session may relay one request for
//! one provider connection binding.
//!
//! Signing bytes: `jeryu.gateway.capability.v1`, one NUL, then the capability
//! JSON without `signature`, object keys sorted recursively, no whitespace,
//! integers only.

use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::RelayError;

pub const AUDIENCE: &str = "jeryu.gateway";
pub const MAX_LIFETIME_MILLIS: u64 = 60_000;
const DOMAIN: &[u8] = b"jeryu.gateway.capability.v1\0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    GatewayHelper,
    QualifiedRamToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerKind {
    User,
    ServiceAccount,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub capability_id: String,
    pub connection_id: String,
    pub connection_generation: u64,
    pub provider: String,
    pub mode: Mode,
    pub owner_kind: OwnerKind,
    pub owner_id: String,
    pub session_id: String,
    pub session_generation: u64,
    pub lease_id: String,
    pub lease_epoch: u64,
    pub audience: String,
    pub issued_at_unix_millis: u64,
    pub expires_at_unix_millis: u64,
    pub signature: String,
}

/// Public keys of the nodes enrolled to call the gateway.
#[derive(Clone, Default)]
pub struct Verifier {
    keys: Vec<[u8; 32]>,
}

impl Verifier {
    #[must_use]
    pub fn new(keys: Vec<[u8; 32]>) -> Self {
        Self { keys }
    }

    /// Phase 1, before anything else: the node's signature, the route's
    /// `session_id` and `provider`, and the owner kind. No field of an
    /// unverified capability reaches the vault.
    pub fn verify_binding(
        &self,
        capability: &Capability,
        session_id: &str,
        provider: &str,
    ) -> Result<(), RelayError> {
        let signature =
            decode_hex_signature(&capability.signature).ok_or(RelayError::LeaseBindingMismatch)?;
        let message = signing_bytes(capability).ok_or(RelayError::LeaseBindingMismatch)?;
        let trusted = self.keys.iter().any(|key| {
            UnparsedPublicKey::new(&ED25519, key)
                .verify(&message, &signature)
                .is_ok()
        });
        if !trusted || capability.session_id != session_id || capability.provider != provider {
            return Err(RelayError::LeaseBindingMismatch);
        }
        if !owner_matches_kind(capability) {
            return Err(RelayError::ConnectionOwnerMismatch);
        }
        Ok(())
    }

    /// Phase 2, after the vault checks (connection-v1 §4 precedence): the
    /// lifetime is at most 60 s, `gateway-open` arrives before expiry, and
    /// the audience is the gateway. Later lines of an accepted request are
    /// not re-checked against `expires_at` (§34).
    pub fn verify_validity(
        &self,
        capability: &Capability,
        now_unix_millis: u64,
    ) -> Result<(), RelayError> {
        let lifetime = capability
            .expires_at_unix_millis
            .checked_sub(capability.issued_at_unix_millis);
        if !matches!(lifetime, Some(1..=MAX_LIFETIME_MILLIS))
            || now_unix_millis >= capability.expires_at_unix_millis
        {
            return Err(RelayError::CapabilityExpired);
        }
        if capability.audience != AUDIENCE {
            return Err(RelayError::CapabilityAudienceMismatch);
        }
        Ok(())
    }

    /// Both phases, for callers that have no vault step between them.
    pub fn verify(
        &self,
        capability: &Capability,
        session_id: &str,
        provider: &str,
        now_unix_millis: u64,
    ) -> Result<(), RelayError> {
        self.verify_binding(capability, session_id, provider)?;
        self.verify_validity(capability, now_unix_millis)
    }
}

fn owner_matches_kind(capability: &Capability) -> bool {
    match capability.owner_kind {
        OwnerKind::User => capability.owner_id.starts_with("user/"),
        OwnerKind::ServiceAccount => capability.owner_id.starts_with("service/"),
    }
}

/// The exact bytes a node signs. `None` if the capability cannot be encoded
/// canonically (it always can, by construction).
#[must_use]
pub fn signing_bytes(capability: &Capability) -> Option<Vec<u8>> {
    let mut value = serde_json::to_value(capability).ok()?;
    value.as_object_mut()?.remove("signature");
    let mut out = DOMAIN.to_vec();
    canonical(&value, &mut out)?;
    Some(out)
}

/// Recursively sorted keys, no whitespace; floats are refused.
fn canonical(value: &Value, out: &mut Vec<u8>) -> Option<()> {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => {
            out.extend_from_slice(serde_json::to_string(value).ok()?.as_bytes());
        }
        Value::Number(number) => {
            if number.is_f64() {
                return None;
            }
            out.extend_from_slice(number.to_string().as_bytes());
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                canonical(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(key).ok()?.as_bytes());
                out.push(b':');
                canonical(&map[key], out)?;
            }
            out.push(b'}');
        }
    }
    Some(())
}

fn decode_hex_signature(hex: &str) -> Option<[u8; 64]> {
    if hex.len() != 128 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut out = [0_u8; 64];
    for (index, pair) in hex.as_bytes().chunks(2).enumerate() {
        let text = std::str::from_utf8(pair).ok()?;
        out[index] = u8::from_str_radix(text, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use ring::signature::{Ed25519KeyPair, KeyPair};

    use super::*;

    /// A test node key and a capability it signed.
    pub(crate) fn signed(now: u64, edit: impl FnOnce(&mut Capability)) -> (Verifier, Capability) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("key");
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("pair");
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
            audience: AUDIENCE.into(),
            issued_at_unix_millis: now,
            expires_at_unix_millis: now + 30_000,
            signature: String::new(),
        };
        edit(&mut capability);
        let message = signing_bytes(&capability).expect("bytes");
        capability.signature = pair
            .sign(&message)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let public: [u8; 32] = pair.public_key().as_ref().try_into().expect("32 bytes");
        (Verifier::new(vec![public]), capability)
    }

    #[test]
    fn a_signed_capability_verifies_and_tampering_does_not() {
        let (verifier, capability) = signed(1_000, |_| {});
        assert_eq!(
            verifier.verify(&capability, "sess-1", "claude", 2_000),
            Ok(())
        );
        let mut forged = capability.clone();
        forged.connection_generation = 2;
        assert_eq!(
            verifier.verify(&forged, "sess-1", "claude", 2_000),
            Err(RelayError::LeaseBindingMismatch),
            "an unsigned change is refused before any field is trusted"
        );
        assert_eq!(
            Verifier::default().verify(&capability, "sess-1", "claude", 2_000),
            Err(RelayError::LeaseBindingMismatch),
            "no enrolled key, no relay"
        );
        assert_eq!(
            verifier.verify(&capability, "sess-2", "claude", 2_000),
            Err(RelayError::LeaseBindingMismatch)
        );
    }

    #[test]
    fn expiry_lifetime_audience_and_owner_kind_are_enforced() {
        let (verifier, capability) = signed(1_000, |_| {});
        assert_eq!(
            verifier.verify(&capability, "sess-1", "claude", 31_000),
            Err(RelayError::CapabilityExpired)
        );
        let (verifier, long) = signed(1_000, |c| c.expires_at_unix_millis = 1_000 + 60_001);
        assert_eq!(
            verifier.verify(&long, "sess-1", "claude", 2_000),
            Err(RelayError::CapabilityExpired),
            "lifetime above 60 s"
        );
        let (verifier, audience) = signed(1_000, |c| c.audience = "jeryu.other".into());
        assert_eq!(
            verifier.verify(&audience, "sess-1", "claude", 2_000),
            Err(RelayError::CapabilityAudienceMismatch)
        );
        let (verifier, kind) = signed(1_000, |c| c.owner_kind = OwnerKind::ServiceAccount);
        assert_eq!(
            verifier.verify(&kind, "sess-1", "claude", 2_000),
            Err(RelayError::ConnectionOwnerMismatch)
        );
    }

    #[test]
    fn signing_bytes_sort_keys_and_drop_the_signature() {
        let (_, capability) = signed(5, |_| {});
        let bytes = signing_bytes(&capability).expect("bytes");
        let text = String::from_utf8(bytes[DOMAIN.len()..].to_vec()).expect("utf8");
        assert!(text.starts_with("{\"audience\":\"jeryu.gateway\",\"capability_id\":"));
        assert!(!text.contains("signature") && !text.contains(' '));
    }
}
