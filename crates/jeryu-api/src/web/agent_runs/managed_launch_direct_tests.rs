use super::*;
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;

#[test]
fn managed_launch_direct_legacy_drivers_refuse_before_thread_or_host_write() {
    let root = tempfile::tempdir().expect("host sentinel root");
    let sentinel = root.path().join("sentinel");
    std::fs::write(&sentinel, b"immutable direct-driver sentinel").expect("write fixture");
    let inode = std::fs::metadata(&sentinel).expect("inode").ino();
    let digest = Sha256::digest(std::fs::read(&sentinel).expect("sentinel bytes"));
    let spec = CommandSpec {
        program: "/bin/sh".to_string(),
        args: vec![
            "-c".to_string(),
            format!("printf changed > '{}'", sentinel.display()),
        ],
        env: Default::default(),
    };
    let store = AgentRunStore::new();
    for (index, backend) in [PtyBackend::Native, PtyBackend::DockerHost]
        .into_iter()
        .enumerate()
    {
        let direct_id = format!("direct-denied-{index}");
        store.seed_test_run(&direct_id, 8);
        let (_, receive) = mpsc::channel();
        spawn_driver_thread(DriverThreadInit {
            store: store.clone(),
            run_id: direct_id.clone(),
            repo_root: root.path().to_path_buf(),
            spec: spec.clone(),
            io_mode: AgentRunIoMode::Pty,
            backend,
            timeout: Duration::from_secs(1),
            output_budget: 1024,
            require_cgroup: true,
            control_rx: receive,
            purge_seeded_auth: false,
        });
        let session_id = format!("session-denied-{index}");
        store.seed_test_run(&session_id, 8);
        let (_, receive) = mpsc::channel();
        spawn_session_agent(
            &store,
            SessionAgentSpawn {
                run_id: session_id.clone(),
                workspace: root.path().to_path_buf(),
                spec: Some(spec.clone()),
                backend,
                control_rx: receive,
                timeout: Duration::from_secs(1),
                output_budget: 1024,
                require_cgroup: true,
                purge_seeded_auth: false,
            },
        );
        for id in [&direct_id, &session_id] {
            assert_eq!(
                store.status(id).expect("denied run").state,
                AgentRunState::Failed
            );
            assert!(store.status(id).expect("denied run").error_code.is_some());
        }
    }
    assert!(
        store.legacy_preparations().is_empty(),
        "no thread or legacy driver preparation"
    );
    assert_eq!(std::fs::metadata(&sentinel).expect("inode").ino(), inode);
    assert_eq!(
        Sha256::digest(std::fs::read(&sentinel).expect("bytes")),
        digest
    );
}
