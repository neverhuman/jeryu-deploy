//! Rust-native transition tooling for the Jeryu split-family manifest.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;
use toml::Value;

#[derive(Debug, Parser)]
#[command(name = "jeryu-split")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate and render the split-family manifest.
    Manifest {
        #[arg(long, default_value = "repos.manifest.toml")]
        manifest: PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        check_paths: bool,
    },
    /// Prove that every source-tree path is assigned to a split repository.
    SourceCoverage {
        #[arg(long, default_value = "repos.manifest.toml")]
        manifest: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Run the governed score or full check lane for every manifest repository.
    FleetCi {
        #[arg(long, default_value = "repos.manifest.toml")]
        manifest: PathBuf,
        #[arg(long)]
        full: bool,
    },
    /// Validate the split release lock.
    VerifyLock {
        #[arg(long, default_value = "jeryu-split.lock.toml")]
        lock: PathBuf,
    },
    /// Run manifest, source-coverage, and lock checks without Python.
    ProductPipeline {
        #[arg(long, default_value = "repos.manifest.toml")]
        manifest: PathBuf,
        #[arg(long, default_value = "jeryu-split.lock.toml")]
        lock: PathBuf,
    },
}

#[derive(Debug, Serialize)]
struct SourceCoverageReport {
    missing: Vec<String>,
    missing_count: usize,
    patterns: usize,
    schema_version: &'static str,
    source_git_dir: Option<String>,
    source_reader: String,
    source_root: String,
    source_sha: String,
    status: &'static str,
    tracked_files: usize,
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().collect();
    let cli = if args.get(1).and_then(|arg| arg.to_str()) == Some("manifest") {
        match parse_manifest_compat(&args) {
            Ok(cli) => cli,
            Err(error) => {
                eprint!("{}", error.stderr);
                return ExitCode::from(error.exit_code);
            }
        }
    } else {
        Cli::parse_from(args)
    };
    if let Command::Manifest { manifest, .. } = &cli.command
        && fs::File::open(manifest).is_err()
    {
        eprintln!(
            "manifest error: manifest not readable: {}",
            manifest.display()
        );
        return ExitCode::from(1);
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ManifestCliError {
    exit_code: u8,
    stderr: String,
}

fn parse_manifest_compat(args: &[OsString]) -> std::result::Result<Cli, ManifestCliError> {
    let program = env::var_os("JERYU_SPLIT_MANIFEST_PROGRAM")
        .filter(|value| !value.is_empty())
        .or_else(|| args.first().cloned())
        .unwrap_or_else(|| OsString::from("jeryu-split"));
    let program = program.to_string_lossy();
    let program =
        if program.is_empty() || program.len() > 4096 || program.chars().any(char::is_control) {
            "jeryu-split"
        } else {
            &program
        };
    let mut manifest = PathBuf::from("repos.manifest.toml");
    let mut json = false;
    let mut check_paths = false;
    let mut index = 2;
    while index < args.len() {
        match args[index].to_str() {
            Some("--manifest") => {
                index += 1;
                let Some(path) = args.get(index) else {
                    return Err(ManifestCliError {
                        exit_code: 1,
                        stderr: String::new(),
                    });
                };
                manifest = PathBuf::from(path);
            }
            Some("--json") => json = true,
            Some("--check-paths") => check_paths = true,
            _ => {
                return Err(ManifestCliError {
                    exit_code: 2,
                    stderr: format!(
                        "usage: {program} [--manifest PATH] [--json] [--check-paths]\n"
                    ),
                });
            }
        }
        index += 1;
    }
    if manifest.as_os_str().is_empty() {
        return Err(ManifestCliError {
            exit_code: 1,
            stderr: "manifest error: --manifest requires a path\n".to_owned(),
        });
    }
    Ok(Cli {
        command: Command::Manifest {
            manifest,
            json,
            check_paths,
        },
    })
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Manifest {
            manifest,
            json,
            check_paths,
        } => manifest_command(&manifest, json, check_paths),
        Command::SourceCoverage { manifest, json } => source_coverage(&manifest, json),
        Command::FleetCi { manifest, full } => fleet_ci(&manifest, full),
        Command::VerifyLock { lock } => verify_lock(&lock),
        Command::ProductPipeline { manifest, lock } => product_pipeline(&manifest, &lock),
    }
}

fn read_toml(path: &Path) -> Result<Value> {
    let source =
        fs::read_to_string(path).with_context(|| format!("read TOML from {}", path.display()))?;
    toml::from_str(&source).with_context(|| format!("parse TOML from {}", path.display()))
}

fn table<'a>(value: &'a Value, context: &str) -> Result<&'a toml::Table> {
    value
        .as_table()
        .with_context(|| format!("{context} must be a TOML table"))
}

