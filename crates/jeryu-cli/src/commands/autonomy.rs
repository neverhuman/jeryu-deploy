//! Adapter for `jeryu autonomy init`.
//!
//! Emits the canonical `.jeryu/autonomy/policies/*.yml` bundle plus the
//! `.jeryu/ci.toml` and `.jeryu/policy.toml` control files. The policy text
//! mirrors `jeryu_autonomy::policy_yaml::fixtures` (embedded here as string
//! constants so the CLI takes no new crate dependency).
//!
//! Two profiles, both keeping the safety floor intact:
//! - `baseline`: R0-R2 auto-merge, R3-R5 human-required (the canonical bundle).
//! - `full-auto`: R0-R4 auto-merge, R5 fail-closed; the protected-paths
//!   hard-human floor and the R5 fail-closed tier are never relaxed.
//!
//! `ci.toml` uses the shared schema "2": it declares that the repository's CI
//! is the jeryu forge gate and names the one `required` lane with exactly the
//! command the gate runs. The gate runs `just required` when the repository's
//! justfile has a `required` recipe, else `bash ops/ci/pr-ci.sh`; `init`
//! detects the same thing unless `--lane-command` overrides it.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cli::{AutonomyInitArgs, AutonomyProfile};
use crate::client::{ClientError, ClientResult};
use crate::commands::render;

/// One emitted file: its path relative to `--path` and its contents.
#[derive(Debug, Serialize)]
struct EmittedFile {
    path: String,
    contents: String,
}

/// The machine-readable summary emitted under `--json`.
#[derive(Debug, Serialize)]
struct AutonomyInitReport {
    profile: &'static str,
    root: String,
    written: bool,
    files: Vec<EmittedFile>,
}

pub(crate) fn run(json: bool, args: AutonomyInitArgs, out: &mut dyn Write) -> ClientResult<()> {
    let root = PathBuf::from(&args.path);
    let lane_command = args
        .lane_command
        .clone()
        .unwrap_or_else(|| detect_lane_command(&repo_root(&root)));
    let files = bundle(args.profile, &lane_command);

    let written = if args.print {
        false
    } else {
        for file in &files {
            write_file(&root.join(&file.path), &file.contents)?;
        }
        true
    };

    let report = AutonomyInitReport {
        profile: profile_name(args.profile),
        root: root.display().to_string(),
        written,
        files,
    };

    if json {
        return render(out, true, &report, "");
    }

    if args.print {
        for file in &report.files {
            writeln!(out, "# === {} ===", file.path).map_err(ClientError::write_failed)?;
            writeln!(out, "{}", file.contents.trim_end()).map_err(ClientError::write_failed)?;
        }
    } else {
        writeln!(
            out,
            "autonomy init ({}) wrote {} files under {}",
            report.profile,
            report.files.len(),
            report.root
        )
        .map_err(ClientError::write_failed)?;
        for file in &report.files {
            writeln!(out, "  {}", file.path).map_err(ClientError::write_failed)?;
        }
    }
    Ok(())
}

fn profile_name(profile: AutonomyProfile) -> &'static str {
    match profile {
        AutonomyProfile::Baseline => "baseline",
        AutonomyProfile::FullAuto => "full-auto",
    }
}

/// Build the full file set for a profile. The risk policy is the only file that
/// differs between profiles; the safety-floor files (protected-paths, R5) are
/// shared verbatim.
fn bundle(profile: AutonomyProfile, lane_command: &str) -> Vec<EmittedFile> {
    let risk = match profile {
        AutonomyProfile::Baseline => RISK_BASELINE_YML,
        AutonomyProfile::FullAuto => RISK_FULL_AUTO_YML,
    };
    vec![
        EmittedFile {
            path: "autonomy/policies/risk.yml".into(),
            contents: risk.to_string(),
        },
        EmittedFile {
            path: "autonomy/policies/approvals.yml".into(),
            contents: APPROVALS_YML.to_string(),
        },
        EmittedFile {
            path: "autonomy/policies/release.yml".into(),
            contents: RELEASE_YML.to_string(),
        },
        EmittedFile {
            path: "autonomy/policies/protected-paths.yml".into(),
            contents: PROTECTED_PATHS_YML.to_string(),
        },
        EmittedFile {
            path: "autonomy/policies/freeze.yml".into(),
            contents: FREEZE_YML.to_string(),
        },
        EmittedFile {
            path: "ci.toml".into(),
            contents: ci_toml(lane_command),
        },
        EmittedFile {
            path: "policy.toml".into(),
            contents: POLICY_TOML.to_string(),
        },
    ]
}

