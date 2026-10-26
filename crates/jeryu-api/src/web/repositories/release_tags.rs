//! `GET /api/v1/release-tags?repos=<a>,<b>[&branch=]`: the newest tag
//! reachable from a branch, for many repositories in one request.
//!
//! The release views asked `GET /api/v1/repos/:id/release-tag` once per
//! repository, and a repository with no git data or no such branch answered
//! `404`, which a page cannot tell from a failed request. Here every
//! repository asked for gets one row, in the order asked, and why there is no
//! tag is a `status` value:
//!
//! - `tagged`: `tag`, `sha` and `tagged_at` name the newest reachable tag.
//! - `untagged`: the branch exists and carries no tag; nothing was released.
//! - `no_branch`: the repository has no such branch.
//! - `no_git_data`: a metadata-only import with no repository storage.
//! - `not_found`: no such repository, or one the caller may not read (the
//!   two read alike, as on every repository route).
//!
//! `repos` names at most [`MAX_REPOS`] repositories, each an id or
//! `owner/name`; a repeated one is answered once. `branch` applies to every
//! repository; without it each uses its own default branch. The single-repo
//! route is unchanged.

use super::compare::is_revision;
use super::release_tag::{TagLookupMiss, nearest_release_tag};
use super::*;
use crate::web::strict_query::{StrictFields, StrictQuery};

/// The most repositories one request names: the bound on the `git` work it
/// costs (a few short `git` runs per repository).
pub(in crate::web) const MAX_REPOS: usize = 100;

#[derive(Debug, Default, Deserialize)]
pub(in crate::web) struct ReleaseTagsQuery {
    /// Comma-separated repository ids or `owner/name`s.
    pub(in crate::web) repos: Option<String>,
    /// The branch to read on every repository; each one's default otherwise.
    pub(in crate::web) branch: Option<String>,
}

impl StrictFields for ReleaseTagsQuery {
    const KEYS: &'static [&'static str] = &["repos", "branch"];
    const REPAIR_HINT: &'static str =
        "send repos as up to 100 comma-separated owner/name or ids, and branch as a branch name";

    fn check_values(&self) -> Result<(), String> {
        let wanted = wanted(self);
        if wanted.is_empty() {
            return Err("repos: name at least one repository (owner/name or id)".to_string());
        }
        if wanted.len() > MAX_REPOS {
            return Err(format!(
                "repos: {} repositories is more than the {MAX_REPOS} one request answers",
                wanted.len()
            ));
        }
        if let Some(branch) = self.branch.as_deref()
            && !is_revision(branch)
        {
            return Err("branch must be a branch name or commit sha".to_string());
        }
        Ok(())
    }
}