fn required_string<'a>(table: &'a toml::Table, field: &str, context: &str) -> Result<&'a str> {
    let value = table
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if value.is_empty() {
        bail!("{context} missing {field}");
    }
    Ok(value)
}

fn string_array(table: &toml::Table, field: &str, context: &str) -> Result<Vec<String>> {
    let Some(value) = table.get(field) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .with_context(|| format!("{context}.{field} must be an array"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .with_context(|| format!("{context}.{field} entries must be strings"))
        })
        .collect()
}

fn repositories(value: &Value) -> Result<&Vec<Value>> {
    table(value, "manifest")?
        .get("repo")
        .and_then(Value::as_array)
        .filter(|repos| !repos.is_empty())
        .context("manifest must contain [[repo]] entries")
}

fn validate_manifest_value(value: &Value, check_paths: bool) -> Result<()> {
    let root = table(value, "manifest")?;
    let repos = repositories(value)?;
    let mut seen = BTreeSet::new();

    for repo in repos {
        let repo = table(repo, "repo entry")?;
        let name = required_string(repo, "name", "repo entry")?;
        if !seen.insert(name.to_owned()) {
            bail!("duplicate repo name: {name}");
        }
        let path = required_string(repo, "path", name)?;
        for field in [
            "github_slug",
            "jeryu_slug",
            "profile",
            "default_branch",
            "current_tag",
            "required_check",
        ] {
            required_string(repo, field, name)?;
        }
        if required_string(repo, "default_branch", name)? != "main" {
            bail!("{name} default_branch must be main");
        }
        if repo.get("has_jeryu_std").and_then(Value::as_bool) != Some(true) {
            bail!("{name} must set has_jeryu_std=true");
        }
        if check_paths {
            let repo_path = Path::new(path);
            if !repo_path.is_dir() {
                bail!("{name} path missing: {path}");
            }
            for required in ["AGENTS.md", "agent/owner-map.json", "agent/test-map.json"] {
                if !repo_path.join(required).is_file() {
                    bail!("{name} missing {required}");
                }
            }
        }
    }

    let required = string_array(root, "required_repos", "manifest")?;
    let missing: Vec<_> = required
        .into_iter()
        .filter(|name| !seen.contains(name))
        .collect();
    if !missing.is_empty() {
        bail!("manifest missing required repos: {}", missing.join(" "));
    }
    Ok(())
}

