//! Plumbing for one relay request: the per-session in-flight slot, the
//! bounded NDJSON line reader, and the wall clock.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::RelayError;
use crate::frames::{MAX_LINE_BYTES, NodeLine};
use crate::relay::{IDLE_TIMEOUT, MAX_IN_FLIGHT};

pub(crate) type Slots = Arc<parking_lot::Mutex<HashMap<String, usize>>>;

/// A held in-flight slot for one session; released on drop.
pub(crate) struct Slot {
    slots: Slots,
    session_id: String,
}

impl Slot {
    pub(crate) fn take(
        slots: &Arc<parking_lot::Mutex<HashMap<String, usize>>>,
        session_id: &str,
    ) -> Result<Self, RelayError> {
        let mut map = slots.lock();
        let count = map.entry(session_id.to_string()).or_default();
        if *count >= MAX_IN_FLIGHT {
            return Err(RelayError::ConnectionBusy);
        }
        *count += 1;
        Ok(Self {
            slots: slots.clone(),
            session_id: session_id.to_string(),
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut map = self.slots.lock();
        if let Some(count) = map.get_mut(&self.session_id) {
            *count -= 1;
            if *count == 0 {
                map.remove(&self.session_id);
            }
        }
    }
}

/// Splits a byte stream into NDJSON lines, bounded and idle-timed. A
/// transport chunk is never copied whole: only the part up to the next
/// newline joins the line buffer, and a line past the limit is refused before
/// its bytes are copied.
pub(crate) struct LineReader<S> {
    input: S,
    buffer: Vec<u8>,
    pending: Bytes,
}

impl<S, E> LineReader<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    pub(crate) fn new(input: S) -> Self {
        Self {
            input,
            buffer: Vec::new(),
            pending: Bytes::new(),
        }
    }

    pub(crate) async fn next_line(&mut self) -> Result<NodeLine, RelayError> {
        loop {
            let newline = self.pending.iter().position(|&b| b == b'\n');
            let take = newline.map_or(self.pending.len(), |end| end + 1);
            if self.buffer.len() + take > MAX_LINE_BYTES + 1 {
                return Err(RelayError::RelayStreamInvalid);
            }
            let piece = self.pending.split_to(take);
            self.buffer.extend_from_slice(&piece);
            if newline.is_some() {
                let line = std::mem::take(&mut self.buffer);
                return crate::frames::parse_node_line(&line[..line.len() - 1]);
            }
            match tokio::time::timeout(IDLE_TIMEOUT, self.input.next()).await {
                Err(_) => return Err(RelayError::RelayIdleTimeout),
                Ok(None) | Ok(Some(Err(_))) => return Err(RelayError::RelayStreamInvalid),
                Ok(Some(Ok(bytes))) => self.pending = bytes,
            }
        }
    }
}

pub(crate) fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}
