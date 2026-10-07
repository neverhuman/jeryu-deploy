//! Bidirectional live terminal attach for one agent run.
//!
//! `GET /api/v1/agent-runs/{id}/attach?after_seq=N&control=true` upgrades to a
//! WebSocket. The server first replays the retained raw-TTY ring past `after_seq`
//! (a `resync` frame precedes it when the cursor already rolled off), then pushes
//! live events. Every viewer may watch; at most one holds the control lease and
//! may drive the terminal. Taking control bumps the lease epoch, so the displaced
//! controller is told `control_lost` and its later input is refused rather than
//! interleaved with the new controller's keystrokes.
//!
//! Server frames (JSON text): `attached`, `tty` (the same `AgentTtyEvent` shape the
//! SSE stream and `agent_work.tail` return), `resync`, `ack`, `error`,
//! `control_lost`, and `ended` once the run finishes and its output is drained.
//!
//! Client frames: JSON `{"type":"control","request_id":..,"command":{..}}` with a
//! `/control` command body, `{"type":"take_control"}`, `{"type":"release_control"}`,
//! or a binary frame holding raw UTF-8 keystrokes (sent as `send_input`).

use axum::extract::Extension;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use base64::Engine;
use jeryu_core::AccountSummary;

use super::*;

/// Largest client frame accepted; one terminal input never needs more.
const MAX_CLIENT_FRAME_BYTES: usize = 64 * 1024;
/// How often an attached socket rechecks its lease and the run's liveness.
const ATTACH_TICK: Duration = Duration::from_millis(250);
/// Attach sockets allowed at once on one run.
const MAX_VIEWERS_PER_RUN: usize = 32;
/// A client that stops reading for this long is disconnected.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default, Deserialize)]
pub(in crate::web) struct AttachQuery {
    #[serde(default)]
    pub(in crate::web) after_seq: Option<u64>,
    #[serde(default)]
    pub(in crate::web) control: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AttachClientMessage {
    Control {
        #[serde(default)]
        request_id: Option<String>,
        command: AgentControlCommand,
    },
    TakeControl,
    ReleaseControl,
}

pub(in crate::web) async fn attach(
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    State(state): State<Arc<WebState>>,
    AxumPath(agent_run_id): AxumPath<String>,
    Query(query): Query<AttachQuery>,
    account: Option<Extension<AccountSummary>>,
) -> AxumResponse {
    if state.agent_runs.run_state(&agent_run_id).is_none() {
        return agent_run_not_found(&agent_run_id);
    }
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };
    let account = account.map(|Extension(account)| account);
    let may_control = super::access::may_control_host_run(account.as_ref());
    let login = account.map_or_else(|| "anonymous".to_string(), |account| account.login);
    let store = state.agent_runs.clone();
    ws.max_message_size(MAX_CLIENT_FRAME_BYTES)
        .on_upgrade(move |socket| {
            AttachSession {
                socket,
                store,
                run_id: agent_run_id,
                holder: format!("{login}/{}", uuid::Uuid::new_v4().simple()),
                epoch: None,
                may_control,
                counted: false,
            }
            .run(query.after_seq.unwrap_or(0), query.control.unwrap_or(false))
        })
        .into_response()
}

struct AttachSession {
    socket: WebSocket,
    store: AgentRunStore,
    run_id: String,
    holder: String,
    epoch: Option<u64>,
    /// Host-PTY runs accept input only from an authenticated admin.
    may_control: bool,
    /// Whether this socket holds one of the run's viewer slots.
    counted: bool,
}

