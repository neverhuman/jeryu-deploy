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
async fn attach_drives_a_real_pty_agent_end_to_end() {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    let state = Arc::new(WebState::new(ForgeCore::new()));
    let temp = tempfile::tempdir().expect("workspace");
    let repo_root = temp.path().join("repo");
    std::fs::create_dir_all(&repo_root).expect("repo root");
    let script = repo_root.join("agent.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf 'READY\\n'\nread line\nprintf 'GOT:%s\\n' \"$line\"\n",
    )
    .expect("script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let claim = jeryu_runnerd::WorkcellClaimRequest {
        agent_id: "agent-attach".into(),
        workspace_root: temp.path().to_path_buf(),
        repo_roots: vec![repo_root.clone()],
        branch_budget: 2,
        runner_id: "runner-a".into(),
        runner_epoch: 7,
        git_status_summary: "clean".into(),
        ci_snapshot_age_ms: None,
        startup: jeryu_runnerd::StartupSync::Rebased {
            main_ref: "refs/heads/main".into(),
            base_sha: "base".into(),
            head_sha: "head".into(),
        },
    };
    let (workcell_id, runner_epoch) = {
        let mut manager = state.workcells.lock().expect("workcells");
        let held = manager
            .hold_failed_tree(jeryu_runnerd::HoldFailedTreeRequest {
                claim,
                ci_run_id: "ci-1".into(),
                failed_run_id: "failed-1".into(),
                failed_receipt_id: "receipt-1".into(),
                failure_log_digest: "sha256:abc".into(),
            })
            .expect("hold");
        let repairing = manager
            .begin_live_repair(&held.workcell_id, held.runner_epoch)
            .expect("repair");
        (repairing.workcell_id, repairing.runner_epoch)
    };
    let body = json!({
        "source": {"kind": "workcell", "workcell_id": workcell_id, "runner_epoch": runner_epoch},
        "io_mode": "pty", "repo_root": PathBuf::from(&repo_root), "program": "agent.sh",
        "budget": {"wall_secs": 10, "output_bytes": 65536}, "require_cgroup": false,
    });
    let response = super::start(State(state.clone()), Bytes::from(body.to_string())).await;
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let start: Value = serde_json::from_slice(&bytes).expect("json");
    let run_id = start["agent_run_id"].as_str().expect("run id").to_string();
    let base = serve(state.clone()).await;
    let mut client = connect(format!("{base}/{run_id}/attach?control=true")).await;
    let hello = next_of(&mut client, "attached").await;
    if hello["controller"] != true {
        let status = state.agent_runs.status(&run_id).expect("status");
        if status.error_code.as_deref() == Some("agent_run_sandbox_unavailable") {
            eprintln!("SKIP attach pty e2e: sandbox unavailable");
            return;
        }
        panic!("run ended before attach: {:?}", status.error_code);
    }
    let mut screen = String::new();
    let mut sent = false;
    loop {
        let frame = next_json(&mut client).await;
        match frame["type"].as_str() {
            Some("tty") => {
                let event = &frame["event"];
                screen.push_str(event["text"].as_str().unwrap_or_default());
                if !sent && screen.contains("READY") {
                    client
                        .send(WsMessage::Binary(b"typed-over-ws\n".to_vec()))
                        .await
                        .expect("keys");
                    sent = true;
                }
            }
            Some("ended") => break,
            _ => {}
        }
    }
    assert!(
        screen.contains("GOT:typed-over-ws"),
        "agent echoed input: {screen:?}"
    );
    let status = state.agent_runs.status(&run_id).expect("status");
    assert_eq!(status.state, AgentRunState::Succeeded);
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
