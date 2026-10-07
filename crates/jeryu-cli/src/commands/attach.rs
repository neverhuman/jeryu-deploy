//! `jeryu agent attach`: a live terminal client for one agent run.
//!
//! The server side is `GET /api/v1/agent-runs/{id}/attach` (WebSocket). Output
//! bytes are written to stdout unchanged. With `--control` and a TTY on stdin the
//! local terminal enters raw mode, keystrokes are forwarded as binary frames, and
//! window changes are sent as `resize_pty`. `--json` prints every server frame as
//! one JSON line instead, for scripts and other agents.

use std::io::{IsTerminal, Write};

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

use crate::client::{ClientError, ClientResult};
use crate::commands::api::api_token;

/// Ctrl-]: the conventional detach key (telnet, docker attach).
const DETACH_KEY: u8 = 0x1d;

/// Build the WebSocket URL for one run from the API base URL.
pub(crate) fn attach_url(
    api_url: &str,
    run_id: &str,
    after_seq: u64,
    control: bool,
) -> ClientResult<String> {
    let base = api_url.trim_end_matches('/');
    let rest = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return Err(ClientError::Invalid("API URL must be HTTP(S)".into()));
    };
    if run_id.is_empty()
        || !run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(ClientError::Invalid("invalid agent run id".into()));
    }
    Ok(format!(
        "{rest}/api/v1/agent-runs/{run_id}/attach?after_seq={after_seq}&control={control}"
    ))
}

/// What the caller should do after one server frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FrameOutcome {
    Continue,
    Ended,
}

/// Render one server frame. Terminal bytes go to `out` verbatim; status lines
/// go to `notes` so they never corrupt the agent's screen stream.
pub(crate) fn render_frame(
    frame: &Value,
    json_lines: bool,
    out: &mut dyn Write,
    notes: &mut dyn Write,
) -> ClientResult<FrameOutcome> {
    let kind = frame["type"].as_str().unwrap_or_default();
    if json_lines {
        writeln!(out, "{frame}").map_err(ClientError::write_failed)?;
    } else {
        match kind {
            "tty" => {
                out.write_all(&tty_bytes(&frame["event"]))
                    .and_then(|()| out.flush())
                    .map_err(ClientError::write_failed)?;
            }
            "attached" | "control" => {
                let mode = if frame["controller"] == true {
                    "control"
                } else {
                    "read-only"
                };
                let _ = write!(notes, "\r\n[jeryu: attached {mode}; Ctrl-] detaches]\r\n");
            }
            "control_lost" => {
                let _ = write!(
                    notes,
                    "\r\n[jeryu: another client took control; now read-only]\r\n"
                );
            }
            "resync" => {
                let _ = write!(
                    notes,
                    "\r\n[jeryu: output skipped; terminal may be incomplete]\r\n"
                );
            }
            "error" => {
                let code = frame["code"].as_str().unwrap_or("error");
                let _ = write!(notes, "\r\n[jeryu: {code}]\r\n");
            }
            "ended" => {
                let state = frame["state"].as_str().unwrap_or("finished");
                let _ = write!(notes, "\r\n[jeryu: agent run {state}]\r\n");
            }
            _ => {}
        }
    }
    Ok(if kind == "ended" {
        FrameOutcome::Ended
    } else {
        FrameOutcome::Continue
    })
}

/// The raw bytes of one TTY event: `bytes_b64` when present, else `text`.
pub(crate) fn tty_bytes(event: &Value) -> Vec<u8> {
    if let Some(encoded) = event["bytes_b64"].as_str() {
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap_or_default();
    }
    event["text"]
        .as_str()
        .unwrap_or_default()
        .as_bytes()
        .to_vec()
}

/// Run the attach session until the run ends, the server closes, or the user
/// detaches.
pub(crate) fn run(
    api_url: &str,
    run_id: &str,
    after_seq: u64,
    control: bool,
    json_lines: bool,
    out: &mut dyn Write,
) -> ClientResult<()> {
    let url = attach_url(api_url, run_id, after_seq, control)?;
    let mut request = url
        .into_client_request()
        .map_err(|_| ClientError::Invalid("invalid attach URL".into()))?;
    if let Some(token) = api_token()? {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| ClientError::Invalid("invalid API token".into()))?;
        request.headers_mut().insert("authorization", value);
    }
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ClientError::NotWired("cannot start the attach runtime".into()))?;
    let interactive = control && !json_lines && std::io::stdin().is_terminal();
    let _raw = interactive.then(RawMode::enter).flatten();
    runtime.block_on(session(request, interactive, json_lines, out))
}

