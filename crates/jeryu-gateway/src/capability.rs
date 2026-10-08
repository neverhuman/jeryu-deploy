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

/// Whether `capability` is signed by `key`, the capability key enrolled for
/// the mTLS peer that presented it. No other enrolled key is tried.
#[must_use]
pub fn signed_by(capability: &Capability, key: &[u8; 32]) -> bool {
    let (Some(signature), Some(message)) = (
        decode_hex_signature(&capability.signature),
        signing_bytes(capability),
    ) else {
        return false;
    };
    UnparsedPublicKey::new(&ED25519, key)
        .verify(&message, &signature)
        .is_ok()
}

/// After phase 0 and the vault checks (connection-v1 §4 precedence 9 and
/// 10): the lifetime is at most 60 s, `gateway-open` arrives before expiry,
/// and the audience is the gateway. Later lines of an accepted request are
/// not re-checked against `expires_at`.
pub fn verify_validity(capability: &Capability, now_unix_millis: u64) -> Result<(), RelayError> {
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

pub(crate) fn owner_matches_kind(capability: &Capability) -> bool {
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
    pub(crate) fn signed(now: u64, edit: impl FnOnce(&mut Capability)) -> ([u8; 32], Capability) {
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
        (public, capability)
    }

    #[test]
    fn only_the_signing_key_verifies_and_tampering_does_not() {
        let (key, capability) = signed(1_000, |_| {});
        assert!(signed_by(&capability, &key));
        let mut forged = capability.clone();
        forged.connection_generation = 2;
        assert!(!signed_by(&forged, &key), "an unsigned change is refused");
        let (other_key, _) = signed(1_000, |_| {});
        assert!(
            !signed_by(&capability, &other_key),
            "another enrolled node's key does not verify"
        );
        let mut garbled = capability.clone();
        garbled.signature = "zz".repeat(64);
        assert!(!signed_by(&garbled, &key));
    }

    #[test]
    fn expiry_lifetime_and_audience_are_enforced() {
        let (_, capability) = signed(1_000, |_| {});
        assert_eq!(verify_validity(&capability, 2_000), Ok(()));
        assert_eq!(
            verify_validity(&capability, 31_000),
            Err(RelayError::CapabilityExpired)
        );
        let (_, long) = signed(1_000, |c| c.expires_at_unix_millis = 1_000 + 60_001);
        assert_eq!(
            verify_validity(&long, 2_000),
            Err(RelayError::CapabilityExpired),
            "lifetime above 60 s"
        );
        let (_, audience) = signed(1_000, |c| c.audience = "jeryu.other".into());
        assert_eq!(
            verify_validity(&audience, 2_000),
            Err(RelayError::CapabilityAudienceMismatch)
        );
        let (_, kind) = signed(1_000, |c| c.owner_kind = OwnerKind::ServiceAccount);
        assert!(!owner_matches_kind(&kind));
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
