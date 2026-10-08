//! The gateway dials providers itself. Proxy environment variables must not
//! route its credential-bearing requests elsewhere, or the public-address
//! filter would be checking the proxy, not the provider. Setting process
//! environment is unsafe in a threaded test binary, so the parent test runs
//! this binary again with the proxy variables set, and the child relays.

mod common;

use std::io::Read;
use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use jeryu_gateway::frames::GatewayLine;
use serde_json::Value;

const CHILD: &str = "JERYU_GATEWAY_PROXY_CHILD";

#[test]
fn proxy_environment_variables_are_ignored() {
    // A proxy that counts every connection and answers none.
    let proxy = TcpListener::bind("127.0.0.1:0").expect("proxy");
    let url = format!("http://{}", proxy.local_addr().expect("addr"));
    let dialed = Arc::new(AtomicUsize::new(0));
    let counter = dialed.clone();
    std::thread::spawn(move || {
        for stream in proxy.incoming().flatten() {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut stream = stream;
            let _ = stream.read(&mut [0_u8; 256]);
        }
    });
    let mut child = Command::new(std::env::current_exe().expect("test binary"));
    child
        .args([
            "--exact",
            "child_relays_with_proxy_variables_set",
            "--nocapture",
        ])
        .env(CHILD, "1");
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        child.env(name, &url);
    }
    child.env_remove("NO_PROXY").env_remove("no_proxy");
    let output = child.output().expect("child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "child failed: {stdout}");
    assert!(
        stdout.contains("relayed-directly"),
        "child did not relay: {stdout}"
    );
    assert_eq!(
        dialed.load(Ordering::SeqCst),
        0,
        "the gateway dialed the proxy"
    );
}

/// Runs only as the child of the test above, with proxy variables set.
#[tokio::test]
async fn child_relays_with_proxy_variables_set() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let harness = Harness::new().await;
    let capability = harness.capability(|_| {});
    let mut lines = vec![open_line("r-1", &capability, None)];
    lines.extend(body_lines("r-1", b"{}"));
    let response =
        tokio::time::timeout(std::time::Duration::from_secs(20), harness.relay_all(lines))
            .await
            .expect("relay finished");
    let body: Vec<u8> = response
        .iter()
        .filter_map(|line| match line {
            GatewayLine::ResponseBody { bytes_b64, .. } => {
                Some(jeryu_gateway::frames::decode_chunk(bytes_b64).expect("chunk"))
            }
            _ => None,
        })
        .flatten()
        .collect();
    let body: Value = serde_json::from_slice(&body).unwrap_or_else(|_| panic!("{response:?}"));
    assert_eq!(body["injected"], true);
    println!("relayed-directly");
}