/// The command the forge gate runs when the repository has a `required` recipe.
const JUST_REQUIRED: &str = "just required";
/// The command the forge gate falls back to without a `required` recipe.
const PR_CI_SCRIPT: &str = "bash ops/ci/pr-ci.sh";

/// The repository root for a `--path`: the parent of a `.jeryu` directory,
/// otherwise the directory itself.
fn repo_root(path: &Path) -> PathBuf {
    if path.file_name().is_some_and(|name| name == ".jeryu") {
        return path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    }
    path.to_path_buf()
}

/// Pick the lane command the way the forge gate does: `just required` when the
/// justfile has a `required` recipe, else `bash ops/ci/pr-ci.sh` when that
/// script exists, else `just required`.
fn detect_lane_command(root: &Path) -> String {
    let has_required = ["justfile", "Justfile", ".justfile", "JUSTFILE"]
        .iter()
        .filter_map(|name| std::fs::read_to_string(root.join(name)).ok())
        .any(|text| justfile_has_required(&text));
    if !has_required && root.join("ops/ci/pr-ci.sh").is_file() {
        return PR_CI_SCRIPT.to_string();
    }
    JUST_REQUIRED.to_string()
}

/// Whether a justfile defines a top-level `required` recipe (with or without
/// parameters, dependencies or attributes).
fn justfile_has_required(text: &str) -> bool {
    text.lines().any(|line| {
        let header = line.strip_prefix('@').unwrap_or(line);
        let Some(after) = header.strip_prefix("required") else {
            return false;
        };
        // `required:` / `required: deps` / `required arg:`; never an
        // assignment (`required := ...`) or a longer name (`required-x:`).
        let params_ok = after.is_empty() || after.starts_with([' ', '\t', ':']);
        params_ok
            && after
                .find(':')
                .is_some_and(|i| !after[i..].starts_with(":="))
    })
}

/// Render `.jeryu/ci.toml` in the shared schema "2".
fn ci_toml(lane_command: &str) -> String {
    let command = toml_basic_string(lane_command);
    format!(
        "schema_version = \"2\"\n\
         provider = \"jeryu\"\n\
         \n\
         [[lane]]\n\
         name = \"required\"\n\
         command = {command}\n"
    )
}

/// Quote a value as a TOML basic string.
fn toml_basic_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_file(path: &Path, contents: &str) -> ClientResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ClientError::Invalid(format!("create {}: {e}", parent.display())))?;
    }
    std::fs::write(path, contents)
        .map_err(|e| ClientError::Invalid(format!("write {}: {e}", path.display())))
}

// ---------------------------------------------------------------------------
// Canonical policy text. Mirrors jeryu_autonomy::policy_yaml::fixtures.
// ---------------------------------------------------------------------------