fn manifest_command(path: &Path, json: bool, check_paths: bool) -> Result<()> {
    let value = read_toml(path)?;
    validate_manifest_value(&value, check_paths)?;
    let repos = repositories(&value)?;
    if json {
        let output = serde_json::json!({ "repo": repos });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    for repo in repos {
        let repo = table(repo, "repo entry")?;
        println!(
            "{}|{}|{}|{}",
            required_string(repo, "name", "repo entry")?,
            required_string(repo, "path", "repo entry")?,
            required_string(repo, "github_slug", "repo entry")?,
            required_string(repo, "jeryu_slug", "repo entry")?,
        );
    }
    Ok(())
}

fn source_coverage(path: &Path, json: bool) -> Result<()> {
    let value = read_toml(path)?;
    let root = table(&value, "manifest")?;
    let source_root = PathBuf::from(required_string(root, "source_root", "manifest")?);
    let source_sha = required_string(root, "source_sha", "manifest")?.to_owned();
    let source_git_dir = root
        .get("source_git_dir")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let mut patterns = string_array(root, "shared_source_paths", "manifest")?;
    for repo in repositories(&value)? {
        patterns.extend(string_array(
            table(repo, "repo entry")?,
            "source_paths",
            "repo entry",
        )?);
    }
    let (files, source_reader) = git_tree(&source_root, source_git_dir.as_deref(), &source_sha)?;
    let missing: Vec<_> = files
        .iter()
        .filter(|path| !is_covered(path, &patterns))
        .cloned()
        .collect();
    let status = if missing.is_empty() { "pass" } else { "fail" };
    let report = SourceCoverageReport {
        missing_count: missing.len(),
        missing,
        patterns: patterns.len(),
        schema_version: "jeryu.split.source-coverage/v1",
        source_git_dir: source_git_dir
            .as_ref()
            .map(|path| path.display().to_string()),
        source_reader,
        source_root: source_root.display().to_string(),
        source_sha,
        status,
        tracked_files: files.len(),
    };
    if json {
        println!("{}", source_coverage_json(&report)?);
    } else if report.missing.is_empty() {
        println!(
            "source coverage pass: {} tracked files covered by {} patterns",
            report.tracked_files, report.patterns
        );
    } else {
        println!(
            "source coverage failed: {} tracked files are not assigned",
            report.missing_count
        );
        for path in report.missing.iter().take(100) {
            println!("{path}");
        }
    }
    if report.missing.is_empty() {
        Ok(())
    } else {
        bail!("source coverage failed")
    }
}

fn source_coverage_json(report: &SourceCoverageReport) -> Result<String> {
    Ok(serde_json::to_string_pretty(report)?)
}

fn git_tree(
    source_root: &Path,
    source_git_dir: Option<&Path>,
    source_sha: &str,
) -> Result<(Vec<String>, String)> {
    let (mut command, reader) = if source_root.exists() {
        let mut command = ProcessCommand::new("git");
        command.args(["-C", &source_root.display().to_string()]);
        (command, source_root.display().to_string())
    } else if let Some(git_dir) = source_git_dir {
        let mut command = ProcessCommand::new("git");
        command.arg(format!("--git-dir={}", git_dir.display()));
        (command, git_dir.display().to_string())
    } else {
        let mut command = ProcessCommand::new("git");
        command.args(["-C", &source_root.display().to_string()]);
        (command, format!("{} (missing)", source_root.display()))
    };
    let output = command
        .args(["ls-tree", "-r", "--name-only", source_sha])
        .output()
        .with_context(|| format!("read source tree from {reader}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        });
        bail!(
            "failed to read source tree from {reader}: {}",
            detail.trim()
        );
    }
    let stdout = String::from_utf8(output.stdout).context("source tree paths must be UTF-8")?;
    Ok((
        stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect(),
        reader,
    ))
}

fn is_covered(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        path == pattern
            || pattern
                .strip_suffix("/**")
                .is_some_and(|prefix| path.starts_with(&format!("{prefix}/")))
            || wildcard_matches(pattern, path)
    })
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern: Vec<_> = pattern.chars().collect();
    let value: Vec<_> = value.chars().collect();
    let mut memo = vec![vec![None; value.len() + 1]; pattern.len() + 1];
    wildcard_matches_at(&pattern, &value, 0, 0, &mut memo)
}

