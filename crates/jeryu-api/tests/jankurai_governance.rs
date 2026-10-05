use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const TAG: &str = "v1.6.11-deadlang-precision-split.6";
const REV: &str = "2ec7eb2af47e16d2e0dce0bed31c521be4322920";
const TREE: &str = "d3c63d138583404a57dc3177f0972952b1b399c7";
const ARCHIVE_SHA256: &str = "66849f13399e5a8b5d461c48a27322fe3b29a83e86cc5ce41d8c5d89e5d045ea";
const BINARY_SHA256: &str = "9aa101c10cdc40c8a15e3f5c1341be3bce78e86f61754d849bbe8229e485a783";
const MANIFEST_COMMIT: &str = "4b6c49e32d273eb4846c9320108bafc9045dc686";
const MANIFEST_TREE: &str = "80f6a86698bbef3b67b72dbdcba5acf9c041b698";
const MANIFEST_SHA256: &str = "d5c8e3adcdd1f9f32499d9f36e6d7055aef7186ce7bcca4d0fb56c5962b71901";
const IMAGE_RECEIPT_SHA256: &str =
    "521df0817f22e3786b7d71c00884a4ede8abe8b57b0b4cd644ef6d88970718d2";

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    let path = repository_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[test]
fn proptest_equivalent_generated_jankurai_consumers_share_one_closed_identity() {
    // The forge bridge and the sandbox image bake a binary in, so they carry the pin.
    let full_identity_consumers = [
        "crates/jeryu-api/src/ci_bridge.rs",
        "images/agent-sandbox/Dockerfile",
        "images/agent-sandbox/jankurai-installation-receipt.json",
        "ops/agent-sandbox/smoke.sh",
    ];
    let identity_properties = [TAG, REV, TREE, ARCHIVE_SHA256, BINARY_SHA256];

    for relative in full_identity_consumers {
        let content = read(relative);
        for expected in identity_properties {
            assert!(
                content.contains(expected),
                "{relative} omitted governed identity property {expected}"
            );
        }
    }

    // Host CI verifies the installed auditor and its receipt instead: no pin of its own.
    let pin_free_consumers = [
        ".github/workflows/ci-fast.yml",
        ".github/workflows/jankurai.yml",
        ".github/workflows/proof-evidence.yml",
        ".github/workflows/release.yml",
        ".github/workflows/security.yml",
        "ops/ci/common.sh",
        "ops/ci/ensure-jankurai.sh",
        "ops/ci/lib.sh",
        "ops/ci/pr-ci.sh",
        "scripts/ci-doctor.sh",
    ];
    for relative in pin_free_consumers {
        let content = read(relative);
        assert!(
            !content.contains(BINARY_SHA256),
            "{relative} carries a Jankurai pin; it must verify the installed receipt"
        );
    }

    let active_projection = [
        ".github/workflows/ci-fast.yml",
        ".github/workflows/jankurai.yml",
        ".github/workflows/proof-evidence.yml",
        ".github/workflows/release.yml",
        ".github/workflows/security.yml",
        "CHANGELOG.md",
        "agent/native-cli-manifest.toml",
        "crates/jeryu-api/src/ci_bridge.rs",
        "docs/governed-jankurai.md",
        "images/agent-sandbox/Dockerfile",
        "images/agent-sandbox/README.md",
        "images/agent-sandbox/jankurai-installation-receipt.json",
        "ops/agent-sandbox/smoke.sh",
        "ops/ci/common.sh",
        "ops/ci/ensure-jankurai.sh",
        "ops/ci/lib.sh",
        "ops/ci/pr-ci.sh",
        "ops/ci/test-governed-jankurai.sh",
        "scripts/ci-doctor.sh",
    ];
    let retired_identity = [
        "v1.6.11-deadlang-precision-split.1",
        "dface7397fe24d46b0b1885ddd5782c34edbff49",
        "34a8a1fb59bc4ebfadf12c45d95f169d06acc781",
        "2fbca5d04083e3c8d32f383d5b6b4520b8911690b26968c6fbcb210e1202b938",
        "fdb42e5fa7d9851c0729e59bf1e582c895aa9cfc03a7175b420c6025d2fd014e",
    ];

    for relative in active_projection {
        let content = read(relative);
        for retired in retired_identity {
            assert!(
                !content.contains(retired),
                "{relative} retains retired active identity {retired}"
            );
        }
    }
}