/// Baseline risk ladder: R0-R2 auto-merge, R3-R5 human-required (R5 fail-closed).
const RISK_BASELINE_YML: &str = "schema: vibegate.risk.v1\n\
tiers:\n\
\x20 - id: R5\n\
\x20   description: \"missing/tampered evidence, suspicious behavior, unknown blast radius\"\n\
\x20   matchers:\n\
\x20     - conditions: [evidence_missing]\n\
\x20     - conditions: [evidence_signature_invalid]\n\
\x20     - conditions: [prompt_injection_suspected]\n\
\x20     - conditions: [policy_sha_drift]\n\
\x20   auto_merge: false\n\
\x20   human_required: true\n\
\x20   fail_closed: true\n\
\x20 - id: R4\n\
\x20   description: \"auth, crypto, secrets, infra, CI, policy, release, prod, prompt/judge rules\"\n\
\x20   matchers:\n\
\x20     - any_path_matches_protected: true\n\
\x20   auto_merge: false\n\
\x20   human_required: true\n\
\x20 - id: R3\n\
\x20   description: \"non-trivial logic, dependency changes, data migrations\"\n\
\x20   matchers:\n\
\x20     - lines_changed_gte: 200\n\
\x20   auto_merge: false\n\
\x20   human_required: true\n\
\x20 - id: R2\n\
\x20   description: \"moderate logic change with tests\"\n\
\x20   matchers:\n\
\x20     - all_files_have_targeted_tests: true\n\
\x20   auto_merge: true\n\
\x20 - id: R1\n\
\x20   description: \"small change with targeted tests\"\n\
\x20   matchers:\n\
\x20     - max_lines_changed: 40\n\
\x20   auto_merge: true\n\
\x20 - id: R0\n\
\x20   description: \"trivial / docs\"\n\
\x20   matchers:\n\
\x20     - default: true\n\
\x20   auto_merge: true\n";

/// Full-auto risk ladder: R0-R4 auto-merge, R5 fail-closed. The R4 tier still
/// matches the protected-paths floor (any_path_matches_protected) so the
/// protected-paths file remains the hard human gate; R5 stays fail-closed.
const RISK_FULL_AUTO_YML: &str = "schema: vibegate.risk.v1\n\
tiers:\n\
\x20 - id: R5\n\
\x20   description: \"missing/tampered evidence, suspicious behavior, unknown blast radius\"\n\
\x20   matchers:\n\
\x20     - conditions: [evidence_missing]\n\
\x20     - conditions: [evidence_signature_invalid]\n\
\x20     - conditions: [prompt_injection_suspected]\n\
\x20     - conditions: [policy_sha_drift]\n\
\x20   auto_merge: false\n\
\x20   human_required: true\n\
\x20   fail_closed: true\n\
\x20 - id: R4\n\
\x20   description: \"auth, crypto, secrets, infra, CI, policy, release, prod, prompt/judge rules\"\n\
\x20   matchers:\n\
\x20     - any_path_matches_protected: true\n\
\x20   auto_merge: true\n\
\x20   human_required: false\n\
\x20 - id: R3\n\
\x20   description: \"non-trivial logic, dependency changes, data migrations\"\n\
\x20   matchers:\n\
\x20     - lines_changed_gte: 200\n\
\x20   auto_merge: true\n\
\x20   human_required: false\n\
\x20 - id: R2\n\
\x20   description: \"moderate logic change with tests\"\n\
\x20   matchers:\n\
\x20     - all_files_have_targeted_tests: true\n\
\x20   auto_merge: true\n\
\x20 - id: R1\n\
\x20   description: \"small change with targeted tests\"\n\
\x20   matchers:\n\
\x20     - max_lines_changed: 40\n\
\x20   auto_merge: true\n\
\x20 - id: R0\n\
\x20   description: \"trivial / docs\"\n\
\x20   matchers:\n\
\x20     - default: true\n\
\x20   auto_merge: true\n";

