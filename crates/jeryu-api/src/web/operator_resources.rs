//! Read routes for the operator pages that had no API of their own:
//! `GET /api/v1/releases`, `/api/v1/mirrors`, `/api/v1/settings` and
//! `/api/v1/audit`.
//!
//! Each is shaped like `GET /api/v1/repos`: one row per repository the viewer
//! can read (admins see all), an optional `?repo=` filter taking an id or
//! `owner/name`, `limit`/`per_page`/`page` paging, and a top-level `total`
//! plus the applied `page`. The audit trail names privileged mutations across
//! every repository, so like the event log it is for global admins only
//! (`auth::admin_only_request`).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Query, State};
use jeryu_core::{AccountSummary, Repository, UserRole};
use jeryu_readmodel::contracts::{
    RepositoryId, RepositoryMirrorStatus, RepositoryVisibility, WebFeatureFlags,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::WebState;
use super::paging::{PageInfo, PageParams, PageRejection};
use super::permissions::{feature_flags, permissions};
use super::repositories::{TagLookupMiss, mirror_status, nearest_release_tag, repo_id};

/// Query parameters shared by the four routes.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ResourceQuery {
    /// Repository id or `owner/name`; absent means every visible repository.
    pub(super) repo: Option<String>,
    #[serde(flatten)]
    pub(super) paging: PageParams,
}

/// Repositories the viewer may read, narrowed by `?repo=`, in registry order.
fn visible_repos(
    state: &WebState,
    account: &AccountSummary,
    repo: Option<&str>,
) -> Vec<Repository> {
    let core = state.github.core();
    let repo = repo.map(str::trim).filter(|repo| !repo.is_empty());
    core.list_repositories(None)
        .into_iter()
        .filter(|candidate| {
            repo.is_none_or(|wanted| {
                candidate.id.to_string() == wanted || candidate.full_name == wanted
            })
        })
        .filter(|candidate| {
            account.role == UserRole::Admin
                || core.user_can_read_repo(&account.login, &candidate.owner, &candidate.name)
        })
        .collect()
}

