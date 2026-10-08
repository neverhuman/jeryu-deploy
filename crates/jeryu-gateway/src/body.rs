//! The request body of an accepted relay call, streamed upstream as the node
//! sends it. Each `request-body` line re-checks the assignment, the lease,
//! and the connection metadata before its bytes go upstream; nothing is
//! buffered beyond one chunk, and the credential is never retrieved again.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::Stream;
use parking_lot::Mutex;

use crate::RelayError;
use crate::authority;
use crate::capability::Capability;
use crate::frames::{NodeLine, decode_chunk};
use crate::relay::{Binding, Gateway, MAX_REQUEST_BODY};
use crate::stream::LineReader;

/// A request that passed phase 0 and every open precondition.
pub(crate) struct Accepted {
    gateway: Arc<Gateway>,
    runner_id: String,
    capability: Capability,
    binding: Binding,
}

impl Accepted {
    pub(crate) fn new(
        gateway: Arc<Gateway>,
        runner_id: String,
        capability: Capability,
        binding: Binding,
    ) -> Self {
        Self {
            gateway,
            runner_id,
            capability,
            binding,
        }
    }

    /// The per-line check (connection-v1 §6): the same runner still holds the
    /// same current lease, and the connection is still live at this
    /// generation. `expires_at` is not re-checked.
    pub(crate) fn recheck(&self) -> Result<(), RelayError> {
        let gateway = &self.gateway;
        authority::current(
            gateway.authority.as_ref(),
            &self.runner_id,
            &self.capability,
            (gateway.clock)(),
        )
        .map_err(|_| RelayError::LeaseBindingMismatch)?;
        gateway.credentials.check(&self.binding)
    }
}

/// Where the body stream leaves the reason it stopped, for the relay to
/// answer with once the upstream call returns.
pub(crate) type Failure = Arc<Mutex<Option<RelayError>>>;

struct State<S> {
    lines: LineReader<S>,
    request_id: String,
    accepted: Arc<Accepted>,
    failure: Failure,
    expected: u64,
    total: usize,
    done: bool,
}

impl<S, E> State<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, RelayError> {
        match self.lines.next_line().await? {
            NodeLine::RequestBody {
                request_id,
                seq,
                bytes_b64,
            } => {
                if request_id != self.request_id || seq != self.expected {
                    return Err(RelayError::RelayStreamInvalid);
                }
                self.accepted.recheck()?;
                let chunk = decode_chunk(&bytes_b64)?;
                self.total += chunk.len();
                if self.total > MAX_REQUEST_BODY {
                    return Err(RelayError::RelayStreamInvalid);
                }
                self.expected += 1;
                Ok(Some(Bytes::from(chunk)))
            }
            NodeLine::RequestEnd {
                request_id,
                seq_end,
            } => {
                if request_id != self.request_id || seq_end.checked_add(1) != Some(self.expected) {
                    return Err(RelayError::RelayStreamInvalid);
                }
                self.accepted.recheck()?;
                Ok(None)
            }
            NodeLine::GatewayOpen { .. } => Err(RelayError::RelayStreamInvalid),
        }
    }
}

/// The node's `request-body` lines as an upstream body stream. A refused
/// line ends the stream with an error and records why in `failure`.
pub(crate) fn request_body<S, E>(
    lines: LineReader<S>,
    request_id: String,
    accepted: Arc<Accepted>,
    failure: Failure,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
    E: Send + 'static,
{
    let state = State {
        lines,
        request_id,
        accepted,
        failure,
        expected: 1,
        total: 0,
        done: false,
    };
    futures_util::stream::unfold(state, |mut state| async move {
        if state.done {
            return None;
        }
        match state.next_chunk().await {
            Ok(Some(chunk)) => Some((Ok(chunk), state)),
            Ok(None) => None,
            Err(error) => {
                *state.failure.lock() = Some(error);
                state.done = true;
                Some((Err(std::io::Error::other("relay request refused")), state))
            }
        }
    })
}