impl AttachSession {
    async fn run(mut self, after_seq: u64, want_control: bool) {
        if !self.store.attach_viewer(&self.run_id) {
            let reason = "this run already has the maximum number of attached clients";
            let _ = self.error(None, "attach_viewer_limit", reason).await;
            let _ = self.socket.send(Message::Close(None)).await;
            return;
        }
        self.counted = true;
        if want_control && self.may_control {
            self.epoch = self.store.take_control(&self.run_id, &self.holder);
        }
        let Some((mut receiver, replay)) = self.store.tty_stream_start(&self.run_id, after_seq)
        else {
            return self.finish();
        };
        let hello = json!({
            "type": "attached",
            "agent_run_id": self.run_id,
            "controller": self.epoch.is_some(),
            "control_epoch": self.epoch,
            "oldest_retained_seq": replay.oldest_retained_seq,
        });
        if !self.send(hello).await {
            return self.finish();
        }
        if replay.lagged && !self.send(resync(replay.oldest_retained_seq)).await {
            return self.finish();
        }
        for event in &replay.events {
            if !self.send(tty(event)).await {
                return self.finish();
            }
        }
        let mut cursor = replay.next_after_seq;
        let mut tick = tokio::time::interval(ATTACH_TICK);
        loop {
            let open = tokio::select! {
                event = receiver.recv() => match event {
                    Ok(event) if event.seq > cursor => {
                        cursor = event.seq;
                        self.send(tty(&event)).await
                    }
                    Ok(_) => true,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let oldest = self.store.oldest_retained_tty_seq(&self.run_id);
                        self.send(resync(oldest)).await
                    }
                    Err(broadcast::error::RecvError::Closed) => false,
                },
                message = self.socket.recv() => match message {
                    Some(Ok(message)) => self.client(message).await,
                    None | Some(Err(_)) => false,
                },
                _ = tick.tick() => self.tick(&mut receiver, &mut cursor).await,
            };
            if !open {
                break;
            }
        }
        self.finish();
    }

    /// Refresh lease and liveness. A finished run drains what it already
    /// published, says `ended`, and closes.
    async fn tick(
        &mut self,
        receiver: &mut broadcast::Receiver<AgentTtyEvent>,
        cursor: &mut u64,
    ) -> bool {
        if let Some(epoch) = self.epoch
            && !self.store.holds_control(&self.run_id, &self.holder, epoch)
        {
            self.epoch = None;
            if !self
                .send(json!({"type": "control_lost", "control_epoch": epoch}))
                .await
            {
                return false;
            }
        }
        let state = self.store.run_state(&self.run_id);
        if state == Some(AgentRunState::Running) {
            return true;
        }
        while let Ok(event) = receiver.try_recv() {
            if event.seq > *cursor {
                *cursor = event.seq;
                if !self.send(tty(&event)).await {
                    return false;
                }
            }
        }
        let _ = self.send(json!({"type": "ended", "state": state})).await;
        let _ = self.socket.send(Message::Close(None)).await;
        false
    }

    async fn client(&mut self, message: Message) -> bool {
        let parsed = match message {
            Message::Text(text) => serde_json::from_str::<AttachClientMessage>(&text)
                .map_err(|_| "attach_message_invalid"),
            // Raw keystrokes stay bytes: a split multibyte sequence or a
            // non-UTF-8 byte reaches the terminal exactly as typed.
            Message::Binary(bytes) => Ok(AttachClientMessage::Control {
                request_id: None,
                command: AgentControlCommand::SendBytes {
                    bytes_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
                },
            }),
            Message::Close(_) => return false,
            Message::Ping(_) | Message::Pong(_) => return true,
        };
        match parsed {
            Err(code) => {
                self.error(None, code, "the attach frame was not understood")
                    .await
            }
            Ok(AttachClientMessage::TakeControl) if !self.may_control => {
                let reason = "only an admin may drive a host-PTY agent run";
                self.error(None, "control_forbidden", reason).await
            }
            Ok(AttachClientMessage::TakeControl) => {
                self.epoch = self.store.take_control(&self.run_id, &self.holder);
                let frame = json!({"type": "control", "controller": self.epoch.is_some(),
                    "control_epoch": self.epoch});
                self.send(frame).await
            }
            Ok(AttachClientMessage::ReleaseControl) => {
                self.release();
                self.send(json!({"type": "control", "controller": false}))
                    .await
            }
            Ok(AttachClientMessage::Control {
                request_id,
                command,
            }) => self.control(request_id, command).await,
        }
    }

    async fn control(&mut self, request_id: Option<String>, command: AgentControlCommand) -> bool {
        let holds = self
            .epoch
            .is_some_and(|epoch| self.store.holds_control(&self.run_id, &self.holder, epoch));
        if !holds {
            let reason =
                "attach with ?control=true or send take_control before driving the terminal";
            return self.error(request_id, "control_not_held", reason).await;
        }
        if self.store.run_state(&self.run_id) != Some(AgentRunState::Running) {
            return self
                .error(
                    request_id,
                    "agent_run_finished",
                    "the agent run is already finished",
                )
                .await;
        }
        match self.store.send_control_by(
            &self.run_id,
            command,
            self.epoch.map(|epoch| (self.holder.as_str(), epoch)),
        ) {
            Ok(ack) => {
                let frame = json!({"type": "ack", "request_id": request_id,
                    "control_seq": ack.control_seq, "command": ack.command});
                self.send(frame).await
            }
            Err(_) => {
                let reason = "the live driver did not accept the command";
                self.error(request_id, "control_rejected", reason).await
            }
        }
    }

    async fn error(&mut self, request_id: Option<String>, code: &str, message: &str) -> bool {
        let frame = json!({"type": "error", "request_id": request_id, "code": code,
            "message": message});
        self.send(frame).await
    }

    async fn send(&mut self, frame: Value) -> bool {
        let send = self.socket.send(Message::Text(frame.to_string()));
        matches!(tokio::time::timeout(SEND_TIMEOUT, send).await, Ok(Ok(())))
    }

    fn release(&mut self) {
        if let Some(epoch) = self.epoch.take() {
            self.store
                .release_control(&self.run_id, &self.holder, epoch);
        }
    }

    fn finish(mut self) {
        self.release();
        if self.counted {
            self.store.detach_viewer(&self.run_id);
        }
    }
}