async fn session(
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
    interactive: bool,
    json_lines: bool,
    out: &mut dyn Write,
) -> ClientResult<()> {
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| ClientError::NotWired(format!("attach failed: {error}")))?;
    let (mut sink, mut stream) = socket.split();
    let mut notes = std::io::stderr();
    let mut stdin = tokio::io::stdin();
    let mut keys = [0_u8; 4096];
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        .map_err(|_| ClientError::NotWired("cannot watch terminal size".into()))?;
    if interactive && let Some(resize) = resize_frame() {
        let _ = sink.send(Message::Text(resize.to_string())).await;
    }
    loop {
        tokio::select! {
            message = stream.next() => {
                let Some(Ok(message)) = message else { return Ok(()) };
                let text = match message {
                    Message::Text(text) => text,
                    Message::Close(_) => return Ok(()),
                    _ => continue,
                };
                let frame: Value = serde_json::from_str(&text)
                    .map_err(|_| ClientError::Invalid("server sent invalid attach frame".into()))?;
                if render_frame(&frame, json_lines, out, &mut notes)? == FrameOutcome::Ended {
                    return Ok(());
                }
            }
            read = stdin.read(&mut keys), if interactive => {
                let length = match read { Ok(0) | Err(_) => return Ok(()), Ok(length) => length };
                let (input, detach) = match keys[..length].iter().position(|&b| b == DETACH_KEY) {
                    Some(at) => (&keys[..at], true),
                    None => (&keys[..length], false),
                };
                if !input.is_empty() {
                    let sent = sink.send(Message::Binary(input.to_vec())).await;
                    if sent.is_err() { return Ok(()) }
                }
                if detach {
                    let _ = sink.send(Message::Close(None)).await;
                    let _ = write!(notes, "\r\n[jeryu: detached]\r\n");
                    return Ok(());
                }
            }
            _ = winch.recv(), if interactive => {
                if let Some(resize) = resize_frame() {
                    let _ = sink.send(Message::Text(resize.to_string())).await;
                }
            }
        }
    }
}

fn resize_frame() -> Option<Value> {
    let size = rustix::termios::tcgetwinsize(std::io::stdout()).ok()?;
    (size.ws_row > 0).then(|| {
        json!({"type": "control", "request_id": "winch",
            "command": {"kind": "resize_pty", "cols": size.ws_col, "rows": size.ws_row}})
    })
}

/// Puts the local terminal in raw mode and restores it on drop.
struct RawMode(rustix::termios::Termios);

impl RawMode {
    fn enter() -> Option<Self> {
        use rustix::termios::{OptionalActions, tcgetattr, tcsetattr};
        let original = tcgetattr(std::io::stdin()).ok()?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(std::io::stdin(), OptionalActions::Now, &raw).ok()?;
        Some(Self(original))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let now = rustix::termios::OptionalActions::Now;
        let _ = rustix::termios::tcsetattr(std::io::stdin(), now, &self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_url_maps_scheme_and_rejects_bad_ids() {
        assert_eq!(
            attach_url("https://jeryu.example/", "ar-1", 7, true).unwrap(),
            "wss://jeryu.example/api/v1/agent-runs/ar-1/attach?after_seq=7&control=true"
        );
        assert!(
            attach_url("http://127.0.0.1:8787", "ar-1", 0, false)
                .unwrap()
                .starts_with("ws://127.0.0.1:8787/")
        );
        assert!(attach_url("http://h", "../admin", 0, false).is_err());
        assert!(attach_url("ftp://h", "ar-1", 0, false).is_err());
    }

    #[test]
    fn render_writes_raw_bytes_and_ends_on_ended() {
        let raw = [0x1b_u8, b'[', b'2', b'J', 0xff];
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let (mut out, mut notes) = (Vec::new(), Vec::new());
        let tty = json!({"type": "tty", "event": {"seq": 1, "bytes_b64": encoded}});
        assert_eq!(
            render_frame(&tty, false, &mut out, &mut notes).unwrap(),
            FrameOutcome::Continue
        );
        assert_eq!(out, raw, "terminal bytes pass through unchanged");
        let ended = json!({"type": "ended", "state": "succeeded"});
        assert_eq!(
            render_frame(&ended, false, &mut out, &mut notes).unwrap(),
            FrameOutcome::Ended
        );
        assert!(String::from_utf8_lossy(&notes).contains("succeeded"));
        assert_eq!(out, raw, "status lines never enter the terminal stream");
    }

    #[test]
    fn json_mode_prints_one_frame_per_line() {
        let (mut out, mut notes) = (Vec::new(), Vec::new());
        let frame = json!({"type": "tty", "event": {"seq": 2, "text": "hi"}});
        render_frame(&frame, true, &mut out, &mut notes).unwrap();
        let line: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(line["event"]["text"], "hi");
        assert_eq!(tty_bytes(&line["event"]), b"hi");
    }
}