const APPROVALS_YML: &str = "schema: vibegate.approvals.v1\n\
invariants:\n\
\x20 no_self_approval: true\n\
\x20 exact_sha_required: true\n\
\x20 target_branch_policy_only: true\n\
\x20 fail_closed_on_missing_evidence: true\n\
\x20 fail_closed_on_agent_disagreement: true\n\
\x20 require_distinct_agent_identities: true\n\
hard_stops:\n\
\x20 - name: secret_scan_failed\n\
\x20 - name: sast_failed\n\
\x20 - name: reviewer_blocked\n\
\x20 - name: sha_drift\n\
\x20 - name: policy_sha_drift\n\
\x20 - name: missing_required_review_role\n\
\x20 - name: missing_evidence_pack\n\
\x20 - name: evidence_signature_invalid\n\
\x20 - name: prompt_injection_suspected\n\
\x20 - name: codeowners_not_satisfied\n\
\x20 - name: freeze_window_active\n\
\x20 - name: budget_exceeded\n\
\x20 - name: training_use_required_but_disallowed\n\
\x20 - name: lockfile_diff_without_manifest_diff\n\
\x20 - name: judge_signature_invalid\n\
quorum:\n\
\x20 R0: { approvals_needed: 0, roles: [], human_required: false }\n\
\x20 R1: { approvals_needed: 1, roles: [test_integrity], human_required: false }\n\
\x20 R2: { approvals_needed: 2, roles: [test_integrity, security], human_required: false }\n\
\x20 R3:\n\
\x20   approvals_needed: 4\n\
\x20   roles: [test_integrity, security, runtime, lockfile]\n\
\x20   human_required: true\n\
\x20 R4: { approvals_needed: 0, roles: [], human_required: true, fail_closed_without_human: true }\n\
\x20 R5: { approvals_needed: 0, roles: [], human_required: true, fail_closed: true }\n\
verdict_ttl_minutes: 60\n\
re_judge_on:\n\
\x20 - merge_train_rebase\n\
\x20 - target_branch_advance\n\
\x20 - policy_change_on_target\n\
\x20 - new_commit_on_pr\n";

const RELEASE_YML: &str = "schema: vibegate.release.v1\n\
build:\n\
\x20 build_once: true\n\
\x20 require_sbom: true\n\
\x20 require_slsa_provenance: true\n\
\x20 require_artifact_signature: true\n\
\x20 require_rollback_plan: true\n\
release_ready_receipts:\n\
\x20 - build\n\
\x20 - sbom\n\
\x20 - provenance\n";

const PROTECTED_PATHS_YML: &str = "schema: vibegate.protected_paths.v1\n\
hard_human:\n\
\x20 - \"src/auth/**\"\n\
\x20 - \"src/crypto/**\"\n\
\x20 - \"secrets/**\"\n\
\x20 - \".jeryu/autonomy/**\"\n\
semantic_triggers:\n\
\x20 - touches_secret_handling\n\
\x20 - changes_security_scanner_config\n";

const FREEZE_YML: &str = "schema: vibegate.freeze.v1\n\
enabled: false\n\
windows: []\n";