fn tty(event: &AgentTtyEvent) -> Value {
    json!({"type": "tty", "event": event})
}

fn resync(oldest_retained_seq: u64) -> Value {
    json!({"type": "resync", "oldest_retained_seq": oldest_retained_seq})
}

impl AgentRunStore {
    /// Current lifecycle state, or `None` for an unknown run.
    pub(super) fn run_state(&self, run_id: &str) -> Option<AgentRunState> {
        let inner = self.inner.lock().expect("agent run store mutex");
        inner.runs.get(run_id).map(|record| record.state)
    }

    /// Hand the control lease to `holder`, displacing any previous controller.
    /// Returns the new epoch, or `None` when the run is unknown or not running.
    pub(super) fn take_control(&self, run_id: &str, holder: &str) -> Option<u64> {
        let mut inner = self.inner.lock().expect("agent run store mutex");
        let record = inner.runs.get_mut(run_id)?;
        if record.state != AgentRunState::Running {
            return None;
        }
        record.control_lease.epoch = record.control_lease.epoch.saturating_add(1);
        record.control_lease.holder = Some(holder.to_string());
        Some(record.control_lease.epoch)
    }

    pub(super) fn holds_control(&self, run_id: &str, holder: &str, epoch: u64) -> bool {
        let inner = self.inner.lock().expect("agent run store mutex");
        inner.runs.get(run_id).is_some_and(|record| {
            record.control_lease.epoch == epoch
                && record.control_lease.holder.as_deref() == Some(holder)
        })
    }

    /// Take one of the run's attach slots; false when the run is full.
    pub(super) fn attach_viewer(&self, run_id: &str) -> bool {
        let mut inner = self.inner.lock().expect("agent run store mutex");
        let Some(record) = inner.runs.get_mut(run_id) else {
            return false;
        };
        if record.control_lease.viewers >= MAX_VIEWERS_PER_RUN {
            return false;
        }
        record.control_lease.viewers += 1;
        true
    }

    pub(super) fn detach_viewer(&self, run_id: &str) {
        let mut inner = self.inner.lock().expect("agent run store mutex");
        if let Some(record) = inner.runs.get_mut(run_id) {
            record.control_lease.viewers = record.control_lease.viewers.saturating_sub(1);
        }
    }

    /// Release only the lease this holder still owns; a newer controller keeps it.
    pub(super) fn release_control(&self, run_id: &str, holder: &str, epoch: u64) {
        let mut inner = self.inner.lock().expect("agent run store mutex");
        if let Some(record) = inner.runs.get_mut(run_id)
            && record.control_lease.epoch == epoch
            && record.control_lease.holder.as_deref() == Some(holder)
        {
            record.control_lease.holder = None;
        }
    }
}
