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
    let mut found = args.iter().any(|arg| contains_canary(arg.as_bytes()))
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
    let mut budget = MAX_ENTRIES;
    for root in roots {
        if found {
            break;
        }
        found = walk(&root, &mut budget);
    }
    let verdict = if found { "present" } else { "absent" };
    println!("LEAK_ORACLE_PROBE={verdict}");
    ExitCode::SUCCESS
}

/// Depth-first walk that never follows symlinks and never enters `/proc`.
fn walk(root: &Path, budget: &mut usize) -> bool {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if path.starts_with("/proc") || *budget == 0 {
            continue;
        }
        *budget -= 1;
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                pending.extend(entries.flatten().map(|entry| entry.path()));
            }
        } else if meta.is_file()
            && meta.len() <= MAX_FILE_BYTES
            && std::fs::read(&path).is_ok_and(|bytes| contains_canary(&bytes))
        {
            return true;
        }
    }
    false
}

fn contains_canary(bytes: &[u8]) -> bool {
    bytes
        .windows(CANARY_PREFIX.len())
        .any(|window| window == CANARY_PREFIX)
}