fn wildcard_matches_at(
    pattern: &[char],
    value: &[char],
    pattern_index: usize,
    value_index: usize,
    memo: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(result) = memo[pattern_index][value_index] {
        return result;
    }
    let result = if pattern_index == pattern.len() {
        value_index == value.len()
    } else {
        match pattern[pattern_index] {
            '*' => {
                wildcard_matches_at(pattern, value, pattern_index + 1, value_index, memo)
                    || (value_index < value.len()
                        && wildcard_matches_at(
                            pattern,
                            value,
                            pattern_index,
                            value_index + 1,
                            memo,
                        ))
            }
            '?' if value_index < value.len() => {
                wildcard_matches_at(pattern, value, pattern_index + 1, value_index + 1, memo)
            }
            '[' if value_index < value.len() => {
                if let Some((next_index, class_matches)) =
                    character_class(pattern, pattern_index, value[value_index])
                {
                    class_matches
                        && wildcard_matches_at(pattern, value, next_index, value_index + 1, memo)
                } else {
                    value[value_index] == '['
                        && wildcard_matches_at(
                            pattern,
                            value,
                            pattern_index + 1,
                            value_index + 1,
                            memo,
                        )
                }
            }
            token if value_index < value.len() && token == value[value_index] => {
                wildcard_matches_at(pattern, value, pattern_index + 1, value_index + 1, memo)
            }
            _ => false,
        }
    };
    memo[pattern_index][value_index] = Some(result);
    result
}

fn character_class(pattern: &[char], open_index: usize, value: char) -> Option<(usize, bool)> {
    let mut index = open_index + 1;
    let negated = pattern.get(index) == Some(&'!');
    if negated {
        index += 1;
    }
    let start = index;
    let mut matched = false;
    while index < pattern.len() && (pattern[index] != ']' || index == start) {
        let first = pattern[index];
        if pattern.get(index + 1) == Some(&'-')
            && pattern.get(index + 2).is_some_and(|token| *token != ']')
        {
            let last = pattern[index + 2];
            matched |= first <= value && value <= last;
            index += 3;
        } else {
            matched |= first == value;
            index += 1;
        }
    }
    (index < pattern.len() && pattern[index] == ']').then_some((index + 1, matched != negated))
}

fn fleet_ci(path: &Path, full: bool) -> Result<()> {
    let value = read_toml(path)?;
    for (name, path, lane) in fleet_entries(&value, full)? {
        println!("{name}: just {lane}");
        io::stdout().flush()?;
        run_process(
            ProcessCommand::new("just").arg(&lane).current_dir(path),
            &name,
        )?;
    }
    Ok(())
}

fn fleet_entries(value: &Value, full: bool) -> Result<Vec<(String, PathBuf, String)>> {
    validate_manifest_value(value, false)?;
    let lane = if full { "check" } else { "score" };
    repositories(value)?
        .iter()
        .map(|repo| {
            let repo = table(repo, "repo entry")?;
            let name = required_string(repo, "name", "repo entry")?.to_owned();
            let path = PathBuf::from(required_string(repo, "path", &name)?);
            Ok((name, path, lane.to_owned()))
        })
        .collect()
}

fn verify_lock(path: &Path) -> Result<()> {
    let value = read_toml(path)?;
    verify_lock_value(&value)?;
    println!("lock ok: {} repos", repositories(&value)?.len());
    Ok(())
}

