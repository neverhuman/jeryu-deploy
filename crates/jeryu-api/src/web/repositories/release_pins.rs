//! `GET /api/v1/release-pins`: every repository whose code ships inside
//! another repository's deployment, and the commit of it that deployment
//! carries.
//!
//! A repository released only as part of a parent (jeryu-web inside
//! jeryu-deploy) records no deployment of its own, so the release views would
//! fall back to its newest tag — months and hundreds of commits behind, which
//! caps the compare and leaves merged pull requests undecided. The parent's
//! deployment payload names the commit it was built from
//! (`jeryu_web_commit`), which is the baseline those views want; the shift
//! truth cache already reads it the same way.
//!
//! One request answers for the whole forge: asking each repository separately
//! would not help, because the answer lives in a *different* repository's
//! deployment.

use super::*;

/// Payload keys naming a repository's commit end in this.
const COMMIT_KEY_SUFFIX: &str = "_commit";

#[derive(Debug, Serialize)]
struct ReleasePin {
    /// The pinned repository, `owner/name`.
    repo: String,
    /// The parent deployment's environment: dev, canary, stable, production.
    environment: String,
    /// The pinned repository's commit the parent release carries.
    sha: String,
    /// `owner/name` of the repository whose deployment carries it.
    parent: String,
    /// The parent deployment's release id, when its payload names one.
    release: Option<String>,
    deployed_at: String,
}

#[derive(Debug, Serialize)]
struct ReleasePinsResponse {
    pins: Vec<ReleasePin>,
}

pub(in crate::web) async fn release_pins(
    State(state): State<std::sync::Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
) -> AxumResponse {
    let core = state.github.core();
    let readable = |owner: &str, name: &str| {
        account.role == UserRole::Admin || core.user_can_read_repo(&account.login, owner, name)
    };
    let repos: Vec<Repository> = core
        .list_repositories(None)
        .into_iter()
        .filter(|repo| readable(&repo.owner, &repo.name))
        .collect();
    // Name -> `owner/name`, for resolving `jeryu_web_commit` to a repository.
    // A name two owners share cannot be resolved this way, so it resolves
    // only against the parent's own owner.
    let mut by_name: BTreeMap<&str, Option<&Repository>> = BTreeMap::new();
    for repo in &repos {
        match by_name.entry(repo.name.as_str()) {
            Entry::Vacant(slot) => {
                slot.insert(Some(repo));
            }
            Entry::Occupied(mut slot) => {
                slot.insert(None);
            }
        }
    }
    let mut pins = Vec::new();
    for parent in &repos {
        let Ok(environments) = core.deployment_environments(&parent.owner, &parent.name) else {
            continue;
        };
        for environment in environments {
            let Some(current) = environment.current else {
                continue;
            };
            let deployment = current.deployment;
            let release = deployment
                .payload
                .get("release")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            let Some(payload) = deployment.payload.as_object() else {
                continue;
            };
            for (key, value) in payload {
                let Some(name) = key.strip_suffix(COMMIT_KEY_SUFFIX) else {
                    continue;
                };
                let name = name.replace('_', "-");
                let Some(sha) = value.as_str().filter(|sha| is_commit_sha(sha)) else {
                    continue;
                };
                // A payload key is free-form text: it names a pin only when it
                // names a repository, and never the parent's own commit.
                let Some(child) = repos
                    .iter()
                    .find(|repo| repo.owner == parent.owner && repo.name == name)
                    .or_else(|| by_name.get(name.as_str()).copied().flatten())
                else {
                    continue;
                };
                if child.full_name == parent.full_name {
                    continue;
                }
                pins.push(ReleasePin {
                    repo: child.full_name.clone(),
                    environment: environment.environment.clone(),
                    sha: sha.to_string(),
                    parent: parent.full_name.clone(),
                    release: release.clone(),
                    deployed_at: deployment.created_at.to_rfc3339(),
                });
            }
        }
    }
    pins.sort_by(|a, b| {
        (&a.repo, &a.environment, &a.parent).cmp(&(&b.repo, &b.environment, &b.parent))
    });
    Json(ReleasePinsResponse { pins }).into_response()
}

