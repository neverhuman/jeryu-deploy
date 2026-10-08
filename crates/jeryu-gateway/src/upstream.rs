//! The upstream half of an accepted relay request: send it with the
//! credential, then stream the response back as NDJSON lines. Each response
//! piece re-checks the assignment and connection first.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;

use crate::RelayError;
use crate::body::{Accepted, Failure};
use crate::frames::{GatewayLine, Header, MAX_CHUNK_BYTES};
use crate::relay::{IDLE_TIMEOUT, UpstreamAuth};
use crate::stream::MAX_TRANSPORT_CHUNK;

/// What the open phase decided about the upstream call.
pub(crate) struct Prepared {
    pub(crate) method: reqwest::Method,
    pub(crate) url: String,
    pub(crate) headers: Vec<Header>,
    pub(crate) fixed_headers: Vec<(String, String)>,
    pub(crate) auth: UpstreamAuth,
}

pub(crate) async fn forward<B>(
    client: &reqwest::Client,
    prepared: Prepared,
    body: B,
    failure: &Failure,
    accepted: &Arc<Accepted>,
    request_id: &str,
    tx: &mpsc::Sender<Bytes>,
) -> Result<(), RelayError>
where
    B: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let Prepared {
        method,
        url,
        headers,
        fixed_headers,
        auth,
    } = prepared;
    let mut request = client
        .request(method, url)
        .body(reqwest::Body::wrap_stream(body));
    for Header { name, value } in &headers {
        request = request.header(name.as_str(), value.as_str());
    }
    for (name, value) in &fixed_headers {
        request = request.header(name.as_str(), value.as_str());
    }
    request = request.header(auth.header.as_str(), auth.value.as_str());
    drop(auth);
    let body_failure = || failure.lock().take();
    let response = match tokio::time::timeout(IDLE_TIMEOUT, request.send()).await {
        Err(_) => return Err(RelayError::RelayIdleTimeout),
        Ok(Err(_)) => return Err(body_failure().unwrap_or(RelayError::UpstreamHostDenied)),
        Ok(Ok(response)) => response,
    };
    if let Some(error) = body_failure() {
        return Err(error);
    }
    if response.status().is_redirection() {
        return Err(RelayError::UpstreamHostDenied);
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 256)
        .map(|value| Header {
            name: "content-type".into(),
            value: value.into(),
        });
    let head = GatewayLine::Head {
        request_id: request_id.to_string(),
        status: response.status().as_u16(),
        headers: content_type.into_iter().collect(),
    };
    send(tx, &head).await?;
    let mut stream = response.bytes_stream();
    let mut seq = 0_u64;
    loop {
        let chunk = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Err(_) => return Err(RelayError::RelayIdleTimeout),
            Ok(None) => break,
            Ok(Some(Err(_))) => return Err(RelayError::RelayStreamInvalid),
            Ok(Some(Ok(chunk))) if chunk.len() > MAX_TRANSPORT_CHUNK => {
                return Err(RelayError::RelayStreamInvalid);
            }
            Ok(Some(Ok(chunk))) => chunk,
        };
        for piece in chunk.chunks(MAX_CHUNK_BYTES) {
            // A disconnect, replacement, or lease change stops the body here.
            accepted.recheck()?;
            seq += 1;
            let line = GatewayLine::ResponseBody {
                request_id: request_id.to_string(),
                seq,
                bytes_b64: crate::frames::encode_chunk(piece),
            };
            send(tx, &line).await?;
        }
    }
    let end = GatewayLine::ResponseEnd {
        request_id: request_id.to_string(),
        seq_end: seq,
    };
    send(tx, &end).await
}

/// One response line to the node. A node that stops reading for the idle
/// timeout ends the request instead of holding it open forever.
pub(crate) async fn send(tx: &mpsc::Sender<Bytes>, line: &GatewayLine) -> Result<(), RelayError> {
    let bytes = Bytes::from(line.to_line()?);
    match tokio::time::timeout(IDLE_TIMEOUT, tx.send(bytes)).await {
        Err(_) => Err(RelayError::RelayIdleTimeout),
        Ok(Err(_)) => Err(RelayError::RelayStreamInvalid),
        Ok(Ok(())) => Ok(()),
    }
}
