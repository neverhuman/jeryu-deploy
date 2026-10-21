//! Keep the agent client transport and custody checks in the API gate.
#[test]
fn agent_forge_transport_and_custody() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = std::process::Command::new("python3")
        .args(["-m", "unittest", "discover", "-s", "scripts/agents"])
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .current_dir(root)
        .status()
        .expect("Python 3 is required for the agent forge client tests");
    assert!(status.success(), "agent forge client tests failed");
}
