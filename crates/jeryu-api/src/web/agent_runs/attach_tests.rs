//! Live attach: replay, fan-out, single-writer control lease, and run end.

use std::sync::mpsc::Receiver;

use axum::http::HeaderValue;
use futures_util::SinkExt;
use jeryu_core::ForgeCore;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::*;
use crate::web::auth::same_origin;
use jeryu_core::AccountSummary;

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn seed_controllable(store: &AgentRunStore, run_id: &str) -> Receiver<AgentControl> {
    store.seed_test_run(run_id, 64);
    let (tx, rx) = mpsc::channel();
    let mut inner = store.inner.lock().expect("agent run store mutex");
    inner.runs.get_mut(run_id).expect("seeded run").control_tx = Some(tx);
    rx
}

fn account(login: &str, role: jeryu_core::UserRole) -> AccountSummary {
    AccountSummary {
        login: login.into(),
        display_name: login.into(),
        role,
        status: jeryu_core::AccountStatus::Active,
        auth_epoch: 0,
        must_change_password: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

/// Serve the attach route as an authenticated admin.
async fn serve(state: Arc<WebState>) -> String {
    serve_as(state, account("operator", jeryu_core::UserRole::Admin)).await
}

async fn serve_as(state: Arc<WebState>, account: AccountSummary) -> String {
    let app = axum::Router::new()
        .route(
            "/api/v1/agent-runs/:id/attach",
            axum::routing::get(super::attach),
        )
        .layer(axum::Extension(account))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    format!("ws://{address}/api/v1/agent-runs")
}

async fn connect(url: String) -> Client {
    tokio_tungstenite::connect_async(url)
        .await
        .expect("websocket connect")
        .0
}

async fn next_json(client: &mut Client) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .expect("frame before timeout")
            .expect("open stream")
            .expect("valid frame");
        if let WsMessage::Text(text) = message {
            return serde_json::from_str(&text).expect("json frame");
        }
    }
}

async fn next_of(client: &mut Client, kind: &str) -> Value {
    loop {
        let frame = next_json(client).await;
        if frame["type"] == kind {
            return frame;
        }
    }
}

async fn send_json(client: &mut Client, value: Value) {
    client
        .send(WsMessage::Text(value.to_string()))
        .await
        .expect("send frame");
}

#[test]
fn control_lease_moves_to_the_newest_holder_and_release_is_fenced() {
    let store = AgentRunStore::new();
    store.seed_test_run("ar-lease", 4);
    let first = store.take_control("ar-lease", "alice/1").expect("lease");
    let second = store.take_control("ar-lease", "bob/2").expect("lease");
    assert!(second > first);
    assert!(!store.holds_control("ar-lease", "alice/1", first));
    store.release_control("ar-lease", "alice/1", first);
    assert!(
        store.holds_control("ar-lease", "bob/2", second),
        "a displaced holder cannot release the newer lease"
    );
    store.release_control("ar-lease", "bob/2", second);
    assert!(!store.holds_control("ar-lease", "bob/2", second));
    assert_eq!(store.take_control("missing", "x"), None);
}

#[test]
fn websocket_origin_must_match_host() {
    let headers = |origin: Option<&str>| {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("jeryu.example:8443"));
        if let Some(origin) = origin {
            headers.insert(
                header::ORIGIN,
                HeaderValue::from_str(origin).expect("origin"),
            );
        }
        headers
    };
    assert!(same_origin(&headers(Some("https://jeryu.example:8443"))));
    assert!(!same_origin(&headers(Some("https://evil.example"))));
    assert!(!same_origin(&headers(Some(
        "https://jeryu.example:8443.evil"
    ))));
    assert!(!same_origin(&headers(Some("null"))));
    assert!(!same_origin(&headers(None)));
}

