//! Route-level coverage for the web router, one submodule per subject.
//! Shared fixtures live here; each submodule pulls them in with `use super::*`.

use super::*;
use crate::Method;
use crate::web::markdown::render_markdown;
use crate::web::repositories::repo_list_response;
use crate::web::surface::serialize_payload;
use crate::web::surface::{bootstrap_payload, map_method};
use crate::web::ws::{hello_message, requested_scopes, snapshot_event, unsubscribe_scopes};
use axum::extract::{Extension, Query};
use jeryu_agentbridge::driver::{AgentDriver, CollectingSink, CommandSpec, stage_editbot};
use jeryu_codegraph::{
    CrateDepRow, GraphSnapshot, SymbolRefRow, SymbolRow, ToolBuildScanConfig,
    scan_tool_build_clusters,
};
use jeryu_core::CheckConclusion;
use jeryu_core::{
    AccountStatus, AccountSummary, CommitStatusState, CreateCheckRunRequest,
    CreateCommitStatusRequest, CreatePullRequestRequest, CreateRepositoryRequest,
    CreateReviewRequest, RepoAccessLevel, ReviewState, SetBranchProtectionRequest, UserRole,
};
use jeryu_readmodel::contracts::{RepositoryRole, ServerWsMessage};
use jeryu_readmodel::{HealthLevel, sample_read_model};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::tempdir;

mod account_email_tests;
mod admin_reset_tests;
mod agent_presence_tests;
mod agent_run_access_tests;
mod auth_tests;
mod bot_setup_script;
mod bots_owner_reach_tests;
mod bots_tests;
mod branch_protection_tests;
mod canonical_links_tests;
mod capability_discovery_tests;
mod control_plane_tests;
mod discovery_tests;
mod event_trust_tests;
mod evidence_independence_tests;
mod family_key_tests;
mod flow_tests;
mod gate_capabilities_tests;
mod gate_wanted_tests;
mod gh_pr_view_tests;
mod git_authorization;
mod github_rest_tests;
mod human_time_tests;
mod jankurai_audit_tests;
mod pull_import_tests;
mod pull_origin_tests;
mod pull_replacement_tests;
mod pull_workflow_status_tests;
mod pulls_tests;
mod raw_path_tests;
mod regate_tests;
mod release_actions_tests;
mod release_board_tests;
mod repo_admin_tests;
mod repo_automation_tests;
mod repo_list_tests;
mod review_activity_tests;
mod review_catch_tests;
mod review_resolution_tests;
mod review_wanted_tests;
mod signup_tests;
mod site_settings_tests;
mod source_tests;
mod surface_tests;
mod team_bots_tests;
mod test_fixture_tests;
mod tools_tests;
mod waitlist_forward_tests;
mod waitlist_mail_tests;
mod waitlist_tests;
mod work_authorization;
mod work_tests;
mod workcells_tests;
mod ws_tests;

fn bare_ref(storage_root: &std::path::Path, owner: &str, repo: &str, ref_name: &str) -> String {
    let bare = storage_root.join(owner).join(format!("{repo}.git"));
    let output = crate::test_git::git_command()
        .args(["rev-parse", ref_name])
        .current_dir(&bare)
        .output()
        .unwrap_or_else(|e| panic!("git rev-parse {ref_name} failed to spawn: {e}"));
    assert!(
        output.status.success(),
        "git rev-parse {ref_name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn git_in(cwd: &std::path::Path, args: &[&str]) -> String {
    let output = crate::test_git::git_command()
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "jeryu-test")
        .env("GIT_AUTHOR_EMAIL", "jeryu-test@example.com")
        .env("GIT_COMMITTER_NAME", "jeryu-test")
        .env("GIT_COMMITTER_EMAIL", "jeryu-test@example.com")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn build_bare_repo_with_main_and_feature(
    storage_root: &std::path::Path,
    owner: &str,
    repo: &str,
    feature_path: &str,
    feature_contents: &str,
) -> (String, String) {
    let bare = storage_root.join(owner).join(format!("{repo}.git"));
    std::fs::create_dir_all(bare.parent().expect("bare parent")).expect("create owner dir");
    let work = storage_root.join(format!("{owner}-{repo}-merge-work"));
    std::fs::create_dir_all(&work).expect("create work dir");

    let git = |args: &[&str], cwd: &std::path::Path| git_in(cwd, args);

    git(&["init", "--quiet", "-b", "main"], &work);
    std::fs::write(work.join("BASE.txt"), "base\n").expect("write base file");
    git(&["add", "BASE.txt"], &work);
    git(&["commit", "--quiet", "-m", "base"], &work);
    let base_sha = git(&["rev-parse", "HEAD"], &work);

    git(&["checkout", "--quiet", "-b", "feature"], &work);
    let path = work.join(feature_path);
    std::fs::create_dir_all(path.parent().expect("feature file parent"))
        .expect("create feature file dir");
    std::fs::write(&path, feature_contents).expect("write feature file");
    git(&["add", feature_path], &work);
    git(&["commit", "--quiet", "-s", "-m", "feature"], &work);
    let head_sha = git(&["rev-parse", "HEAD"], &work);
    git(
        &[
            "clone",
            "--quiet",
            "--bare",
            ".",
            bare.to_str().expect("bare utf8"),
        ],
        &work,
    );
    git(&["symbolic-ref", "HEAD", "refs/heads/main"], bare.as_path());

    (base_sha, head_sha)
}

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create fixture parent");
    }
    std::fs::write(path, contents).expect("write fixture file");
}

fn authenticated_account(login: &str) -> Extension<AccountSummary> {
    Extension(AccountSummary {
        login: login.to_string(),
        display_name: login.to_string(),
        role: UserRole::User,
        status: AccountStatus::Active,
        auth_epoch: 0,
        must_change_password: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    })
}

fn authenticated_admin_account(login: &str) -> Extension<AccountSummary> {
    Extension(AccountSummary {
        login: login.to_string(),
        display_name: login.to_string(),
        role: UserRole::Admin,
        status: AccountStatus::Active,
        auth_epoch: 0,
        must_change_password: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    })
}

async fn response_json(response: AxumResponse) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body reads");
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("response body is not JSON ({err}): {bytes:?}"))
}