fn is_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::catalog::SplitCatalog;
    use jeryu_core::{
        CreateDeploymentRequest, CreateDeploymentStatusRequest, CreateRepositoryRequest,
        DeploymentState, ForgeCore, RepoAccessLevel,
    };
    use jeryu_gitd::{GitdConfig, RepoManager};

    fn repo(core: &ForgeCore, name: &str) {
        core.create_repository(
            "jeryu",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: true,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }

    fn deploy(
        core: &ForgeCore,
        repo: &str,
        sha: &str,
        environment: &str,
        payload: serde_json::Value,
    ) {
        let id = core
            .create_deployment(
                "jeryu",
                repo,
                "rel-bot",
                CreateDeploymentRequest {
                    sha: sha.to_string(),
                    ref_name: None,
                    task: "deploy".to_string(),
                    environment: environment.to_string(),
                    description: None,
                    payload: Some(payload),
                    production_environment: None,
                    transient_environment: false,
                },
            )
            .unwrap()
            .id;
        core.create_deployment_status(
            "jeryu",
            repo,
            id,
            "rel-bot",
            CreateDeploymentStatusRequest {
                state: DeploymentState::Success,
                description: None,
                environment_url: None,
                log_url: None,
                auto_inactive: true,
            },
        )
        .unwrap();
    }

    async fn call(state: &std::sync::Arc<WebState>, account: AccountSummary) -> serde_json::Value {
        let response = release_pins(State(state.clone()), Extension(account)).await;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// jeryu-deploy's production and stable releases each name the jeryu-web
    /// commit they were built from; jeryu-web has no deployment of its own.
    #[tokio::test]
    async fn reports_the_commit_a_parent_release_pins_for_each_environment() {
        let core = ForgeCore::new();
        for name in ["jeryu-deploy", "jeryu-web", "jeryu-tool"] {
            repo(&core, name);
        }
        core.create_account("root", "admin-password", UserRole::Admin)
            .unwrap();
        core.create_account("bob", "user-password", UserRole::User)
            .unwrap();
        core.grant_repo_access(
            "root",
            "bob",
            "jeryu",
            "jeryu-deploy",
            RepoAccessLevel::Read,
        )
        .unwrap();
        let web = "c".repeat(40);
        let older = "d".repeat(40);
        deploy(
            &core,
            "jeryu-deploy",
            &"a".repeat(40),
            "production",
            serde_json::json!({
                "release": "prod-20261005T144904Z-c29a93e",
                "jeryu_web_commit": web,
                // Neither names a repository: not a pin.
                "live_commit": "e".repeat(40),
                "web_dist_sha256": "f".repeat(64),
            }),
        );
        deploy(
            &core,
            "jeryu-deploy",
            &"b".repeat(40),
            "stable",
            serde_json::json!({ "jeryu_web_commit": older }),
        );
        // A pin is a commit sha, not a branch name or a release id.
        deploy(
            &core,
            "jeryu-tool",
            &"9".repeat(40),
            "production",
            serde_json::json!({ "jeryu_web_commit": "main" }),
        );
        let root = core.get_account("root").unwrap();
        let bob = core.get_account("bob").unwrap();

        let state = std::sync::Arc::new(WebState::with_repo_manager(
            core,
            std::sync::Arc::new(RepoManager::new(GitdConfig::new(
                std::env::temp_dir().join("jeryu-release-pins-test-git"),
            ))),
            std::path::PathBuf::from("/tmp/jeryu-no-spa"),
            std::env::temp_dir(),
            SplitCatalog::builtin(),
        ));

        let body = call(&state, root).await;
        let pins = body["pins"].as_array().unwrap();
        assert_eq!(pins.len(), 2, "{body}");
        assert_eq!(pins[0]["repo"], "jeryu/jeryu-web");
        assert_eq!(pins[0]["environment"], "production");
        assert_eq!(pins[0]["sha"], web);
        assert_eq!(pins[0]["parent"], "jeryu/jeryu-deploy");
        assert_eq!(pins[0]["release"], "prod-20261005T144904Z-c29a93e");
        assert!(pins[0]["deployed_at"].is_string(), "{body}");
        assert_eq!(pins[1]["environment"], "stable");
        assert_eq!(pins[1]["sha"], older);
        assert!(
            pins[1]["release"].is_null(),
            "a release-less payload names no release: {body}"
        );

        // bob can read the parent but not jeryu-web: the pin is about
        // jeryu-web's release state, so it stays hidden.
        let for_bob = call(&state, bob).await;
        assert_eq!(for_bob["pins"], serde_json::json!([]), "{for_bob}");
    }
}
