use super::*;

/// A `WebState` whose read model has one saturated pool, so the activity
/// and pool scopes produce non-trivial snapshot frames.
fn ws_state_with_pool() -> WebState {
    use jeryu_readmodel::{PoolActivity, PoolRollup, RepoActivity};
    let mut state = WebState::new(ForgeCore::new());
    let mut pool = PoolRollup::new("trusted");
    pool.active_slots = 2;
    pool.running_jobs = 2;
    pool.queued_jobs = 3; // saturated
    pool.online_runners = 2;
    state.tui.pool_activity = PoolActivity {
        repos: vec![RepoActivity {
            repo: "alice/jeryu".into(),
            queued_jobs: 3,
            running_jobs: 2,
            ..RepoActivity::default()
        }],
        pools: vec![pool],
        ..PoolActivity::default()
    };
    state
}

#[test]
fn subscribe_frame_yields_scopes_and_snapshot_events() {
    let state = ws_state_with_pool();
    // A real client `subscribe` frame per the ClientWsMessage contract.
    let frame = json!({
        "type": "subscribe",
        "subscriptions": [
            { "scope": "global.activity", "filters": {} },
            { "scope": "pool.trusted", "filters": {} },
            { "scope": "system.health", "filters": {} },
        ],
    });
    // It deserializes into the typed wire contract (format is genuine).
    let parsed: jeryu_readmodel::contracts::ClientWsMessage =
        serde_json::from_value(frame.clone()).expect("subscribe frame parses");
    assert!(matches!(
        parsed,
        jeryu_readmodel::contracts::ClientWsMessage::Subscribe { .. }
    ));

    // The handler's scope extractor pulls every requested scope.
    let scopes = requested_scopes(&frame);
    assert_eq!(scopes.len(), 3);

    // Each subscribed scope yields a monotonic Event snapshot frame.
    let mut last_seq = 0u64;
    for scope in &scopes {
        let event = snapshot_event(&state, scope)
            .unwrap_or_else(|| panic!("scope {scope} should produce a snapshot"));
        assert_eq!(&event.scope, scope);
        assert!(event.seq > last_seq, "seq must be strictly monotonic");
        last_seq = event.seq;
        // The frame round-trips as a ServerWsMessage::Event on the wire.
        let msg = ServerWsMessage::Event { event };
        let encoded = serde_json::to_string(&msg).unwrap();
        assert!(encoded.contains("\"type\":\"event\""));
        assert!(encoded.contains(scope.as_str()));
    }

    // The activity snapshot reports the saturated pool's bottleneck.
    let activity = snapshot_event(&state, "global.activity").unwrap();
    let bottlenecks = activity.payload.get("bottlenecks").unwrap();
    assert!(
        bottlenecks.as_array().is_some_and(|b| !b.is_empty()),
        "saturated pool must surface a bottleneck"
    );
}

#[test]
fn unknown_scope_produces_no_snapshot() {
    let state = ws_state_with_pool();
    assert!(snapshot_event(&state, "pool.does-not-exist").is_none());
    assert!(snapshot_event(&state, "totally.unknown").is_none());
}

#[test]
fn ws_hub_seq_is_monotonic_and_tracks_subscribers() {
    let hub = WsHub::new();
    assert_eq!(hub.current_seq(), 0);
    let a = hub.next_seq();
    let b = hub.next_seq();
    assert!(b > a);
    assert_eq!(hub.current_seq(), b);

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let conn = hub.register(tx);
    let mut scopes = BTreeSet::new();
    scopes.insert("global.activity".to_string());
    scopes.insert("pool.trusted".to_string());
    hub.set_scopes(conn, &scopes);
    hub.remove_scopes(conn, &["pool.trusted".to_string()]);
    // Unregister must not panic and leaves the hub usable.
    hub.unregister(conn);
    assert!(hub.next_seq() > b);
}

#[test]
fn ws_hub_publish_fans_out_to_subscribed_connections_only() {
    let hub = WsHub::new();
    let (tx_sub, mut rx_sub) = tokio::sync::mpsc::unbounded_channel();
    let (tx_other, mut rx_other) = tokio::sync::mpsc::unbounded_channel();
    let subscribed = hub.register(tx_sub);
    let other = hub.register(tx_other);
    let mut scopes = BTreeSet::new();
    scopes.insert("tool_finder.scan".to_string());
    hub.set_scopes(subscribed, &scopes);
    let mut other_scopes = BTreeSet::new();
    other_scopes.insert("global.activity".to_string());
    hub.set_scopes(other, &other_scopes);

    let delivered = hub.publish("tool_finder.scan", |seq| {
        jeryu_readmodel::contracts::WebEvent {
            seq,
            timestamp: "t".to_string(),
            scope: "tool_finder.scan".to_string(),
            kind: "tool_finder.scan.progress".to_string(),
            entity: "system/host".to_string(),
            summary: "scan progress".to_string(),
            payload: json!({}),
        }
    });
    assert_eq!(delivered, 1, "only the subscribed connection receives");
    match rx_sub.try_recv() {
        Ok(ServerWsMessage::Event { event }) => {
            assert_eq!(event.scope, "tool_finder.scan");
            assert!(event.seq > 0);
        }
        other => panic!("expected pushed event, got {other:?}"),
    }
    assert!(
        rx_other.try_recv().is_err(),
        "unsubscribed conn gets nothing"
    );

    // A connection whose socket loop died (receiver dropped) is pruned.
    drop(rx_sub);
    let delivered = hub.publish("tool_finder.scan", |seq| {
        jeryu_readmodel::contracts::WebEvent {
            seq,
            timestamp: "t".to_string(),
            scope: "tool_finder.scan".to_string(),
            kind: "tool_finder.scan.progress".to_string(),
            entity: "system/host".to_string(),
            summary: "scan progress".to_string(),
            payload: json!({}),
        }
    });
    assert_eq!(delivered, 0, "dead subscriber pruned on publish");
}

#[test]
fn hello_frame_reports_current_seq() {
    let state = ws_state_with_pool();
    // Hand out two sequences, then the hello frame must echo current_seq.
    let _ = state.ws.next_seq();
    let _ = state.ws.next_seq();
    match hello_message(&state) {
        ServerWsMessage::Hello { current_seq, .. } => assert_eq!(current_seq, 2),
        other => panic!("expected hello, got {other:?}"),
    }
}

#[test]
fn unsubscribe_frame_extracts_scopes() {
    let frame = json!({ "type": "unsubscribe", "scopes": ["pool.trusted", "system.health"] });
    let dropped = unsubscribe_scopes(&frame);
    assert_eq!(dropped, vec!["pool.trusted", "system.health"]);
}
