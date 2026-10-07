//! Rust-native split-family tooling over the authority manifest.
//!
//! The family's membership and release identity live in one file owned by
//! `jeryu-release-ops`; [`family`] locates and parses it. This binary never
//! carries a copy of it.

mod family;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use toml::Value;

use family::Family;

#[derive(Debug, Parser)]
#[command(name = "jeryu-split")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate and render the family authority manifest.
    Manifest {
        /// Authority manifest; defaults to the located one.
        #[arg(long)]
        manifest: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        check_paths: bool,
    },
    /// Print where the family authority manifest was found.
    ManifestPath,
    /// Run the governed score or full check lane for every family member.
    FleetCi {
        /// Authority manifest; defaults to the located one.
        #[arg(long)]
        manifest: Option<PathBuf>,
        #[arg(long)]
        full: bool,
    },
    /// Validate the split release lock.
    VerifyLock {
        #[arg(long, default_value = "jeryu-split.lock.toml")]
        lock: PathBuf,
    },
    /// Run the authority-manifest and lock checks without Python.
    ProductPipeline {
        /// Authority manifest; defaults to the located one.
        #[arg(long)]
        manifest: Option<PathBuf>,
        #[arg(long, default_value = "jeryu-split.lock.toml")]
        lock: PathBuf,
    },
    /// Verify hosted workflows remain thin wrappers around agent/ci-lanes.toml.
    CiLanesCheck,
    /// List commands declared by agent/ci-lanes.toml.
    CiLanesList {
        /// Emit only lanes that participate in the full workflow union.
        #[arg(long)]
        full: bool,
        /// Emit JSON instead of tab-separated lane/command rows.
        #[arg(long)]
        json: bool,
    },
    /// Build the affected-package plan for a committed change range.
    AffectedPlan {
        /// Protected base ref used for the three-dot diff.
        #[arg(long, default_value = "origin/main")]
        base: String,
        /// Output path relative to the repository root.
        #[arg(long, default_value = "target/ci-fast/affected-plan.json")]
        out: PathBuf,
        /// Worker count recorded in the plan.
        #[arg(long, default_value_t = 40)]
        workers: u32,
    },
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
    if let Command::Manifest {
        manifest: Some(manifest),
        ..
    } = &cli.command
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
    let mut manifest: Option<PathBuf> = None;
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
                manifest = Some(PathBuf::from(path));
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
    if manifest
        .as_ref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
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
        } => manifest_command(manifest.as_deref(), json, check_paths),
        Command::ManifestPath => manifest_path(),
        Command::FleetCi { manifest, full } => fleet_ci(manifest.as_deref(), full),
        Command::VerifyLock { lock } => verify_lock(&lock),
        Command::ProductPipeline { manifest, lock } => product_pipeline(manifest.as_deref(), &lock),
        Command::CiLanesCheck => {
            emit_repo_gate(jeryu_repogate::run_ci_lanes_check(Path::new("."))?)
        }
        Command::CiLanesList { full, json } => emit_repo_gate(jeryu_repogate::run_ci_lanes_list(
            Path::new("."),
            full,
            json,
        )?),
        Command::AffectedPlan { base, out, workers } => emit_repo_gate(
            jeryu_repogate::run_affected_plan(Path::new("."), &base, &out, workers)?,
        ),
    }
}

fn emit_repo_gate(outcome: jeryu_repogate::GateOutcome) -> Result<()> {
    for line in outcome.stdout {
        println!("{line}");
    }
    if outcome.exit_code != 0 {
        bail!("repository gate exited {}", outcome.exit_code);
    }
    Ok(())
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

/// The `[[repo]]` entries of a release lock.
fn repositories(value: &Value) -> Result<&Vec<Value>> {
    table(value, "lock")?
        .get("repo")
        .and_then(Value::as_array)
        .filter(|repos| !repos.is_empty())
        .context("lock must contain [[repo]] entries")
}

/// The authority manifest an explicit `--manifest` names, else the located one.
fn load_family(manifest: Option<&Path>) -> Result<Family> {
    match manifest {
        Some(path) => family::read(path),
        None => family::read(&family::locate(Path::new("."))?),
    }
}

fn manifest_path() -> Result<()> {
    println!("{}", family::locate(Path::new("."))?.display());
    Ok(())
}

fn manifest_command(manifest: Option<&Path>, json: bool, check_paths: bool) -> Result<()> {
    let family = load_family(manifest)?;
    if check_paths {
        family.check_paths()?;
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&family)?);
        return Ok(());
    }
    for member in &family.members {
        println!(
            "{}|{}|{}|{}|{}",
            member.name,
            member.path.display(),
            member.jeryu_slug,
            member.required_check,
            member.tag.as_deref().unwrap_or("pending"),
        );
    }
    Ok(())
}

