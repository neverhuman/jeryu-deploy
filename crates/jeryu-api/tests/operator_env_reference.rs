//! `configs/jeryu.env.example` is the reference an operator configures a forge
//! from, so it is checked against the code: every variable the server reads is
//! listed there, and every variable listed there is still read.
//!
//! "The server" is the `jeryu serve` path: all of `crates/jeryu-api/src` plus
//! the two `jeryu-cli` files that start it. Test files are left out. A name is
//! found where the code spells it whole, `"JERYU_..."`, which covers direct
//! reads and the constants that hold names; `"JERYU_X_{}"` spellings are
//! templates and are matched by any example name of that shape.
//!
//! The procedure for operators is `docs/configuration.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Where the server code lives, relative to the repository root.
const SERVER_SOURCES: &[&str] = &[
    "crates/jeryu-api/src",
    "crates/jeryu-cli/src/main.rs",
    "crates/jeryu-cli/src/data_dir.rs",
];

const EXAMPLE: &str = "configs/jeryu.env.example";

/// `JERYU_` names in the server code that are not settings an operator sets,
/// each with the reason.
const NOT_SETTINGS: &[(&str, &str)] = &[
    (
        "JERYU_AGENT_PROMPT",
        "written into an agent run's environment",
    ),
    ("JERYU_BRANCH", "written into a session agent's environment"),
    ("JERYU_CI_RUN_ID", "written into an agent run's environment"),
    (
        "JERYU_CI_USE_SCCACHE",
        "written into a CI job's environment",
    ),
    (
        "JERYU_FAILED_RECEIPT_ID",
        "written into an agent run's environment",
    ),
    (
        "JERYU_FAILED_RUN_ID",
        "written into an agent run's environment",
    ),
    (
        "JERYU_FAILURE_LOG_DIGEST",
        "written into an agent run's environment",
    ),
    (
        "JERYU_RUNNER_EPOCH",
        "written into an agent run's environment",
    ),
    (
        "JERYU_WORKCELL_ID",
        "written into an agent run's environment",
    ),
    ("JERYU_BUILD_COMMIT", "fixed at build time by option_env!"),
    ("JERYU_WEB_COMMIT", "fixed at build time by option_env!"),
    (
        "JERYU_API_URL",
        "read by the jeryu CLI client, never by serve",
    ),
];

/// Settings the example lists that are not spelled `"JERYU_..."`: the process
/// environment, and the git binary the jeryu-gitd crate reads. Each comes with
/// a fragment of server code that shows it is still read.
const OTHER_SETTINGS: &[(&str, &str)] = &[
    ("HOME", "\"HOME\""),
    ("XDG_DATA_HOME", "\"XDG_DATA_HOME\""),
    ("PATH", "\"PATH\""),
    ("TMPDIR", "std::env::temp_dir()"),
    ("GITD_GIT_BIN", "GitdConfig::new("),
];

fn repo_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}

fn is_test_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    name == "tests" || name == "tests.rs" || name.ends_with("_tests.rs")
}

fn collect_sources(path: &Path, out: &mut Vec<PathBuf>) {
    if is_test_path(path) {
        return;
    }
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = fs::read_dir(path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
            .map(|entry| entry.expect("directory entry").path())
            .collect();
        entries.sort();
        for entry in entries {
            collect_sources(&entry, out);
        }
    } else if path.extension().is_some_and(|ext| ext == "rs") {
        out.push(path.to_path_buf());
    }
}

/// The server code, file by file.
fn server_code() -> Vec<(String, String)> {
    let root = repo_root();
    let mut files = Vec::new();
    for source in SERVER_SOURCES {
        let path = root.join(source);
        assert!(path.exists(), "{source} is gone; update SERVER_SOURCES");
        collect_sources(&path, &mut files);
    }
    files
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
            let shown = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
            (shown, text)
        })
        .collect()
}

/// Every whole string literal that starts with `JERYU_`, with the file it was
/// first seen in.
fn jeryu_literals(code: &[(String, String)]) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    for (file, text) in code {
        let mut rest = text.as_str();
        while let Some(start) = rest.find("\"JERYU_") {
            let after = &rest[start + 1..];
            let len = after
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '{' | '}')))
                .unwrap_or(after.len());
            if after[len..].starts_with('"') {
                found
                    .entry(after[..len].to_string())
                    .or_insert_with(|| file.clone());
            }
            rest = &after[len..];
        }
    }
    found
}