fn verify_lock_value(value: &Value) -> Result<()> {
    let repos = repositories(value)?;
    let mut failures = Vec::new();
    for repo in repos {
        let repo = table(repo, "lock repo entry")?;
        let name = repo
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("<unknown>");
        for field in [
            "name",
            "github_slug",
            "local_path",
            "tag",
            "commit",
            "required_check",
        ] {
            if repo
                .get(field)
                .and_then(Value::as_str)
                .is_none_or(|value| value.trim().is_empty())
            {
                failures.push(format!("{name} missing {field}"));
            }
        }
        let commit = repo
            .get("commit")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let valid_sha = commit.len() == 40
            && commit
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if commit != "PENDING" && commit != "PENDING_SELF" && !valid_sha {
            failures.push(format!("{name} commit is not a sha: {commit}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(failures.join("\n"))
    }
}

fn product_pipeline(manifest: &Path, lock: &Path) -> Result<()> {
    println!("+ jeryu-split manifest --check-paths");
    manifest_command(manifest, false, true)?;
    println!("+ jeryu-split source-coverage");
    source_coverage(manifest, false)?;
    println!("+ jeryu-split verify-lock");
    verify_lock(lock)?;
    println!("product pipeline bootstrap ok");
    Ok(())
}

fn run_process(command: &mut ProcessCommand, context: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("launch governed command for {context}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("governed command for {context} failed with {status}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(path: &Path, arguments: &[&str]) -> String {
        let output = ProcessCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn coverage_patterns_match_legacy_assignment_shapes() {
        let patterns = vec![
            "crates/core/**".to_owned(),
            "README.md".to_owned(),
            "docs/*.md".to_owned(),
            "tests/fixture?.json".to_owned(),
        ];
        assert!(is_covered("crates/core/src/lib.rs", &patterns));
        assert!(is_covered("README.md", &patterns));
        assert!(is_covered("docs/release.md", &patterns));
        assert!(is_covered("tests/fixture1.json", &patterns));
        assert!(!is_covered("crates/other/src/lib.rs", &patterns));
        assert!(!is_covered("README.md.bak", &patterns));
    }

    #[test]
    fn coverage_patterns_match_python_fnmatch_classes_and_unicode_characters() {
        assert!(wildcard_matches("docs/[a-c].md", "docs/b.md"));
        assert!(!wildcard_matches("docs/[a-c].md", "docs/z.md"));
        assert!(wildcard_matches("docs/[!0-9].md", "docs/x.md"));
        assert!(!wildcard_matches("docs/[!0-9].md", "docs/7.md"));
        assert!(wildcard_matches("docs/?.md", "docs/é.md"));
        assert!(wildcard_matches("docs/[.md", "docs/[.md"));
    }

    #[test]
    fn lock_rejects_missing_and_noncanonical_commits() {
        let valid: Value = toml::from_str(
            r#"
                [[repo]]
                name = "jeryu"
                github_slug = "neverhuman/jeryu"
                local_path = "/tmp/jeryu"
                tag = "jeryu-v5.0.0-split.0"
                commit = "0123456789abcdef0123456789abcdef01234567"
                required_check = "jeryu/required"
            "#,
        )
        .unwrap();
        verify_lock_value(&valid).unwrap();

        let invalid: Value = toml::from_str(
            r#"
                [[repo]]
                name = "jeryu"
                github_slug = "neverhuman/jeryu"
                local_path = "/tmp/jeryu"
                tag = "jeryu-v5.0.0-split.0"
                commit = "ABC"
                required_check = ""
            "#,
        )
        .unwrap();
        let error = verify_lock_value(&invalid).unwrap_err().to_string();
        assert!(error.contains("jeryu missing required_check"));
        assert!(error.contains("jeryu commit is not a sha: ABC"));
    }

    #[test]
    fn manifest_rejects_duplicate_repositories() {
        let duplicate: Value = toml::from_str(
            r#"
                required_repos = ["jeryu", "missing"]
                [[repo]]
                name = "jeryu"
                path = "/tmp/jeryu"
                github_slug = "neverhuman/jeryu"
                jeryu_slug = "jeryu/jeryu"
                profile = "portal"
                default_branch = "main"
                current_tag = "jeryu-v5.0.0-split.0"
                required_check = "jeryu/required"
                has_jeryu_std = true
                [[repo]]
                name = "jeryu"
                path = "/tmp/jeryu-duplicate"
                github_slug = "neverhuman/jeryu"
                jeryu_slug = "jeryu/jeryu"
                profile = "portal"
                default_branch = "main"
                current_tag = "jeryu-v5.0.0-split.0"
                required_check = "jeryu/required"
                has_jeryu_std = true
            "#,
        )
        .unwrap();
        assert!(
            validate_manifest_value(&duplicate, false)
                .unwrap_err()
                .to_string()
                .contains("duplicate repo name")
        );
    }

    #[test]
    fn manifest_path_validation_is_physical_and_required_inventory_is_closed() {
        let temporary = tempfile::tempdir().unwrap();
        let repo = temporary.path().join("jeryu");
        fs::create_dir_all(repo.join("agent")).unwrap();
        for path in ["AGENTS.md", "agent/owner-map.json", "agent/test-map.json"] {
            fs::write(repo.join(path), b"{}\n").unwrap();
        }
        let source = format!(
            r#"
                required_repos = ["jeryu"]
                [[repo]]
                name = "jeryu"
                path = "{}"
                github_slug = "neverhuman/jeryu"
                jeryu_slug = "jeryu/jeryu"
                profile = "portal"
                default_branch = "main"
                current_tag = "jeryu-v5.0.0-split.0"
                required_check = "jeryu/required"
                has_jeryu_std = true
            "#,
            repo.display()
        );
        let manifest: Value = toml::from_str(&source).unwrap();
        validate_manifest_value(&manifest, true).unwrap();

        fs::remove_file(repo.join("agent/test-map.json")).unwrap();
        assert!(
            validate_manifest_value(&manifest, true)
                .unwrap_err()
                .to_string()
                .contains("jeryu missing agent/test-map.json")
        );

        let missing_required: Value = toml::from_str(&source.replace(
            "required_repos = [\"jeryu\"]",
            "required_repos = [\"jeryu\", \"jeryu-core\"]",
        ))
        .unwrap();
        assert!(
            validate_manifest_value(&missing_required, false)
                .unwrap_err()
                .to_string()
                .contains("manifest missing required repos: jeryu-core")
        );
    }

    #[test]
    fn source_tree_reader_uses_the_declared_bare_fallback() {
        let temporary = tempfile::tempdir().unwrap();
        let working = temporary.path().join("working");
        let bare = temporary.path().join("source.git");
        fs::create_dir(&working).unwrap();
        git(&working, &["init", "--quiet"]);
        fs::write(working.join("README.md"), b"source\n").unwrap();
        git(&working, &["add", "README.md"]);
        git(
            &working,
            &[
                "-c",
                "user.name=Jeryu Test",
                "-c",
                "user.email=test@jeryu.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let head = git(&working, &["rev-parse", "HEAD"]);
        let output = ProcessCommand::new("git")
            .args(["clone", "--bare", "--no-local"])
            .arg(&working)
            .arg(&bare)
            .output()
            .unwrap();
        assert!(output.status.success());

        let (files, reader) =
            git_tree(&temporary.path().join("missing-source"), Some(&bare), &head).unwrap();
        assert_eq!(files, ["README.md"]);
        assert_eq!(reader, bare.display().to_string());
    }

    #[test]
    fn source_coverage_fails_when_a_tracked_path_is_unassigned() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir(&source).unwrap();
        git(&source, &["init", "--quiet"]);
        fs::write(source.join("unassigned.txt"), b"unassigned\n").unwrap();
        git(&source, &["add", "unassigned.txt"]);
        git(
            &source,
            &[
                "-c",
                "user.name=Jeryu Test",
                "-c",
                "user.email=test@jeryu.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let head = git(&source, &["rev-parse", "HEAD"]);
        let manifest = temporary.path().join("manifest.toml");
        fs::write(
            &manifest,
            format!(
                "source_root = {:?}\nsource_sha = {:?}\nshared_source_paths = [\"README.md\"]\n\n[[repo]]\nname = \"jeryu\"\npath = {:?}\ngithub_slug = \"neverhuman/jeryu\"\njeryu_slug = \"jeryu/jeryu\"\nprofile = \"portal\"\ndefault_branch = \"main\"\ncurrent_tag = \"jeryu-v5.0.0-split.0\"\nrequired_check = \"jeryu/required\"\nhas_jeryu_std = true\n",
                source.display().to_string(),
                head,
                source.display().to_string(),
            ),
        )
        .unwrap();

        let error = source_coverage(&manifest, true).unwrap_err().to_string();
        assert_eq!(error, "source coverage failed");
    }

    #[test]
    fn source_coverage_json_preserves_sorted_pass_report_bytes() {
        let report = SourceCoverageReport {
            missing: Vec::new(),
            missing_count: 0,
            patterns: 3,
            schema_version: "jeryu.split.source-coverage/v1",
            source_git_dir: None,
            source_reader: "/source".to_owned(),
            source_root: "/source".to_owned(),
            source_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            status: "pass",
            tracked_files: 7,
        };

        assert_eq!(
            source_coverage_json(&report).unwrap(),
            r#"{
  "missing": [],
  "missing_count": 0,
  "patterns": 3,
  "schema_version": "jeryu.split.source-coverage/v1",
  "source_git_dir": null,
  "source_reader": "/source",
  "source_root": "/source",
  "source_sha": "0123456789abcdef0123456789abcdef01234567",
  "status": "pass",
  "tracked_files": 7
}"#
        );
    }

    #[test]
    fn source_coverage_json_preserves_sorted_fail_report_bytes() {
        let report = SourceCoverageReport {
            missing: vec!["src/unassigned.rs".to_owned()],
            missing_count: 1,
            patterns: 2,
            schema_version: "jeryu.split.source-coverage/v1",
            source_git_dir: Some("/source.git".to_owned()),
            source_reader: "/source.git".to_owned(),
            source_root: "/missing-source".to_owned(),
            source_sha: "89abcdef0123456789abcdef0123456789abcdef".to_owned(),
            status: "fail",
            tracked_files: 5,
        };

        assert_eq!(
            source_coverage_json(&report).unwrap(),
            r#"{
  "missing": [
    "src/unassigned.rs"
  ],
  "missing_count": 1,
  "patterns": 2,
  "schema_version": "jeryu.split.source-coverage/v1",
  "source_git_dir": "/source.git",
  "source_reader": "/source.git",
  "source_root": "/missing-source",
  "source_sha": "89abcdef0123456789abcdef0123456789abcdef",
  "status": "fail",
  "tracked_files": 5
}"#
        );
    }

    #[test]
    fn fleet_plan_preserves_manifest_order_and_selects_one_lane() {
        let manifest: Value = toml::from_str(
            r#"
                required_repos = ["first", "second"]
                [[repo]]
                name = "first"
                path = "/tmp/first"
                github_slug = "neverhuman/first"
                jeryu_slug = "jeryu/first"
                profile = "portal"
                default_branch = "main"
                current_tag = "first-v5.0.0-split.0"
                required_check = "first/required"
                has_jeryu_std = true
                [[repo]]
                name = "second"
                path = "/tmp/second"
                github_slug = "neverhuman/second"
                jeryu_slug = "jeryu/second"
                profile = "core"
                default_branch = "main"
                current_tag = "second-v5.0.0-split.0"
                required_check = "second/required"
                has_jeryu_std = true
            "#,
        )
        .unwrap();

        let score = fleet_entries(&manifest, false).unwrap();
        assert_eq!(
            score[0],
            (
                "first".to_owned(),
                PathBuf::from("/tmp/first"),
                "score".to_owned()
            )
        );
        assert_eq!(
            score[1],
            (
                "second".to_owned(),
                PathBuf::from("/tmp/second"),
                "score".to_owned()
            )
        );
        assert!(
            fleet_entries(&manifest, true)
                .unwrap()
                .iter()
                .all(|(_, _, lane)| lane == "check")
        );
    }

    #[test]
    fn governed_process_failure_is_not_swallowed() {
        let error = run_process(ProcessCommand::new("sh").args(["-c", "exit 17"]), "fixture")
            .unwrap_err()
            .to_string();
        assert!(error.contains("governed command for fixture failed"));
        assert!(error.contains("17"));
    }
}
