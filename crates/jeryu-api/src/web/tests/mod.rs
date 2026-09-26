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

mod auth_tests;
mod control_plane_tests;
mod git_authorization;
mod github_rest_tests;
mod pulls_tests;
mod repo_admin_tests;
mod repo_list_tests;
mod source_tests;
mod surface_tests;
mod tools_tests;
mod work_authorization;
mod work_tests;
mod workcells_tests;
mod ws_tests;

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