/// The repositories asked for, trimmed, in order, each once.
fn wanted(query: &ReleaseTagsQuery) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    query
        .repos
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .filter(|repo| seen.insert(repo.to_string()))
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Serialize)]
pub(in crate::web) struct ReleaseTagRow {
    /// The repository as the request named it.
    repo: String,
    /// `owner/name`; `null` when `status` is `not_found`.
    full_name: Option<String>,
    /// The branch read; `null` when `status` is `not_found`.
    branch: Option<String>,
    /// `tagged`, `untagged`, `no_branch`, `no_git_data` or `not_found`.
    status: &'static str,
    tag: Option<String>,
    sha: Option<String>,
    /// The tagged commit's committer date, as on the single-repo route.
    tagged_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub(in crate::web) struct ReleaseTagsResponse {
    releases: Vec<ReleaseTagRow>,
}

pub(in crate::web) async fn release_tags(
    State(state): State<std::sync::Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    StrictQuery(query): StrictQuery<ReleaseTagsQuery>,
) -> AxumResponse {
    let wanted = wanted(&query);
    let branch = query.branch.clone();
    let rows = tokio::task::spawn_blocking(move || {
        let core = state.github.core();
        let repos = core.list_repositories(None);
        wanted
            .into_iter()
            .map(|asked| {
                let found = repos
                    .iter()
                    .find(|repo| repo.id.to_string() == asked || repo.full_name == asked)
                    .filter(|repo| {
                        // The repository route gate's read rule.
                        account.role == UserRole::Admin
                            || !repo.private
                            || core.user_can_read_repo(&account.login, &repo.owner, &repo.name)
                    });
                let Some(repo) = found else {
                    return ReleaseTagRow {
                        repo: asked,
                        full_name: None,
                        branch: None,
                        status: "not_found",
                        tag: None,
                        sha: None,
                        tagged_at: None,
                    };
                };
                let read = branch
                    .clone()
                    .unwrap_or_else(|| repo.default_branch.clone());
                let (status, released) = match nearest_release_tag(&state, repo, &read) {
                    Ok(Some(released)) => ("tagged", Some(released)),
                    Ok(None) => ("untagged", None),
                    Err(TagLookupMiss::NoBranch) => ("no_branch", None),
                    Err(TagLookupMiss::NoGitData) => ("no_git_data", None),
                };
                ReleaseTagRow {
                    repo: asked,
                    full_name: Some(repo.full_name.clone()),
                    branch: Some(read),
                    status,
                    tag: released.as_ref().map(|released| released.tag.clone()),
                    sha: released.as_ref().and_then(|released| released.sha.clone()),
                    tagged_at: released.and_then(|released| released.tagged_at),
                }
            })
            .collect::<Vec<_>>()
    })
    .await;
    match rows {
        Ok(releases) => Json(ReleaseTagsResponse { releases }).into_response(),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &error.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, header};
    use jeryu_core::{CreateRepositoryRequest, ForgeCore, RepoAccessLevel, UserRole};
    use jeryu_gitd::{GitdConfig, RepoId, RepoManager};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;
    use crate::web::catalog::SplitCatalog;

    /// One request answers every repository asked for, in order, with why
    /// there is no tag as a value, and never shows a caller a repository it
    /// may not read.
    #[tokio::test]
    async fn one_request_answers_many_repositories_and_no_tag_is_a_value() {
        let storage = tempfile::tempdir().unwrap();
        let manager = RepoManager::new(GitdConfig::new(storage.path().to_path_buf()));
        let push = |name: &str, tag: Option<&str>| -> String {
            let bare = manager
                .create_bare(&RepoId::new("alice", name).unwrap())
                .unwrap();
            let work = tempfile::tempdir().unwrap();
            let run = |args: &[&str]| {
                let out = crate::test_git::git_command()
                    .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                    .args(["-c", "init.defaultBranch=main"])
                    .args(args)
                    .current_dir(work.path())
                    .output()
                    .unwrap();
                assert!(out.status.success(), "git {args:?}");
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            run(&["init", "-q"]);
            run(&["commit", "-q", "--allow-empty", "-m", "feat: one"]);
            if let Some(tag) = tag {
                run(&["tag", tag]);
            }
            run(&["push", "-q", &bare.path.to_string_lossy(), "main", "--tags"]);
            run(&["rev-parse", "HEAD"])
        };
        let vault_sha = push("vault", Some("v7.7.7-secret"));
        push("plain", None);

        let core = ForgeCore::new();
        for (name, private) in [("vault", true), ("plain", false), ("ghost", false)] {
            core.create_repository(
                "alice",
                CreateRepositoryRequest {
                    name: name.to_string(),
                    private,
                    description: None,
                    default_branch: Some("main".to_string()),
                },
            )
            .unwrap();
        }
        core.create_account("jeryu-admin", "admin-password", UserRole::Admin)
            .unwrap();
        core.create_account("reader", "reader-password", UserRole::User)
            .unwrap();
        core.create_account("outsider", "outsider-password", UserRole::User)
            .unwrap();
        core.grant_repo_access(
            "jeryu-admin",
            "reader",
            "alice",
            "vault",
            RepoAccessLevel::Read,
        )
        .unwrap();
        let token = |login: &str| {
            core.create_personal_access_token(login, "test", None)
                .unwrap()
                .secret
        };
        let (admin, reader, outsider) = (token("jeryu-admin"), token("reader"), token("outsider"));
        let state = WebState::with_repo_manager(
            core,
            std::sync::Arc::new(manager),
            std::path::PathBuf::from("/tmp/jeryu-no-spa"),
            std::env::temp_dir(),
            SplitCatalog::builtin(),
        )
        .with_auth(true, false, false);
        let app = crate::web::app(state, std::path::Path::new("/tmp/jeryu-no-spa"));
        let get = |path: String, token: String| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::builder()
                            .uri(path)
                            .header(header::AUTHORIZATION, format!("Bearer {token}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (status, String::from_utf8_lossy(&bytes).to_string())
            }
        };
        let all = "/api/v1/release-tags?repos=alice/vault,alice/plain,alice/ghost,alice/missing,alice/vault";

        let (status, body) = get(all.to_string(), reader.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let body: Value = serde_json::from_str(&body).unwrap();
        let rows = body["releases"].as_array().unwrap();
        assert_eq!(rows.len(), 4, "the repeated repository is answered once");
        assert_eq!(
            rows[0],
            json!({"repo": "alice/vault", "full_name": "alice/vault", "branch": "main",
                   "status": "tagged", "tag": "v7.7.7-secret", "sha": vault_sha,
                   "tagged_at": rows[0]["tagged_at"]})
        );
        assert!(rows[0]["tagged_at"].is_string());
        let status_of = |rows: &[Value]| -> Vec<String> {
            rows.iter()
                .map(|row| row["status"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            status_of(rows),
            ["tagged", "untagged", "no_git_data", "not_found"]
        );
        assert!(rows[1]["tag"].is_null());

        // A caller who may not read the private repository reads it as absent.
        let (status, body) = get(all.to_string(), outsider).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.contains("v7.7.7-secret") && !body.contains(&vault_sha),
            "{body}"
        );
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            status_of(body["releases"].as_array().unwrap()),
            ["not_found", "untagged", "no_git_data", "not_found"]
        );

        let (_, body) = get(
            "/api/v1/release-tags?repos=alice/plain&branch=nope".to_string(),
            admin.clone(),
        )
        .await;
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["releases"][0]["status"], "no_branch");
        assert_eq!(body["releases"][0]["branch"], "nope");

        let too_many: Vec<String> = (0..=MAX_REPOS).map(|n| format!("alice/r{n}")).collect();
        for bad in [
            "/api/v1/release-tags".to_string(),
            "/api/v1/release-tags?repos=,".to_string(),
            format!("/api/v1/release-tags?repos={}", too_many.join(",")),
            "/api/v1/release-tags?repos=alice/plain&branch=--output%3D/x".to_string(),
            "/api/v1/release-tags?repo=alice/plain".to_string(),
        ] {
            let (status, body) = get(bad.clone(), admin.clone()).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
        }
    }
}
