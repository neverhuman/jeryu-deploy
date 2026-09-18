//! `GET /api/v1/deployments?environment=production`: for every repository the
//! caller can read that has a live deployment in the environment, the deployed
//! commit and how many commits its default branch has that the deployment lacks.
//!
//! One request feeds the repositories page's "Unshipped" column; asking each
//! repository's `environments` separately would be one request per row.
//! Repositories with no live deployment in the environment are omitted, which
//! the page renders as "does not ship here" rather than as zero.

use super::*;

#[derive(Debug, Deserialize)]
pub(in crate::web) struct DeployedQuery {
    environment: Option<String>,
}

#[derive(Debug, Serialize)]
struct DeployedRepository {
    repo: String,
    default_branch: String,
    sha: String,
    release: Option<String>,
    deployed_at: String,
    deployed_by: String,
    /// Commits on the default branch the deployment lacks; `None` when git
    /// cannot answer (repository not materialized, deployed commit unknown).
    commits_behind: Option<usize>,
}

#[derive(Debug, Serialize)]
struct DeployedResponse {
    environment: String,
    repositories: Vec<DeployedRepository>,
}

pub(in crate::web) async fn deployed_repositories(
    State(state): State<std::sync::Arc<WebState>>,
    Extension(account): Extension<AccountSummary>,
    Query(query): Query<DeployedQuery>,
) -> AxumResponse {
    let environment = query
        .environment
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "production".to_string());
    let core = state.github.core();
    let mut repositories = Vec::new();
    for repo in core.list_repositories(None) {
        if account.role != UserRole::Admin
            && !core.user_can_read_repo(&account.login, &repo.owner, &repo.name)
        {
            continue;
        }
        let Ok(environments) = core.deployment_environments(&repo.owner, &repo.name) else {
            continue;
        };
        let Some(current) = environments
            .into_iter()
            .find(|env| env.environment == environment)
            .and_then(|env| env.current)
        else {
            continue;
        };
        let deployment = current.deployment;
        let release = deployment
            .payload
            .get("release")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        repositories.push(DeployedRepository {
            commits_behind: commits_behind(&state, &repo, &deployment.sha),
            repo: repo.full_name.clone(),
            default_branch: repo.default_branch.clone(),
            sha: deployment.sha,
            release,
            deployed_at: deployment.created_at.to_rfc3339(),
            deployed_by: deployment.creator,
        });
    }
    Json(DeployedResponse {
        environment,
        repositories,
    })
    .into_response()
}

/// `git rev-list --count <sha>..<default branch>` in the bare repository. The
/// sha is the deployment's validated 40-hex commit; the branch is the
/// repository's configured default, passed after `--end-of-options`.
fn commits_behind(state: &WebState, repo: &Repository, sha: &str) -> Option<usize> {
    let resolved = state
        .repo_manager
        .open_parts(&repo.owner, &repo.name)
        .ok()?;
    let range = format!("{sha}..refs/heads/{}", repo.default_branch);
    let out = Command::new(&state.repo_manager.config().git_bin)
        .arg("-C")
        .arg(&resolved.path)
        .args(["rev-list", "--count", "--end-of-options", &range])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::catalog::SplitCatalog;
    use jeryu_core::{
        CreateDeploymentRequest, CreateDeploymentStatusRequest, CreateRepositoryRequest,
        DeploymentState, ForgeCore, RepoAccessLevel,
    };
    use jeryu_gitd::{GitdConfig, RepoId, RepoManager};

    fn repo(core: &ForgeCore, name: &str) {
        core.create_repository(
            "alice",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: true,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }

    fn deploy(core: &ForgeCore, repo: &str, sha: &str, environment: &str, succeed: bool) {
        let id = core
            .create_deployment(
                "alice",
                repo,
                "alton2",
                CreateDeploymentRequest {
                    sha: sha.to_string(),
                    ref_name: None,
                    task: "deploy".to_string(),
                    environment: environment.to_string(),
                    description: None,
                    payload: Some(serde_json::json!({ "release": "rel-2" })),
                    production_environment: None,
                    transient_environment: false,
                },
            )
            .unwrap()
            .id;
        if succeed {
            core.create_deployment_status(
                "alice",
                repo,
                id,
                "alton2",
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
    }

    async fn call(
        state: &std::sync::Arc<WebState>,
        account: AccountSummary,
        environment: Option<&str>,
    ) -> serde_json::Value {
        let query = DeployedQuery {
            environment: environment.map(str::to_string),
        };
        let response =
            deployed_repositories(State(state.clone()), Extension(account), Query(query)).await;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// alice/svc: main has four commits and production runs the second.
    /// alice/lib: never deployed. alice/secret: deployed but unreadable to bob.
    #[tokio::test]
    async fn lists_live_deployments_with_commits_behind_for_readable_repos() {
        let storage = tempfile::tempdir().unwrap();
        let manager = RepoManager::new(GitdConfig::new(storage.path().to_path_buf()));
        let bare = manager
            .create_bare(&RepoId::new("alice", "svc").unwrap())
            .unwrap();
        let work = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
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
        let mut shas = Vec::new();
        for n in 1..=4 {
            run(&[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("change {n}"),
            ]);
            shas.push(run(&["rev-parse", "HEAD"]));
        }
        run(&["push", "-q", &bare.path.to_string_lossy(), "main"]);

        let core = ForgeCore::new();
        for name in ["svc", "lib", "secret"] {
            repo(&core, name);
        }
        core.create_account("root", "admin-password", UserRole::Admin)
            .unwrap();
        core.create_account("bob", "user-password", UserRole::User)
            .unwrap();
        core.grant_repo_access("root", "bob", "alice", "svc", RepoAccessLevel::Read)
            .unwrap();
        deploy(&core, "svc", &shas[1], "production", true);
        deploy(&core, "svc", &shas[1], "canary", true);
        deploy(&core, "secret", &"a".repeat(40), "production", true);
        deploy(&core, "lib", &"b".repeat(40), "production", false);
        let root = core.get_account("root").unwrap();
        let bob = core.get_account("bob").unwrap();

        let state = std::sync::Arc::new(WebState::with_repo_manager(
            core,
            std::sync::Arc::new(manager),
            std::path::PathBuf::from("/tmp/jeryu-no-spa"),
            std::env::temp_dir(),
            SplitCatalog::builtin(),
        ));

        let body = call(&state, root.clone(), None).await;
        assert_eq!(body["environment"], "production");
        let names: Vec<&str> = body["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["repo"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["alice/secret", "alice/svc"],
            "a never-successful deployment is not live: {body}"
        );
        let svc = &body["repositories"][1];
        assert_eq!(svc["sha"], shas[1]);
        assert_eq!(svc["release"], "rel-2");
        assert_eq!(svc["deployed_by"], "alton2");
        assert_eq!(svc["commits_behind"], 2);
        assert_eq!(
            body["repositories"][0]["commits_behind"],
            serde_json::Value::Null,
            "no git data means unknown, not zero"
        );

        let for_bob = call(&state, bob, None).await;
        assert_eq!(
            for_bob["repositories"].as_array().unwrap().len(),
            1,
            "a reader sees only what it can read: {for_bob}"
        );
        assert_eq!(for_bob["repositories"][0]["repo"], "alice/svc");

        assert_eq!(
            call(&state, root.clone(), Some("canary")).await["repositories"][0]["commits_behind"],
            2
        );
        assert_eq!(
            call(&state, root, Some("staging")).await["repositories"],
            serde_json::json!([])
        );
    }
}
