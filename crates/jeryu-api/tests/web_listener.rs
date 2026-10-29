#![cfg(feature = "web")]

use jeryu_api::web::{WebServerConfig, serve_on};

#[tokio::test]
async fn prebound_listener_checks_actual_address_for_local_trust() {
    let temp = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let config = WebServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        spa_dir: temp.path().join("spa"),
        data_dir: temp.path().join("data"),
        git_storage_root: temp.path().join("git"),
        split_manifests: Vec::new(),
        auth_required: true,
        trust_local_dev: true,
        secure_cookies: false,
    };
    let err = serve_on(listener, config)
        .await
        .expect_err("public listener must fail even with a loopback config");
    assert!(err.to_string().contains("trust_local_dev"));
    assert!(!temp.path().join("data").exists());
}
