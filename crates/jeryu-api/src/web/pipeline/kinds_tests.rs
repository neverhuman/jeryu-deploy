//! The forge's emit contract is closed on both sides: every kind
//! `docs/pipeline-events.md` lists under "Emitted by the forge" has a live
//! emit point in the sources, and every forge emit point in the sources is in
//! the documented set.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::kinds::FORGE_KINDS;

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn docs() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/pipeline-events.md"),
    )
    .unwrap()
}

/// The kinds the first column of the "Emitted by the forge" table names.
fn documented_forge_kinds(docs: &str) -> BTreeSet<String> {
    let table = docs
        .split_once("Emitted by the forge")
        .expect("docs/pipeline-events.md: no forge table")
        .1
        .split_once("Posted by producers:")
        .expect("docs/pipeline-events.md: forge table does not end")
        .0;
    let mut kinds = BTreeSet::new();
    for row in table.lines().filter(|line| line.starts_with("| `")) {
        let first_cell = row.trim_start_matches('|').split('|').next().unwrap();
        for literal in first_cell.split('`').skip(1).step_by(2) {
            kinds.insert(literal.to_string());
        }
    }
    assert!(!kinds.is_empty(), "no kinds read from the forge table");
    kinds
}

#[test]
fn the_registry_is_sorted_and_unique() {
    let kinds: Vec<_> = FORGE_KINDS.iter().map(|entry| entry.kind).collect();
    let mut sorted = kinds.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(kinds, sorted, "FORGE_KINDS must stay sorted and unique");
}

/// Every registered kind still has the emit point it names: a kind whose
/// emitter is deleted or renamed fails here instead of going quiet in the
/// live event log.
#[test]
fn every_forge_kind_has_an_emit_point() {
    for entry in FORGE_KINDS {
        let path = src().join(entry.source);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert!(
            source.contains(entry.needle),
            "{}: no emit point for {} ({} is not in {})",
            entry.kind,
            entry.kind,
            entry.needle,
            entry.source
        );
    }
}

/// The documented set and the registry are the same set.
#[test]
fn the_docs_and_the_registry_name_the_same_kinds() {
    let registered: BTreeSet<String> = FORGE_KINDS
        .iter()
        .map(|entry| entry.kind.to_string())
        .collect();
    let documented = documented_forge_kinds(&docs());
    assert_eq!(
        documented, registered,
        "docs/pipeline-events.md and FORGE_KINDS disagree"
    );
}

/// No forge emit point outside the registry: a new `NewEvent::forge("x.y")`
/// has to be documented and registered. Kinds built from a variable (the
/// merge queue's transitions, the heartbeat's `gate.*`/`review.*`) are not
/// written out here, which is what the registry's `needle` covers.
#[test]
fn every_literal_forge_emit_point_is_registered() {
    let registered: BTreeSet<&str> = FORGE_KINDS.iter().map(|entry| entry.kind).collect();
    let mut unregistered = BTreeSet::new();
    for file in web_sources() {
        let source = std::fs::read_to_string(&file).unwrap();
        for (index, _) in source.match_indices("NewEvent::forge(") {
            let rest = &source[index + "NewEvent::forge(".len()..];
            let Some(rest) = rest.strip_prefix('"') else {
                continue;
            };
            let kind = &rest[..rest.find('"').expect("unterminated kind literal")];
            if !registered.contains(kind) {
                unregistered.insert(format!("{kind} ({})", file.display()));
            }
        }
    }
    assert!(
        unregistered.is_empty(),
        "forge kinds missing from FORGE_KINDS: {unregistered:?}"
    );
}

/// Every non-test source under `web/`, plus `web.rs`.
fn web_sources() -> Vec<PathBuf> {
    let root = src();
    let mut files = vec![root.join("web.rs")];
    let mut dirs = vec![root.join("web")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "tests") {
                    dirs.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && !path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with("tests.rs")
            {
                files.push(path);
            }
        }
    }
    files
}