/// `JERYU_FLOW_EXPECTED_{}` matches `JERYU_FLOW_EXPECTED_GATE`.
fn template_matches(template: &str, name: &str) -> bool {
    let (Some(open), Some(close)) = (template.find('{'), template.rfind('}')) else {
        return template == name;
    };
    let (prefix, suffix) = (&template[..open], &template[close + 1..]);
    name.len() > prefix.len() + suffix.len()
        && name.starts_with(prefix)
        && name.ends_with(suffix)
        && name[prefix.len()..name.len() - suffix.len()]
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn is_setting_name(text: &str) -> bool {
    text.starts_with(|ch: char| ch.is_ascii_uppercase())
        && text
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

struct ExampleEntry {
    line: usize,
    commented: bool,
}

/// Settings in the example: `NAME=` lines, and `# NAME=` lines for the ones
/// whose empty value is not the same thing to the code.
fn example_entries() -> BTreeMap<String, ExampleEntry> {
    let path = repo_root().join(EXAMPLE);
    let text = fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {EXAMPLE}: {err}"));
    let mut entries = BTreeMap::new();
    let mut problems = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        let (commented, body) = match line.strip_prefix("# ") {
            Some(body) => (true, body),
            None => (false, line),
        };
        let Some((name, value)) = body.split_once('=') else {
            if !commented && !line.is_empty() && !line.starts_with('#') {
                problems.push(format!(
                    "line {}: not a comment or NAME=: {line}",
                    index + 1
                ));
            }
            continue;
        };
        if !is_setting_name(name) {
            if !commented && !line.starts_with('#') {
                problems.push(format!("line {}: not a setting name: {line}", index + 1));
            }
            continue;
        }
        if !value.is_empty() {
            problems.push(format!(
                "line {}: {name} carries a value; the example holds empty placeholders only",
                index + 1
            ));
        }
        let entry = ExampleEntry {
            line: index + 1,
            commented,
        };
        if let Some(previous) = entries.insert(name.to_string(), entry) {
            problems.push(format!(
                "{name} is listed twice (lines {} and {})",
                previous.line,
                index + 1
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{EXAMPLE}:\n  {}",
        problems.join("\n  ")
    );
    entries
}

#[test]
fn the_example_lists_every_variable_the_server_reads_and_no_other() {
    let code = server_code();
    let literals = jeryu_literals(&code);
    let example = example_entries();
    let not_settings: BTreeMap<&str, &str> = NOT_SETTINGS.iter().copied().collect();
    let templates: Vec<&String> = literals.keys().filter(|name| name.contains('{')).collect();

    let mut problems = Vec::new();

    for (name, file) in &literals {
        if name.contains('{') {
            if !example.keys().any(|listed| template_matches(name, listed)) {
                problems.push(format!(
                    "missing: no example line of the shape {name} (read in {file})"
                ));
            }
        } else if not_settings.contains_key(name.as_str()) {
            if example.contains_key(name) {
                problems.push(format!(
                    "{name} is listed, but it is not a setting: {}",
                    not_settings[name.as_str()]
                ));
            }
        } else if !example.contains_key(name) {
            problems.push(format!("missing: {name} (read in {file})"));
        }
    }

    for (name, why) in NOT_SETTINGS {
        if !literals.contains_key(*name) {
            problems.push(format!(
                "stale NOT_SETTINGS entry: {name} ({why}) is no longer in the server code"
            ));
        }
    }

    let all_code: String = code.iter().map(|(_, text)| text.as_str()).collect();
    let other: BTreeSet<&str> = OTHER_SETTINGS.iter().map(|(name, _)| *name).collect();
    for (name, anchor) in OTHER_SETTINGS {
        if !all_code.contains(anchor) {
            problems.push(format!(
                "stale: {name} is listed because the server code holds `{anchor}`, which it no longer does"
            ));
        }
        if !example.contains_key(*name) {
            problems.push(format!("missing: {name} (read through `{anchor}`)"));
        }
    }

    for (name, entry) in &example {
        let read = literals.contains_key(name)
            || other.contains(name.as_str())
            || templates
                .iter()
                .any(|template| template_matches(template, name));
        if !read {
            problems.push(format!(
                "stale: {name} (line {}) is not read by the server code",
                entry.line
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "{EXAMPLE} and the server code disagree:\n  {}\n\
         Add a missing name to the example with what it does, whether it is required, \
         its default and format, or remove a stale one.",
        problems.join("\n  ")
    );
}

/// The settings whose empty value differs from unset in a harmful way stay
/// commented out, so copying the file and leaving them blank is safe.
#[test]
fn settings_whose_empty_value_is_harmful_stay_commented_out() {
    let example = example_entries();
    let must_be_commented = [
        "JERYU_DATA_DIR",
        "JERYU_BOOTSTRAP_ADMIN_PASSWORD",
        "JERYU_BOT_SIGNING_KEY",
        "JERYU_GITHUB_API_TOKEN",
        "JERYU_FLOW_EXPECTED_GATE",
        "HOME",
        "PATH",
        "TMPDIR",
        "GITD_GIT_BIN",
    ];
    let live: Vec<&str> = must_be_commented
        .into_iter()
        .filter(|name| example.get(*name).is_some_and(|entry| !entry.commented))
        .collect();
    assert!(
        live.is_empty(),
        "{EXAMPLE}: an empty value of these differs from unset, keep them `# NAME=`: {live:?}"
    );
}

#[test]
fn template_matching_needs_the_whole_shape() {
    assert!(template_matches(
        "JERYU_FLOW_EXPECTED_{}",
        "JERYU_FLOW_EXPECTED_GATE"
    ));
    assert!(template_matches(
        "JERYU_AGENT_{key}_BIN",
        "JERYU_AGENT_CODEX_BIN"
    ));
    assert!(!template_matches(
        "JERYU_AGENT_{key}_BIN",
        "JERYU_AGENT__BIN"
    ));
    assert!(!template_matches(
        "JERYU_AGENT_{key}_BIN",
        "JERYU_AGENT_RUNTIME"
    ));
    assert!(!template_matches(
        "JERYU_FLOW_EXPECTED_{}",
        "JERYU_FLOW_EXPECTED_"
    ));
}
