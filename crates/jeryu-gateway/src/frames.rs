//! NDJSON lines on the node-to-gateway hop (k3-relay-v1 "Gateway hop").
//! Every line is one JSON object of at most 256 KiB; unknown fields are
//! refused. Body chunks are standard base64 of 1 to 32 768 bytes.

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::RelayError;
use crate::capability::Capability;

pub const MAX_LINE_BYTES: usize = 256 * 1024;
pub const MAX_CHUNK_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// Lines the node sends.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum NodeLine {
    GatewayOpen {
        request_id: String,
        provider: String,
        method: String,
        path: String,
        headers: Vec<Header>,
        #[serde(default)]
        authorization: Option<String>,
        capability: Box<Capability>,
    },
    RequestBody {
        request_id: String,
        seq: u64,
        bytes_b64: String,
    },
    RequestEnd {
        request_id: String,
        seq_end: u64,
    },
}

/// Lines the gateway sends back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum GatewayLine {
    Head {
        request_id: String,
        status: u16,
        headers: Vec<Header>,
    },
    ResponseBody {
        request_id: String,
        seq: u64,
        bytes_b64: String,
    },
    ResponseEnd {
        request_id: String,
        seq_end: u64,
    },
    Error {
        request_id: String,
        error: RelayError,
    },
}

impl GatewayLine {
    /// One NDJSON line, newline included. A line that does not serialize is
    /// `relay_stream_invalid`, never an empty frame.
    pub fn to_line(&self) -> Result<Vec<u8>, RelayError> {
        let mut line = serde_json::to_vec(self).map_err(|_| RelayError::RelayStreamInvalid)?;
        line.push(b'\n');
        Ok(line)
    }
}

/// Parse one node line, enforcing the size limit and identifier shape.
pub fn parse_node_line(line: &[u8]) -> Result<NodeLine, RelayError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(RelayError::RelayStreamInvalid);
    }
    let parsed: NodeLine =
        serde_json::from_slice(line).map_err(|_| RelayError::RelayStreamInvalid)?;
    let request_id = match &parsed {
        NodeLine::GatewayOpen { request_id, .. }
        | NodeLine::RequestBody { request_id, .. }
        | NodeLine::RequestEnd { request_id, .. } => request_id,
    };
    if !valid_id(request_id) {
        return Err(RelayError::RelayStreamInvalid);
    }
    Ok(parsed)
}

/// Decode one body chunk: standard padded base64, 1 to 32 768 bytes.
pub fn decode_chunk(bytes_b64: &str) -> Result<Vec<u8>, RelayError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(bytes_b64)
        .map_err(|_| RelayError::RelayStreamInvalid)?;
    if bytes.is_empty() || bytes.len() > MAX_CHUNK_BYTES {
        return Err(RelayError::RelayStreamInvalid);
    }
    Ok(bytes)
}

#[must_use]
pub fn encode_chunk(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The k3 `id` rule: 1 to 128 bytes of `[A-Za-z0-9._:/-]`, alphanumeric at
/// both ends.
#[must_use]
pub fn valid_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_are_bounded_and_padded() {
        assert_eq!(decode_chunk(&encode_chunk(b"hi")), Ok(b"hi".to_vec()));
        let largest = vec![7_u8; MAX_CHUNK_BYTES];
        assert!(decode_chunk(&encode_chunk(&largest)).is_ok());
        let too_big = vec![7_u8; MAX_CHUNK_BYTES + 1];
        assert_eq!(
            decode_chunk(&encode_chunk(&too_big)),
            Err(RelayError::RelayStreamInvalid)
        );
        assert_eq!(decode_chunk(""), Err(RelayError::RelayStreamInvalid));
        assert_eq!(
            decode_chunk("aGk"),
            Err(RelayError::RelayStreamInvalid),
            "unpadded"
        );
    }

    #[test]
    fn node_lines_refuse_unknown_fields_and_bad_ids() {
        let ok = br#"{"type":"request-end","request_id":"r-1","seq_end":0}"#;
        assert!(parse_node_line(ok).is_ok());
        let extra = br#"{"type":"request-end","request_id":"r-1","seq_end":0,"session_id":"s"}"#;
        assert_eq!(parse_node_line(extra), Err(RelayError::RelayStreamInvalid));
        let bad_id = br#"{"type":"request-end","request_id":"-r","seq_end":0}"#;
        assert_eq!(parse_node_line(bad_id), Err(RelayError::RelayStreamInvalid));
        let huge = vec![b' '; MAX_LINE_BYTES + 1];
        assert_eq!(parse_node_line(&huge), Err(RelayError::RelayStreamInvalid));
    }
}