#[tokio::test]
async fn attach_replays_fans_out_and_enforces_single_writer() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let controls = seed_controllable(&state.agent_runs, "ar-attach");
    for seq in 1..=2 {
        let event = test_raw_tty_event("ar-attach", seq, format!("line-{seq}\r\n").as_bytes());
        state.agent_runs.push_test_tty("ar-attach", event);
    }
    let base = serve(state.clone()).await;

    let mut viewer = connect(format!("{base}/ar-attach/attach?after_seq=0")).await;
    let hello = next_json(&mut viewer).await;
    assert_eq!(hello["type"], "attached");
    assert_eq!(hello["controller"], false);
    assert_eq!(next_json(&mut viewer).await["event"]["seq"], 1);
    assert_eq!(next_json(&mut viewer).await["event"]["seq"], 2);

    let mut driver = connect(format!("{base}/ar-attach/attach?after_seq=2&control=true")).await;
    let hello = next_json(&mut driver).await;
    assert_eq!(hello["controller"], true);

    let live = test_raw_tty_event("ar-attach", 3, b"live\r\n");
    state.agent_runs.push_test_tty("ar-attach", live);
    assert_eq!(next_of(&mut viewer, "tty").await["event"]["seq"], 3);
    assert_eq!(next_of(&mut driver, "tty").await["event"]["seq"], 3);

    // A read-only viewer cannot type into the terminal.
    let input = json!({"type": "control", "request_id": "v1",
        "command": {"kind": "send_input", "text": "rm -rf /\n"}});
    send_json(&mut viewer, input).await;
    let refused = next_of(&mut viewer, "error").await;
    assert_eq!(refused["code"], "control_not_held");
    assert_eq!(refused["request_id"], "v1");

    // Raw keystrokes reach the driver byte-for-byte, including a lone byte of
    // a multibyte sequence split across reads and a non-UTF-8 byte.
    let keys = [b'l', b's', 0xe2, 0x82, 0xff, b'\n'];
    driver
        .send(WsMessage::Binary(keys.to_vec()))
        .await
        .expect("send keys");
    assert_eq!(next_of(&mut driver, "ack").await["command"], "send_bytes");
    match controls
        .recv_timeout(Duration::from_secs(5))
        .expect("control")
    {
        AgentControl::SendInput(bytes) => assert_eq!(bytes, keys),
        other => panic!("unexpected control {other:?}"),
    }
    let resize = json!({"type": "control", "request_id": "r1",
        "command": {"kind": "resize_pty", "cols": 120, "rows": 40}});
    send_json(&mut driver, resize).await;
    assert_eq!(next_of(&mut driver, "ack").await["request_id"], "r1");
    assert!(matches!(
        controls.recv_timeout(Duration::from_secs(5)),
        Ok(AgentControl::ResizePty {
            rows: 40,
            cols: 120
        })
    ));

    // Taking control displaces the old controller, which is told and refused.
    send_json(&mut viewer, json!({"type": "take_control"})).await;
    assert_eq!(next_of(&mut viewer, "control").await["controller"], true);
    next_of(&mut driver, "control_lost").await;
    let late = json!({"type": "control", "command": {"kind": "interrupt"}});
    send_json(&mut driver, late).await;
    assert_eq!(
        next_of(&mut driver, "error").await["code"],
        "control_not_held"
    );

    let audit = state.agent_runs.record("ar-attach").expect("run").controls;
    assert!(audit.iter().all(|control| control.actor.is_some()));

    // When the run finishes, already-published output drains, then `ended`.
    let tail = test_raw_tty_event("ar-attach", 4, b"bye\r\n");
    state.agent_runs.push_test_tty("ar-attach", tail);
    {
        let mut inner = state.agent_runs.inner.lock().expect("store");
        inner.runs.get_mut("ar-attach").expect("run").state = AgentRunState::Succeeded;
    }
    assert_eq!(next_of(&mut viewer, "tty").await["event"]["seq"], 4);
    assert_eq!(next_of(&mut viewer, "ended").await["state"], "succeeded");
}

#[tokio::test]
async fn attach_to_unknown_run_is_not_found() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let base = serve(state).await;
    let error = tokio_tungstenite::connect_async(format!("{base}/missing/attach"))
        .await
        .expect_err("upgrade refused");
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        other => panic!("unexpected error {other}"),
    }
}

/// Real PTY driver behind the socket: injected keystrokes reach the agent and
/// its reply comes back as live output, then the run ends cleanly.
#[tokio::test]
async fn attach_component_delivers_input_and_drains_fixture_output_before_ended() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let run = "component-attach";
    let controls = seed_controllable(&state.agent_runs, run);
    state
        .agent_runs
        .push_test_tty(run, test_raw_tty_event(run, 1, b"READY\r\n"));
    let base = serve(state.clone()).await;
    let mut client = connect(format!("{base}/{run}/attach?control=true")).await;
    assert_eq!(next_of(&mut client, "attached").await["controller"], true);
    let replay = next_of(&mut client, "tty").await;
    assert_eq!(replay["event"]["seq"], 1);
    client
        .send(WsMessage::Binary(b"typed-over-ws\n".to_vec()))
        .await
        .unwrap();
    assert_eq!(next_of(&mut client, "ack").await["command"], "send_bytes");
    assert!(
        matches!(controls.recv_timeout(Duration::from_secs(1)),Ok(AgentControl::SendInput(bytes)) if bytes==b"typed-over-ws\n")
    );
    assert_eq!(
        state.agent_runs.status(run).unwrap().state,
        AgentRunState::Running
    );
    state
        .agent_runs
        .push_test_tty(run, test_raw_tty_event(run, 2, b"GOT:typed-over-ws\r\n"));
    state
        .agent_runs
        .simulate_test_completion(run, AgentRunState::Succeeded);
    let output = next_of(&mut client, "tty").await;
    assert_eq!(output["event"]["seq"], 2);
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(output["event"]["bytes_b64"].as_str().unwrap())
            .unwrap(),
        b"GOT:typed-over-ws\r\n"
    );
    assert_eq!(
        next_of(&mut client, "ended").await["state"],
        "succeeded",
        "explicit component completion"
    );
}