fn fleet_ci(manifest: Option<&Path>, full: bool) -> Result<()> {
    let family = load_family(manifest)?;
    for (name, path, lane) in fleet_entries(&family, full) {
        println!("{name}: just {lane}");
        io::stdout().flush()?;
        run_process(
            ProcessCommand::new("just").arg(&lane).current_dir(path),
            &name,
        )?;
    }
    Ok(())
}

fn fleet_entries(family: &Family, full: bool) -> Vec<(String, PathBuf, String)> {
    let lane = if full { "check" } else { "score" };
    family
        .members
        .iter()
        .map(|member| (member.name.clone(), member.path.clone(), lane.to_owned()))
        .collect()
}

fn verify_lock(path: &Path) -> Result<()> {
    let value = read_toml(path)?;
    verify_lock_value(&value)?;
    println!("lock ok: {} repos", repositories(&value)?.len());
    Ok(())
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The one commit value that is not a commit id: the row for the repository
/// the lock itself lives in, whose own commit id cannot be known until the
/// lock is committed. Every other member is a full 40-hex lowercase id.
const SELF_PIN: &str = "PENDING_SELF";

/// Validate a release lock: it must list at least one `[[repo]]`, and every
/// member must be pinned to a full 40-hex lowercase commit id. A branch, a
/// tag, `latest`, an empty value, a short id and an uppercase id are all
/// refused; only the lock's own repository may carry [`SELF_PIN`], once.
fn verify_lock_value(value: &Value) -> Result<()> {
    let repos = repositories(value)?;
    let mut failures = Vec::new();
    let mut self_pinned = 0usize;
    // jeryu-web ships as a dist pinned by commit and content hash (see
    // crates/jeryu-api/build.rs), so its entry carries no tag.
    let web_pinned = match value.get("web_artifact").and_then(Value::as_str) {
        None => false,
        Some("pinned") => true,
        Some(other) => {
            failures.push(format!("web_artifact must be \"pinned\", found {other}"));
            false
        }
    };
    for repo in repos {
        let repo = table(repo, "lock repo entry")?;
        let name = repo
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("<unknown>");
        let web = web_pinned && name == "jeryu-web";
        for field in [
            "name",
            "github_slug",
            "local_path",
            "tag",
            "commit",
            "required_check",
        ] {
            if web && field == "tag" {
                continue;
            }
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
        let valid_sha = is_lower_hex(commit, 40);
        if web {
            if !valid_sha {
                failures.push(format!("{name} commit must be a full 40-hex sha: {commit}"));
            }
            let dist = repo
                .get("web_dist_sha256")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !is_lower_hex(dist, 64) {
                failures.push(format!("{name} web_dist_sha256 is not a sha256: {dist}"));
            }
        } else if commit == SELF_PIN {
            self_pinned += 1;
            if self_pinned > 1 {
                failures.push(format!(
                    "{name} is a second {SELF_PIN} entry: only the lock's own repository may be \
                     unpinned"
                ));
            }
        } else if !valid_sha {
            failures.push(format!("{name} commit is not a sha: {commit}"));
        }
    }
    if web_pinned
        && !repos
            .iter()
            .any(|repo| repo.get("name").and_then(Value::as_str) == Some("jeryu-web"))
    {
        failures.push("web_artifact is \"pinned\" but the lock has no jeryu-web entry".to_string());
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(failures.join("\n"))
    }
}

fn product_pipeline(manifest: Option<&Path>, lock: &Path) -> Result<()> {
    println!("+ jeryu-split manifest --check-paths");
    manifest_command(manifest, false, true)?;
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
mod tests;
