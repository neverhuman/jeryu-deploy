//! Process-level proof that the shipped `jeryu` binary fails closed.
//!
//! The library tests drive `dispatch` in-process; only a spawned binary proves
//! that the production entrypoint is wired to the fail-closed client instead of
//! the in-memory fixtures.

use std::{net::TcpListener, path::Path, process::Command};
use tempfile::TempDir;

fn isolated_command(home: &Path) -> Command {
    let binary =
        std::env::var_os("JERYU_TEST_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_jeryu").into());
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .current_dir(home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    command
}

#[test]
fn unreachable_api_and_unimplemented_operations_cannot_report_success() {
    let temp = TempDir::new().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let result = isolated_command(temp.path())
        .args(["--api-url", &url, "forge", "repo", "list"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    let result = isolated_command(temp.path())
        .args(["cache", "self-test"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
}
