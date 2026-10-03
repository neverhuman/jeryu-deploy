use std::fs;
use std::path::{Path, PathBuf};

const PROCEDURE: &str = "scripts/release/README.md";

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    let path = repository_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[test]
fn release_procedure_leads_with_the_release_host() {
    let procedure = read(PROCEDURE);
    let first = procedure.lines().next().unwrap_or_default();
    assert!(
        first.contains("release host"),
        "{PROCEDURE} must state the release-host constraint on its first line, got {first:?}"
    );
}

#[test]
fn entry_docs_route_to_the_current_release_procedure() {
    for relative in ["README.md", "docs/release.md"] {
        let content = read(relative);
        assert!(
            content.contains(PROCEDURE),
            "{relative} must point operators at {PROCEDURE}"
        );
        for superseded in ["docs/release-process.md", "ops/deploy/"] {
            assert!(
                !content.contains(superseded),
                "{relative} still routes to the superseded {superseded}"
            );
        }
    }
    let root = repository_root();
    assert!(!root.join("docs/release-process.md").exists());
    assert!(!root.join("ops/deploy").exists());
}