const POLICY_TOML: &str = "require_admission_receipt = false\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_keeps_r3_r4_human_required() {
        let risk = bundle(AutonomyProfile::Baseline, JUST_REQUIRED)
            .into_iter()
            .find(|f| f.path.ends_with("risk.yml"))
            .unwrap()
            .contents;
        // R3/R4 are human-required in the canonical baseline.
        assert!(risk.contains("id: R3"));
        assert!(risk.contains("id: R4"));
        assert_eq!(risk.matches("human_required: true").count(), 3, "R3,R4,R5");
        assert_eq!(risk.matches("auto_merge: true").count(), 3, "R0,R1,R2");
    }

    #[test]
    fn full_auto_lifts_r3_r4_but_keeps_r5_fail_closed_and_floor() {
        let files = bundle(AutonomyProfile::FullAuto, JUST_REQUIRED);
        let risk = files
            .iter()
            .find(|f| f.path.ends_with("risk.yml"))
            .unwrap()
            .contents
            .clone();
        // R0-R4 auto-merge (5 tiers), only R5 human-required and fail-closed.
        assert_eq!(risk.matches("auto_merge: true").count(), 5, "R0..R4");
        assert!(risk.contains("fail_closed: true"), "R5 fail-closed intact");
        assert_eq!(
            risk.matches("human_required: true").count(),
            1,
            "only R5 stays human-required"
        );
        // Safety floor file is identical regardless of profile.
        let protected = files
            .iter()
            .find(|f| f.path.ends_with("protected-paths.yml"))
            .unwrap();
        assert_eq!(protected.contents, PROTECTED_PATHS_YML);
        assert!(protected.contents.contains(".jeryu/autonomy/**"));
    }

    #[test]
    fn control_files_encode_required_keys() {
        let files = bundle(AutonomyProfile::FullAuto, JUST_REQUIRED);
        let ci = &files.iter().find(|f| f.path == "ci.toml").unwrap().contents;
        assert_eq!(
            ci,
            "schema_version = \"2\"\nprovider = \"jeryu\"\n\n[[lane]]\nname = \"required\"\ncommand = \"just required\"\n"
        );
        assert!(!ci.contains("github_actions_required"), "retired key");
        assert!(!ci.contains("runs"), "runs is omitted by default");
        let policy = &files
            .iter()
            .find(|f| f.path == "policy.toml")
            .unwrap()
            .contents;
        assert!(policy.contains("require_admission_receipt = false"));
    }

    #[test]
    fn bundle_emits_full_canonical_file_set() {
        let files = bundle(AutonomyProfile::FullAuto, JUST_REQUIRED);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        for required in [
            "autonomy/policies/risk.yml",
            "autonomy/policies/approvals.yml",
            "autonomy/policies/release.yml",
            "autonomy/policies/protected-paths.yml",
            "autonomy/policies/freeze.yml",
            "ci.toml",
            "policy.toml",
        ] {
            assert!(paths.contains(&required), "missing {required} in {paths:?}");
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jeryu-autonomy-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn justfile_required_recipe_detection() {
        assert!(justfile_has_required("required:\n    cargo test\n"));
        assert!(justfile_has_required(
            "set shell := [\"bash\"]\n\nrequired: fmt lint\n"
        ));
        assert!(justfile_has_required("@required *args:\n    echo\n"));
        assert!(!justfile_has_required("required := \"x\"\n"));
        assert!(!justfile_has_required("required-fast:\n    echo\n"));
        assert!(!justfile_has_required("    required:\n"));
        assert!(!justfile_has_required("check:\n    just required\n"));
    }

    #[test]
    fn lane_command_follows_the_gate_choice() {
        let dir = scratch("detect");
        // Nothing at all: default to `just required`.
        assert_eq!(detect_lane_command(&dir), JUST_REQUIRED);
        // Only the pr-ci script: the gate's fallback.
        std::fs::create_dir_all(dir.join("ops/ci")).unwrap();
        std::fs::write(dir.join("ops/ci/pr-ci.sh"), "#!/bin/sh\n").unwrap();
        assert_eq!(detect_lane_command(&dir), PR_CI_SCRIPT);
        // A justfile without `required` still falls back to the script.
        std::fs::write(dir.join("justfile"), "test:\n    true\n").unwrap();
        assert_eq!(detect_lane_command(&dir), PR_CI_SCRIPT);
        // A `required` recipe wins.
        std::fs::write(dir.join("justfile"), "required:\n    true\n").unwrap();
        assert_eq!(detect_lane_command(&dir), JUST_REQUIRED);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn repo_root_is_parent_of_dot_jeryu() {
        assert_eq!(repo_root(Path::new("/r/.jeryu")), PathBuf::from("/r"));
        assert_eq!(repo_root(Path::new(".jeryu")), PathBuf::from("."));
        assert_eq!(repo_root(Path::new("/r")), PathBuf::from("/r"));
    }

    #[test]
    fn ci_toml_quotes_the_command() {
        let ci = ci_toml("bash ops/ci/pr-ci.sh --lane \"a\"");
        assert!(ci.contains("command = \"bash ops/ci/pr-ci.sh --lane \\\"a\\\"\"\n"));
    }
}