#[tokio::test]
async fn non_admin_viewers_cannot_drive_a_host_pty_run() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let _controls = seed_controllable(&state.agent_runs, "ar-viewer");
    let user = account("writer", jeryu_core::UserRole::User);
    let base = serve_as(state.clone(), user).await;
    let mut client = connect(format!("{base}/ar-viewer/attach?control=true")).await;
    let hello = next_of(&mut client, "attached").await;
    assert_eq!(
        hello["controller"], false,
        "control=true is ignored for a non-admin"
    );
    send_json(&mut client, json!({"type": "take_control"})).await;
    assert_eq!(
        next_of(&mut client, "error").await["code"],
        "control_forbidden"
    );
    client
        .send(WsMessage::Binary(b"id\n".to_vec()))
        .await
        .expect("send keys");
    assert_eq!(
        next_of(&mut client, "error").await["code"],
        "control_not_held"
    );
}

#[tokio::test]
async fn rest_send_bytes_rejects_invalid_base64() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    let _controls = seed_controllable(&state.agent_runs, "ar-bytes");
    let bad = AgentControlCommand::SendBytes {
        bytes_b64: "not base64!".into(),
    };
    assert!(state.agent_runs.send_control("ar-bytes", bad).is_err());
    let good = AgentControlCommand::SendBytes {
        bytes_b64: "/w==".into(),
    };
    let ack = state
        .agent_runs
        .send_control("ar-bytes", good)
        .expect("ack");
    assert_eq!(ack.command, "send_bytes");
}

#[test]
fn a_displaced_lease_is_refused_inside_the_store() {
    let store = AgentRunStore::new();
    let controls = seed_controllable(&store, "ar-fence");
    let old = store.take_control("ar-fence", "alice/1").expect("lease");
    store.take_control("ar-fence", "bob/2").expect("take over");
    let input = AgentControlCommand::SendInput {
        text: "late\n".into(),
    };
    assert!(
        store
            .send_control_by("ar-fence", input, Some(("alice/1", old)))
            .is_err()
    );
    assert!(controls.try_recv().is_err(), "nothing reached the PTY");
}

#[test]
fn control_history_is_capped_and_sequences_keep_counting() {
    let store = AgentRunStore::new();
    let _controls = seed_controllable(&store, "ar-history");
    for _ in 0..1100 {
        store
            .send_control("ar-history", AgentControlCommand::Interrupt)
            .expect("send");
    }
    let controls = store.record("ar-history").expect("run").controls;
    assert_eq!(controls.len(), 1024);
    assert_eq!(controls.last().map(|c| c.seq), Some(1100));
}

#[tokio::test]
async fn a_run_admits_a_bounded_number_of_attached_clients() {
    let state = Arc::new(WebState::new(ForgeCore::new()));
    state.agent_runs.seed_test_run("ar-crowd", 8);
    let base = serve(state.clone()).await;
    let mut clients = Vec::new();
    for _ in 0..32 {
        let mut client = connect(format!("{base}/ar-crowd/attach")).await;
        next_of(&mut client, "attached").await;
        clients.push(client);
    }
    let mut extra = connect(format!("{base}/ar-crowd/attach")).await;
    assert_eq!(next_json(&mut extra).await["code"], "attach_viewer_limit");
    drop(clients.pop());
    let mut retry = None;
    // Deadline-based: the slot frees when the server notices the close.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut client = connect(format!("{base}/ar-crowd/attach")).await;
        if next_json(&mut client).await["type"] == "attached" {
            retry = Some(client);
            break;
        }
    }
    assert!(retry.is_some(), "a closed socket frees its slot");
}
