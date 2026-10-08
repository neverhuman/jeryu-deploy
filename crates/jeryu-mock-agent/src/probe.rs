//! `secret-probe`: report whether any canary bytes are visible to the agent,
//! without printing or copying them.

use std::path::Path;
use std::process::ExitCode;

/// Every leak-oracle canary starts with these bytes (lane A leak-oracle-v1).
const CANARY_PREFIX: &[u8] = b"jeryu-canary-";
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Bound the walk so a large tree cannot stall the probe.
const MAX_ENTRIES: usize = 200_000;

pub(crate) fn secret_probe(args: &[String]) -> ExitCode {
    let found = args.iter().any(|arg| contains_canary(arg.as_bytes()))
        || std::env::vars_os().any(|(key, value)| {
            contains_canary(key.as_encoded_bytes()) || contains_canary(value.as_encoded_bytes())
        });
    // Tests narrow the walk so a shared host's /tmp cannot decide the result.
    let roots: Vec<std::path::PathBuf> = match std::env::var_os("JERYU_MOCK_AGENT_PROBE_ROOTS") {
        Some(list) => std::env::split_paths(&list).collect(),
        None => {
            let mut roots = vec![std::path::PathBuf::from(".")];
            roots.extend(std::env::var_os("HOME").map(Into::into));
            roots.extend(["/workspace", "/tmp", "/run", "/etc"].map(Into::into));
            roots
        }
    };
    if std::fs::read_dir(".").is_err() {
        return ExitCode::from(5);
    }
    // Tests shrink the budget to prove an early stop is not `absent`.
    let mut budget = std::env::var("JERYU_MOCK_AGENT_PROBE_BUDGET")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(MAX_ENTRIES);
    let mut verdict = found;
    for root in roots {
        if verdict {
            break;
        }
        match walk(&root, &mut budget) {
            Ok(present) => verdict = present,
            // mock-agent-v1: a walk that stopped early has not searched.
            // Exit 5 and print no verdict.
            Err(Incomplete) => return ExitCode::from(5),
        }
    }
    let verdict = if verdict { "present" } else { "absent" };
    println!("LEAK_ORACLE_PROBE={verdict}");
    ExitCode::SUCCESS
}

/// The walk stopped before it searched everything it could see: the entry
/// budget ran out, or a read failed for a reason other than the entry being
/// unreadable to this user or already gone.
struct Incomplete;

/// An entry this process cannot read, or that vanished during the walk, is
/// not visible to the agent, so skipping it is not an early stop.
fn invisible(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
    )
}

/// Depth-first walk that never follows symlinks and never enters `/proc`.
/// A root that does not exist is skipped.
fn walk(root: &Path, budget: &mut usize) -> Result<bool, Incomplete> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if path.starts_with("/proc") {
            continue;
        }
        if *budget == 0 {
            return Err(Incomplete);
        }
        *budget -= 1;
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if invisible(&error) => continue,
            Err(_) => return Err(Incomplete),
        };
        if meta.is_dir() {
            let entries = match std::fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(error) if invisible(&error) => continue,
                Err(_) => return Err(Incomplete),
            };
            for entry in entries {
                match entry {
                    Ok(entry) => pending.push(entry.path()),
                    Err(error) if invisible(&error) => {}
                    Err(_) => return Err(Incomplete),
                }
            }
        } else if meta.is_file() && meta.len() <= MAX_FILE_BYTES {
            match std::fs::read(&path) {
                Ok(bytes) if contains_canary(&bytes) => return Ok(true),
                Ok(_) => {}
                Err(error) if invisible(&error) => {}
                Err(_) => return Err(Incomplete),
            }
        }
    }
    Ok(false)
}

fn contains_canary(bytes: &[u8]) -> bool {
    bytes
        .windows(CANARY_PREFIX.len())
        .any(|window| window == CANARY_PREFIX)
}
