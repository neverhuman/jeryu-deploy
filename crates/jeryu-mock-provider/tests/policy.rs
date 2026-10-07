//! Lane A policy for the mocks: no product crate links them, and the mock
//! provider's source names no real provider host.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn no_product_crate_depends_on_a_mock() {
    let mut checked = 0;
    for entry in std::fs::read_dir(workspace_root().join("crates")).expect("crates dir") {
        let dir = entry.expect("entry").path();
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if name.starts_with("jeryu-mock-") {
            continue;
        }
        let Ok(manifest) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        let manifest: toml::Table = manifest.parse().expect("manifest toml");
        for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(deps) = manifest.get(table).and_then(|deps| deps.as_table()) {
                for mock in ["jeryu-mock-provider", "jeryu-mock-agent"] {
                    assert!(
                        !deps.contains_key(mock),
                        "{name} [{table}] must not depend on {mock}"
                    );
                }
            }
        }
        // A product crate may use the gateway, but never its test-only
        // loopback upstream.
        for table in ["dependencies", "build-dependencies"] {
            let gateway = manifest
                .get(table)
                .and_then(|deps| deps.get("jeryu-gateway"))
                .and_then(|dep| dep.get("features"))
                .and_then(|features| features.as_array());
            let loopback = gateway.is_some_and(|features| {
                features
                    .iter()
                    .any(|f| f.as_str() == Some("loopback-upstream"))
            });
            assert!(
                !loopback,
                "{name} [{table}] enables jeryu-gateway/loopback-upstream"
            );
        }
        checked += 1;
    }
    assert!(checked >= 3, "found the product crates");
}

#[test]
fn mock_provider_source_names_no_real_provider_host() {
    // Built from parts so this file does not contain the hosts either.
    let hosts = [
        ["api.", "anthropic.com"].concat(),
        ["api.", "openai.com"].concat(),
        ["api.", "x.ai"].concat(),
    ];
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for dir in ["src", "tests"] {
        for entry in std::fs::read_dir(root.join(dir)).expect("source dir") {
            let path = entry.expect("entry").path();
            let source = std::fs::read_to_string(&path).expect("source");
            for host in &hosts {
                assert!(
                    !source.contains(host.as_str()),
                    "{} names {host}",
                    path.display()
                );
            }
        }
    }
}

#[tokio::test]
async fn the_mock_binds_loopback_only() {
    let provider = jeryu_mock_provider::MockProvider::start(Default::default())
        .await
        .expect("start");
    assert!(provider.addr.ip().is_loopback());
    assert_ne!(provider.addr.port(), 0);
}