#[test]
fn integration_receipt_and_release_broker_contract_remain_fail_closed() {
    let receipt: serde_json::Value = serde_json::from_str(&read(
        "images/agent-sandbox/jankurai-installation-receipt.json",
    ))
    .expect("installation receipt must be valid JSON");

    assert_eq!(receipt["binary"]["sha256"], BINARY_SHA256);
    assert_eq!(receipt["source"]["tag"], TAG);
    assert_eq!(receipt["source"]["commit"], REV);
    assert_eq!(receipt["source"]["tree"], TREE);
    assert_eq!(receipt["source"]["archive_sha256"], ARCHIVE_SHA256);
    assert_eq!(receipt["governance"]["manifest_commit"], MANIFEST_COMMIT);
    assert_eq!(receipt["governance"]["manifest_tree"], MANIFEST_TREE);
    assert_eq!(receipt["governance"]["manifest_sha256"], MANIFEST_SHA256);
    assert_eq!(receipt["governance"]["protected_main"], true);
    assert_eq!(receipt["governance"]["status"], "governed");
    assert_eq!(receipt["conclusion"], "success");
    assert_eq!(receipt["test_mode"], false);

    for relative in ["ops/ci/ensure-jankurai.sh", "ops/ci/lib.sh"] {
        let verifier = read(relative);
        for required in [
            "mode=release-broker",
            "/opt/jain-ci/authority/release-bin/jankurai",
            "expected mode 0555 and one link",
            "release broker Jankurai rejects caller receipt authority",
        ] {
            assert!(
                verifier.contains(required),
                "{relative} omitted broker custody invariant {required}"
            );
        }
    }

    let image = read("images/agent-sandbox/Dockerfile");
    let smoke = read("ops/agent-sandbox/smoke.sh");
    assert!(image.contains(IMAGE_RECEIPT_SHA256));
    assert!(smoke.contains(IMAGE_RECEIPT_SHA256));
}