/// The newest tag reachable from a repository's default branch. Releases
/// are cut as tags, so this is the repository's current release.
#[derive(Debug, Serialize)]
pub(super) struct RepositoryRelease {
    pub(super) repo: RepositoryId,
    pub(super) branch: String,
    /// `false` for a metadata-only import with no bare storage.
    pub(super) has_git_data: bool,
    /// `null` when no tag is reachable: nothing has been released.
    pub(super) tag: Option<String>,
    pub(super) sha: Option<String>,
    pub(super) tagged_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct ReleasePage {
    pub(super) releases: Vec<RepositoryRelease>,
    pub(super) total: usize,
    pub(super) page: PageInfo,
}

/// `GET /api/v1/releases[?repo=]`: the current release tag per repository.
pub(super) async fn releases(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<ResourceQuery>,
) -> Result<Json<ReleasePage>, PageRejection> {
    let page = query.paging.page()?;
    let repos = visible_repos(&state, &account, query.repo.as_deref());
    let total = repos.len();
    // Cut the page before shelling out to git: each row costs a describe.
    let (repos, page) = page.apply(repos);
    let releases = repos
        .iter()
        .map(|repo| {
            let lookup = nearest_release_tag(&state, repo, &repo.default_branch);
            let has_git_data = lookup != Err(TagLookupMiss::NoGitData);
            let released = lookup.ok().flatten();
            RepositoryRelease {
                repo: repo_id(repo),
                branch: repo.default_branch.clone(),
                has_git_data,
                tag: released.as_ref().map(|released| released.tag.clone()),
                sha: released.as_ref().and_then(|released| released.sha.clone()),
                tagged_at: released.and_then(|released| released.tagged_at),
            }
        })
        .collect();
    Ok(Json(ReleasePage {
        releases,
        total,
        page,
    }))
}

/// A repository's offsite push-mirror posture: the same value the repository
/// summary carries in its `mirror` field.
#[derive(Debug, Serialize)]
pub(super) struct RepositoryMirror {
    pub(super) repo: RepositoryId,
    /// `null` when the repository has never been mirrored.
    pub(super) mirror: Option<RepositoryMirrorStatus>,
}

#[derive(Debug, Serialize)]
pub(super) struct MirrorPage {
    pub(super) mirrors: Vec<RepositoryMirror>,
    pub(super) total: usize,
    pub(super) page: PageInfo,
}

/// `GET /api/v1/mirrors[?repo=]`: push-mirror status per repository.
pub(super) async fn mirrors(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<ResourceQuery>,
) -> Result<Json<MirrorPage>, PageRejection> {
    let page = query.paging.page()?;
    let repos = visible_repos(&state, &account, query.repo.as_deref());
    let total = repos.len();
    let (repos, page) = page.apply(repos);
    let core = state.github.core();
    let mirrors = repos
        .iter()
        .map(|repo| {
            let checks = core
                .list_check_runs(&repo.owner, &repo.name, None)
                .map(|runs| runs.check_runs)
                .unwrap_or_default();
            RepositoryMirror {
                repo: repo_id(repo),
                mirror: mirror_status(&checks),
            }
        })
        .collect();
    Ok(Json(MirrorPage {
        mirrors,
        total,
        page,
    }))
}

/// The mutable settings of one repository (what `PATCH /api/v1/repos/:id`
/// changes) and whether this viewer may change them.
#[derive(Debug, Serialize)]
pub(super) struct RepositorySettings {
    pub(super) repo: RepositoryId,
    pub(super) default_branch: String,
    pub(super) family: Option<String>,
    pub(super) archived: bool,
    pub(super) visibility: RepositoryVisibility,
    pub(super) can_write: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct SettingsPage {
    /// Forge-wide flags as decided for this viewer (same as the bootstrap).
    pub(super) feature_flags: WebFeatureFlags,
    /// Permission names the forge grants.
    pub(super) permissions: Vec<String>,
    pub(super) settings: Vec<RepositorySettings>,
    pub(super) total: usize,
    pub(super) page: PageInfo,
}

/// `GET /api/v1/settings[?repo=]`: forge flags plus per-repository settings.
pub(super) async fn settings(
    State(state): State<Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<ResourceQuery>,
) -> Result<Json<SettingsPage>, PageRejection> {
    let page = query.paging.page()?;
    let repos = visible_repos(&state, &account, query.repo.as_deref());
    let total = repos.len();
    let (repos, page) = page.apply(repos);
    let core = state.github.core();
    let settings = repos
        .iter()
        .map(|repo| RepositorySettings {
            repo: repo_id(repo),
            default_branch: repo.default_branch.clone(),
            family: repo.family.clone(),
            archived: repo.archived,
            visibility: if repo.private {
                RepositoryVisibility::Private
            } else {
                RepositoryVisibility::Public
            },
            can_write: account.role == UserRole::Admin
                || core.user_can_write_repo(&account.login, &repo.owner, &repo.name),
        })
        .collect();
    Ok(Json(SettingsPage {
        feature_flags: feature_flags(&state, Some(&account)),
        permissions: permissions(),
        settings,
        total,
        page,
    }))
}

/// One persisted forge audit receipt.
#[derive(Debug, Serialize)]
pub(super) struct AuditRecord {
    pub(super) id: String,
    pub(super) occurred_at: String,
    pub(super) actor: String,
    pub(super) action: String,
    pub(super) subject: String,
    pub(super) phase: String,
    pub(super) detail: Value,
}

#[derive(Debug, Serialize)]
pub(super) struct AuditPage {
    pub(super) entries: Vec<AuditRecord>,
    pub(super) total: usize,
    pub(super) page: PageInfo,
}

/// `GET /api/v1/audit[?repo=]`: the forge audit trail, newest first.
///
/// `?repo=` names a subject (`owner/name`) and is read even when that
/// repository no longer exists: the trail outlives a deleted repository.
/// Without it, every registered repository's trail is merged.
pub(super) async fn audit(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ResourceQuery>,
) -> Result<Json<AuditPage>, PageRejection> {
    let page = query.paging.page()?;
    let core = state.github.core();
    let wanted = query
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|repo| !repo.is_empty());
    let subjects: Vec<String> = match wanted {
        Some(wanted) => vec![
            core.list_repositories(None)
                .into_iter()
                .find(|repo| repo.id.to_string() == wanted)
                .map_or_else(|| wanted.to_string(), |repo| repo.full_name),
        ],
        None => core
            .list_repositories(None)
            .into_iter()
            .map(|repo| repo.full_name)
            .collect(),
    };
    let mut entries: Vec<AuditRecord> = subjects
        .iter()
        .flat_map(|subject| core.list_audit(subject).unwrap_or_default())
        .map(|entry| AuditRecord {
            id: entry.id,
            occurred_at: entry.occurred_at,
            actor: entry.actor,
            action: entry.action,
            subject: entry.subject,
            phase: entry.phase,
            detail: entry.detail,
        })
        .collect();
    entries.sort_by(|a, b| b.occurred_at.cmp(&a.occurred_at));
    let total = entries.len();
    let (entries, page) = page.apply(entries);
    Ok(Json(AuditPage {
        entries,
        total,
        page,
    }))
}