#[test]
fn release_dependencies_are_immutable_git_sources_without_sibling_paths() {
    let root: toml::Value = toml::from_str(&read("Cargo.toml")).expect("parse root Cargo.toml");
    let patches = root
        .get("patch")
        .and_then(toml::Value::as_table)
        .expect("release graph must declare its Core source unifier");
    assert_eq!(
        patches.len(),
        3,
        "release graph may patch only the historical Core, Intelligence and CI Runner sources"
    );

    // jeryu-api and the historical Intelligence release both name the GitHub
    // runner source at split.0; the unifier moves every runner package to the
    // reviewed local-forge split.3 tag (the cgroup capability-probe fix), so the
    // graph has exactly one runner identity.
    let runner_patches = patches
        .get("https://github.com/neverhuman/jeryu-ci-runner.git")
        .and_then(toml::Value::as_table)
        .expect("historical CI Runner source patch must be a table");
    let runner_packages = [
        "jeryu-agent-stream",
        "jeryu-agentbridge",
        "jeryu-ci-compiler",
        "jeryu-ci-ir",
        "jeryu-ci-scheduler",
        "jeryu-runner-core",
        "jeryu-runner-microvm",
        "jeryu-runner-native",
        "jeryu-runner-oci",
        "jeryu-runner-protocol",
        "jeryu-runner-registry",
        "jeryu-runnerd",
        "jeryu-sandbox-linux",
    ];
    assert_eq!(
        runner_patches
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        runner_packages,
        "CI Runner unifier must list exactly the runner packages in the graph"
    );
    for package in runner_packages {
        let source = runner_patches
            .get(package)
            .and_then(toml::Value::as_table)
            .unwrap_or_else(|| panic!("{package} CI Runner unifier must be a table"));
        assert_eq!(
            source.get("git").and_then(toml::Value::as_str),
            Some("http://127.0.0.1:8787/git/jeryu/jeryu-ci-runner.git")
        );
        assert_eq!(
            source.get("tag").and_then(toml::Value::as_str),
            Some("jeryu-ci-runner-v5.0.0-split.3")
        );
        assert!(
            source.get("path").is_none(),
            "{package} must not resolve from a sibling path"
        );
    }

    let core_patches = patches
        .get("https://github.com/neverhuman/jeryu-core.git")
        .and_then(toml::Value::as_table)
        .expect("historical Core source patch must be a table");
    assert_eq!(
        core_patches.len(),
        2,
        "Core unifier must contain only jeryu-core and jeryu-proof"
    );
    for package in ["jeryu-core", "jeryu-proof"] {
        let source = core_patches
            .get(package)
            .and_then(toml::Value::as_table)
            .unwrap_or_else(|| panic!("{package} Core unifier must be a table"));
        assert_eq!(
            source.get("git").and_then(toml::Value::as_str),
            Some("http://127.0.0.1:8787/git/jeryu/jeryu-core.git")
        );
        assert_eq!(
            source.get("tag").and_then(toml::Value::as_str),
            Some("jeryu-core-v5.0.0-split.11")
        );
        assert!(
            source.get("path").is_none(),
            "{package} must not resolve from a sibling path"
        );
    }
    assert!(
        !read("Cargo.toml").contains("path = \"../jeryu-"),
        "release Cargo.toml must not contain sibling Jeryu paths"
    );
    let intelligence_patches = patches
        .get("https://github.com/neverhuman/jeryu-intelligence.git")
        .and_then(toml::Value::as_table)
        .expect("historical Intelligence source patch must be a table");
    assert_eq!(
        intelligence_patches.len(),
        1,
        "Intelligence unifier must contain only jeryu-rustjet"
    );
    let rustjet_source = intelligence_patches
        .get("jeryu-rustjet")
        .and_then(toml::Value::as_table)
        .expect("jeryu-rustjet unifier must be a table");
    assert_eq!(
        rustjet_source.get("git").and_then(toml::Value::as_str),
        Some("http://127.0.0.1:8787/git/jeryu/jeryu-intelligence.git")
    );
    assert_eq!(
        rustjet_source.get("tag").and_then(toml::Value::as_str),
        Some("jeryu-intelligence-v5.0.0-split.3")
    );
    assert!(
        rustjet_source.get("path").is_none(),
        "jeryu-rustjet must not resolve from a sibling path"
    );

    let api: toml::Value =
        toml::from_str(&read("crates/jeryu-api/Cargo.toml")).expect("parse API Cargo.toml");
    let dependencies = api
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .expect("API dependencies must be a table");
    let expected_groups = [
        (
            "jeryu-core-v5.0.0-split.11",
            &[
                "jeryu-core",
                "jeryu-enterprise",
                "jeryu-gitd",
                "jeryu-readmodel",
            ][..],
        ),
        (
            "jeryu-ci-runner-v5.0.0-split.0",
            &[
                "jeryu-agent-stream",
                "jeryu-agentbridge",
                "jeryu-ci-compiler",
                "jeryu-ci-ir",
                "jeryu-runner-core",
                "jeryu-runner-oci",
                "jeryu-runnerd",
            ][..],
        ),
        (
            "jeryu-intelligence-v5.0.0-split.3",
            &["jeryu-autonomy", "jeryu-codegraph", "jeryu-mcp"][..],
        ),
        (
            "jeryu-release-ops-v5.0.0-split.0",
            &["jeryu-bench", "jeryu-obs", "jeryu-wsversion"][..],
        ),
        ("jeryu-jira-v5.0.0-split.2", &["jeryu-jira"][..]),
    ];
    let expected_count: usize = expected_groups
        .iter()
        .map(|(_, packages)| packages.len())
        .sum();
    assert_eq!(
        dependencies
            .keys()
            .filter(|name| name.starts_with("jeryu-"))
            .count(),
        expected_count,
        "internal dependency set must remain closed"
    );
    for (tag, packages) in expected_groups {
        for package in packages {
            let dependency = dependencies
                .get(*package)
                .and_then(toml::Value::as_table)
                .unwrap_or_else(|| panic!("{package} dependency must be an explicit table"));
            let dependency_git = dependency.get("git").and_then(toml::Value::as_str);
            assert!(
                dependency_git.is_some(),
                "{package} must declare a Git source"
            );
            if tag == "jeryu-core-v5.0.0-split.11" {
                assert_eq!(
                    dependency_git,
                    Some("http://127.0.0.1:8787/git/jeryu/jeryu-core.git"),
                    "{package} must resolve from the local Jeryu Core authority"
                );
            }
            assert_eq!(
                dependency.get("tag").and_then(toml::Value::as_str),
                Some(tag),
                "{package} must declare the governed immutable tag"
            );
            assert!(
                dependency.get("path").is_none(),
                "{package} must not declare a sibling path"
            );
        }
    }

    let lock: toml::Value = toml::from_str(&read("Cargo.lock")).expect("parse Cargo.lock");
    assert!(
        lock.get("patch").is_none(),
        "Cargo.lock must not retain unused patch records"
    );
    let packages = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .expect("Cargo.lock packages must be an array");
    let mut core_count = 0;
    let mut proof_count = 0;
    let mut internal_counts = BTreeMap::new();
    for package in packages {
        let table = package.as_table().expect("lock package must be a table");
        let name = table
            .get("name")
            .and_then(toml::Value::as_str)
            .expect("lock package must have a name");
        if !name.starts_with("jeryu-")
            || matches!(name, "jeryu-api" | "jeryu-cli" | "jeryu-split-tool")
        {
            continue;
        }
        *internal_counts.entry(name).or_insert(0_usize) += 1;
        let source = table
            .get("source")
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("{name} must have an immutable Git lock source"));
        assert!(
            source.starts_with("git+"),
            "{name} must resolve from Git, got {source}"
        );
        if name == "jeryu-core" {
            core_count += 1;
            assert_eq!(
                source,
                "git+http://127.0.0.1:8787/git/jeryu/jeryu-core.git?tag=jeryu-core-v5.0.0-split.11#49d5593824ef10767611abbcc281c511bd5face5"
            );
        } else if name == "jeryu-proof" {
            proof_count += 1;
            assert_eq!(
                source,
                "git+http://127.0.0.1:8787/git/jeryu/jeryu-core.git?tag=jeryu-core-v5.0.0-split.11#49d5593824ef10767611abbcc281c511bd5face5"
            );
        } else if name == "jeryu-rustjet" {
            assert_eq!(
                source,
                "git+http://127.0.0.1:8787/git/jeryu/jeryu-intelligence.git?tag=jeryu-intelligence-v5.0.0-split.3#323753b75c65220db9b448af4309197a72cb44f5"
            );
        }
    }
    assert_eq!(
        core_count, 1,
        "release graph must contain one Core identity"
    );
    assert_eq!(
        proof_count, 1,
        "release graph must contain one proof identity"
    );
    assert_eq!(
        internal_counts.get("jeryu-rustjet"),
        Some(&1),
        "release graph must contain one Rustjet identity"
    );
    let duplicates: Vec<_> = internal_counts
        .into_iter()
        .filter(|(_, count)| *count != 1)
        .collect();
    assert!(
        duplicates.is_empty(),
        "release graph contains duplicate internal identities: {duplicates:?}"
    );
}
