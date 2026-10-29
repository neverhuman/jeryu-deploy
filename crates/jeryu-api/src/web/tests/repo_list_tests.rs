use super::*;

/// `GET /api/v1/repos` as the handler answers it, decoded. The handler
/// returns a response because a `?family=` nobody hosts is typed refusal.
async fn repo_page(response: AxumResponse) -> jeryu_readmodel::contracts::RepositoryListResponse {
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    serde_json::from_value(response_json(response).await).expect("a repository page")
}

#[test]
fn repo_list_classifies_jeryu_split_portal_and_members() {
    let core = ForgeCore::new();
    for (owner, name) in [
        ("neverhuman", "jeryu"),
        ("neverhuman", "jeryu-core"),
        ("alice", "unrelated"),
    ] {
        core.create_repository(
            owner,
            CreateRepositoryRequest {
                name: name.to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }
    let state = WebState::new(core);
    let repos = repo_list_response(&state);
    let portal = repos
        .repositories
        .iter()
        .find(|repo| repo.id.owner == "neverhuman" && repo.id.name == "jeryu")
        .expect("portal repo");
    assert_eq!(portal.family.as_deref(), Some("jeryu"));
    assert_eq!(portal.repo_role, Some(RepositoryRole::PublicPortal));
    // The advertised clone URL must match where the smart-HTTP transport is
    // actually mounted (/git/...), not the SPA surface (/repos/...).
    assert_eq!(
        portal.clone_http_url.as_deref(),
        Some("/git/neverhuman/jeryu.git")
    );

    let core_repo = repos
        .repositories
        .iter()
        .find(|repo| repo.id.owner == "neverhuman" && repo.id.name == "jeryu-core")
        .expect("split member repo");
    assert_eq!(core_repo.family.as_deref(), Some("jeryu"));
    assert_eq!(core_repo.repo_role, Some(RepositoryRole::SplitMember));

    let unrelated = repos
        .repositories
        .iter()
        .find(|repo| repo.id.owner == "alice" && repo.id.name == "unrelated")
        .expect("unrelated repo");
    assert_eq!(unrelated.family, None);
    assert_eq!(unrelated.repo_role, None);
    assert_eq!(repos.facets.families, vec!["jeryu".to_string()]);
}

#[test]
fn split_catalog_loads_multiple_manifest_families() {
    let root = tempdir().expect("split manifests dir");
    write_file(
        root.path(),
        "jeryu.toml",
        r#"
repo_family = "jeryu-split"

[[repo]]
github_slug = "neverhuman/jeryu"
jeryu_slug = "jeryu/jeryu"
profile = "public-portal"

[[repo]]
github_slug = "neverhuman/jeryu-core"
jeryu_slug = "jeryu/jeryu-core"
profile = "split-member"
"#,
    );
    write_file(
        root.path(),
        "jekko.toml",
        r#"
repo_family = "jekko-split"

[[repo]]
github_slug = "neverhuman/jekko"
jeryu_slug = "jeryu/jekko"
profile = "public-portal"

[[repo]]
github_slug = "neverhuman/jekko-core"
jeryu_slug = "jeryu/jekko-core"
profile = "split-member"
"#,
    );

    let manifests = vec![
        root.path().join("jeryu.toml"),
        root.path().join("jekko.toml"),
    ];
    let catalog = SplitCatalog::load(&manifests);
    assert_eq!(
        catalog.classify("jeryu", "jeryu"),
        Some(("jeryu-split".to_string(), RepositoryRole::PublicPortal))
    );
    assert_eq!(
        catalog.classify("jeryu", "jeryu-core"),
        Some(("jeryu-split".to_string(), RepositoryRole::SplitMember))
    );
    assert_eq!(
        catalog.classify("jeryu", "jekko"),
        Some(("jekko-split".to_string(), RepositoryRole::PublicPortal))
    );
    assert_eq!(
        catalog.classify("neverhuman", "jekko-core"),
        Some(("jekko-split".to_string(), RepositoryRole::SplitMember))
    );
    assert_eq!(catalog.classify("alice", "unrelated"), None);
}

#[test]
fn split_catalog_classifies_tool_control_plane_and_finder() {
    // The tool control plane and its discovery arm ride the same `public-portal`
    // build profile as the real portal; the `-tool` / `-tool-finder` names must
    // disambiguate so only `jeryu` is the portal.
    let root = tempdir().expect("split manifests dir");
    write_file(
        root.path(),
        "jeryu.toml",
        r#"
repo_family = "jeryu-split"

[[repo]]
name = "jeryu"
github_slug = "neverhuman/jeryu"
jeryu_slug = "jeryu/jeryu"
profile = "public-portal"

[[repo]]
name = "jeryu-tool"
github_slug = "neverhuman/jeryu-tool"
jeryu_slug = "jeryu/jeryu-tool"
profile = "public-portal"

[[repo]]
name = "jeryu-tool-finder"
github_slug = "neverhuman/jeryu-tool-finder"
jeryu_slug = "jeryu/jeryu-tool-finder"
profile = "public-portal"
"#,
    );
    let catalog = SplitCatalog::load(&[root.path().join("jeryu.toml")]);
    assert_eq!(
        catalog.classify("jeryu", "jeryu"),
        Some(("jeryu-split".to_string(), RepositoryRole::PublicPortal))
    );
    assert_eq!(
        catalog.classify("jeryu", "jeryu-tool"),
        Some(("jeryu-split".to_string(), RepositoryRole::ToolControlPlane))
    );
    assert_eq!(
        catalog.classify("neverhuman", "jeryu-tool"),
        Some(("jeryu-split".to_string(), RepositoryRole::ToolControlPlane))
    );
    // The finder rides public-portal but is a normal member, not a portal.
    assert_eq!(
        catalog.classify("jeryu", "jeryu-tool-finder"),
        Some(("jeryu-split".to_string(), RepositoryRole::SplitMember))
    );
}

#[test]
fn split_catalog_files_unlisted_tool_repos_under_the_portal_family() {
    // The live manifest lists only the split members; the tool repos are
    // hosted beside them and must still land in the portal's family rather
    // than whatever family the forge database carries.
    let root = tempdir().expect("split manifests dir");
    write_file(
        root.path(),
        "jeryu.toml",
        r#"
repo_family = "jeryu-split"

[[repo]]
name = "jeryu"
github_slug = "neverhuman/jeryu"
jeryu_slug = "jeryu/jeryu"
profile = "public-portal"

[[repo]]
name = "jeryu-core"
github_slug = "neverhuman/jeryu-core"
jeryu_slug = "jeryu/jeryu-core"
profile = "rust-workspace"
"#,
    );
    let catalog = SplitCatalog::load(&[root.path().join("jeryu.toml")]);
    for owner in ["jeryu", "neverhuman"] {
        assert_eq!(
            catalog.classify(owner, "jeryu-tool"),
            Some(("jeryu-split".to_string(), RepositoryRole::ToolControlPlane))
        );
        assert_eq!(
            catalog.classify(owner, "jeryu-tool-finder"),
            Some(("jeryu-split".to_string(), RepositoryRole::SplitMember))
        );
    }
    assert_eq!(catalog.classify("jeryu", "jeryu-core-tool"), None);
}

#[test]
fn repo_list_family_filter_uses_multi_manifest_catalog() {
    let manifest_root = tempdir().expect("split manifests dir");
    write_file(
        manifest_root.path(),
        "jeryu.toml",
        r#"
repo_family = "jeryu-split"

[[repo]]
github_slug = "neverhuman/jeryu"
jeryu_slug = "jeryu/jeryu"
profile = "public-portal"

[[repo]]
github_slug = "neverhuman/jeryu-core"
jeryu_slug = "jeryu/jeryu-core"
profile = "split-member"
"#,
    );
    write_file(
        manifest_root.path(),
        "jekko.toml",
        r#"
repo_family = "jekko-split"

[[repo]]
github_slug = "neverhuman/jekko"
jeryu_slug = "jeryu/jekko"
profile = "public-portal"

[[repo]]
github_slug = "neverhuman/jekko-core"
jeryu_slug = "jeryu/jekko-core"
profile = "split-member"
"#,
    );

    let core = ForgeCore::new();
    for name in ["jeryu", "jeryu-core", "jekko", "jekko-core", "unrelated"] {
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
    let manifests = vec![
        manifest_root.path().join("jeryu.toml"),
        manifest_root.path().join("jekko.toml"),
    ];
    let storage = tempdir().expect("git storage dir");
    let state = WebState::with_repo_manager(
        core,
        Arc::new(RepoManager::new(GitdConfig::new(
            storage.path().to_path_buf(),
        ))),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/web/dist"),
        std::env::temp_dir(),
        SplitCatalog::load(&manifests),
    );

    let family_only = crate::web::repositories::filtered_repo_list_response(
        &state,
        &crate::web::repositories::RepoListQuery {
            family: Some("jekko-split".to_string()),
            ..Default::default()
        },
    );
    let mut names: Vec<&str> = family_only
        .repositories
        .iter()
        .map(|repo| repo.id.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(family_only.total, 2);
    assert_eq!(names, vec!["jekko", "jekko-core"]);
    assert_eq!(
        family_only.facets.families,
        vec!["jekko".to_string(), "jeryu".to_string()]
    );
    let portal = family_only
        .repositories
        .iter()
        .find(|repo| repo.id.name == "jekko")
        .expect("jekko portal");
    assert_eq!(portal.repo_role, Some(RepositoryRole::PublicPortal));
}

/// Health and the failing/running badges must reflect the repository's CURRENT
/// state — the latest run per check on a live head — not the append-only
/// check-run history. Legacy failures on stale shas and superseded failures on
/// a live head are both invisible once a newer green run exists.
#[test]
fn repo_health_scopes_check_runs_to_current_heads() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    // Legacy failure on a sha no open PR (or branch head) points at anymore.
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "jeryu/ci".to_string(),
            head_sha: "stale-sha".to_string(),
            conclusion: Some(CheckConclusion::Failure),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    // Open PR whose head first failed, then passed on a rerun: only the
    // newest verdict for (head, name) may count.
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "feature".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("live-sha".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "jeryu/ci".to_string(),
            head_sha: "live-sha".to_string(),
            conclusion: Some(CheckConclusion::Failure),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "jeryu/ci".to_string(),
            head_sha: "live-sha".to_string(),
            conclusion: Some(CheckConclusion::Success),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();

    let state = WebState::new(core);
    let repos = repo_list_response(&state);
    let summary = &repos.repositories[0];
    assert_eq!(
        summary.failing_checks, 0,
        "stale + superseded failures must not count"
    );
    assert_eq!(summary.health, "healthy");
    assert_eq!(summary.open_pull_requests, 1);
}

/// A failure that IS the latest verdict on a live head flips health to
/// warning, while a failing `jeryu/github-mirror` bookkeeping run never does —
/// mirror state has its own surface. In-progress runs only count on live heads.
#[test]
fn repo_health_counts_live_failures_and_ignores_mirror_checks() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "feature".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("live-sha".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    // Latest verdict for jeryu/ci on the live head is a failure.
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "jeryu/ci".to_string(),
            head_sha: "live-sha".to_string(),
            conclusion: Some(CheckConclusion::Failure),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    // A failed mirror push on the same head is bookkeeping, not ill-health.
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "jeryu/github-mirror".to_string(),
            head_sha: "live-sha".to_string(),
            conclusion: Some(CheckConclusion::Failure),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    // Running job on the live head counts; on a stale sha it does not.
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "jeryu/agent-review".to_string(),
            head_sha: "live-sha".to_string(),
            status: Some(jeryu_core::CheckRunStatus::InProgress),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "jeryu/agent-review".to_string(),
            head_sha: "stale-sha".to_string(),
            status: Some(jeryu_core::CheckRunStatus::InProgress),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();

    let state = WebState::new(core);
    let repos = repo_list_response(&state);
    let summary = &repos.repositories[0];
    assert_eq!(
        summary.failing_checks, 1,
        "mirror failure excluded, ci failure counted"
    );
    assert_eq!(summary.health, "warning");
    assert_eq!(summary.running_jobs, 1, "stale in-progress run excluded");
}

/// An archived repository is read-only: a failing check on it never turns its
/// health to warning and a failed mirror push on it never reaches the
/// attention inbox. It stays listed under the Archived filter.
#[test]
fn archived_repo_is_ignored_by_health_and_the_attention_inbox() {
    let core = ForgeCore::new();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "retired".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.create_pull_request(
        "jeryu",
        "retired",
        "alice",
        CreatePullRequestRequest {
            title: "feature".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("live-sha".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    for name in ["jeryu/ci", "jeryu/github-mirror"] {
        core.create_check_run(
            "jeryu",
            "retired",
            CreateCheckRunRequest {
                name: name.to_string(),
                head_sha: "live-sha".to_string(),
                conclusion: Some(CheckConclusion::Failure),
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap();
    }
    let state = WebState::new(core.clone());
    assert_eq!(repo_list_response(&state).repositories[0].health, "warning");
    assert_eq!(crate::web::repositories::mirror_failures(&state).len(), 1);

    core.set_repository_archived("alice", "jeryu", "retired", true)
        .unwrap();
    let summary = crate::web::repositories::repo_summary(
        &state,
        &core.get_repository("jeryu", "retired").unwrap(),
    );
    assert!(summary.archived);
    assert_eq!(summary.health, "healthy");
    assert!(crate::web::repositories::mirror_failures(&state).is_empty());
}

/// Score ingest → list → repo-summary badge join, plus mirror status derived
/// from jeryu/github-mirror bookkeeping runs.
#[tokio::test]
async fn jankurai_scores_ingest_and_surface_on_the_repo_summary() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "jeryu",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: true,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    // Mirror bookkeeping: an old success then a newer failure -> last attempt
    // failed, last success still reported, repo health untouched.
    core.create_check_run(
        "jeryu",
        "jeryu",
        CreateCheckRunRequest {
            name: "jeryu/github-mirror".to_string(),
            head_sha: "m1".to_string(),
            conclusion: Some(CheckConclusion::Success),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    core.create_check_run(
        "jeryu",
        "jeryu",
        CreateCheckRunRequest {
            name: "jeryu/github-mirror".to_string(),
            head_sha: "m2".to_string(),
            conclusion: Some(CheckConclusion::Failure),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
    let state = Arc::new(WebState::new(core));
    let id = repo.id.to_string();

    let denied = repo_jankurai_scores_ingest(
        State(state.clone()),
        authenticated_account("writer"),
        AxumPath(id.clone()),
        axum::body::Bytes::from_static(
            br#"{"branch":"main","commit_sha":"abc","score":100,"hard_findings":0,"decision":"scored","caps_applied":[]}"#,
        ),
    )
    .await
    .into_response();
    assert_eq!(denied.status(), axum::http::StatusCode::FORBIDDEN);
    assert!(
        state
            .github
            .core()
            .list_jankurai_scores("jeryu", "jeryu", None, None)
            .unwrap()
            .is_empty(),
        "a denied writer submission must not create score state"
    );

    // Ingest: a scored run on main.
    let created = repo_jankurai_scores_ingest(
        State(state.clone()),
        authenticated_admin_account("jeryu-admin"),
        AxumPath(id.clone()),
        axum::body::Bytes::from_static(
            br#"{"branch":"main","commit_sha":"abc","score":92,"hard_findings":0,"decision":"scored","caps_applied":[]}"#,
        ),
    )
    .await
    .into_response();
    assert_eq!(created.status(), axum::http::StatusCode::CREATED);

    // The backfill probe shape: GET ?sha= returns {"scores": [...]}.
    let listed = response_json(
        repo_jankurai_scores_list(
            State(state.clone()),
            AxumPath("jeryu/jeryu".to_string()),
            Query(crate::web::repositories::ScoreListQuery {
                branch: None,
                sha: Some("abc".to_string()),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(listed["scores"].as_array().unwrap().len(), 1);
    assert_eq!(listed["scores"][0]["score"], 92);

    // Summary join + mirror posture.
    let repos = repo_list_response(&state);
    let summary = &repos.repositories[0];
    assert_eq!(summary.jankurai_score, Some(92));
    assert_eq!(summary.jankurai_decision.as_deref(), Some("scored"));
    assert!(summary.jankurai_scored_at.is_some());
    let mirror = summary.mirror.as_ref().expect("mirror reported");
    assert!(mirror.configured);
    assert!(!mirror.last_attempt_ok, "newest mirror run failed");
    assert_eq!(mirror.last_attempt_conclusion.as_deref(), Some("failure"));
    assert!(
        mirror.last_success_at.is_some(),
        "old success still visible"
    );
    assert_eq!(
        summary.failing_checks, 0,
        "mirror failures are not repo ill-health"
    );
    assert_eq!(summary.health, "healthy");

    // Tool-failed ingest: null score + decision surfaces, badge score stays None.
    let failed = repo_jankurai_scores_ingest(
        State(state.clone()),
        authenticated_admin_account("jeryu-admin"),
        AxumPath(id.clone()),
        axum::body::Bytes::from_static(
            br#"{"branch":"main","commit_sha":"zzz","score":null,"decision":"tool-failed","tool_exit":2}"#,
        ),
    )
    .await
    .into_response();
    assert_eq!(failed.status(), axum::http::StatusCode::CREATED);
    let summary = &repo_list_response(&state).repositories[0];
    assert_eq!(summary.jankurai_score, None);
    assert_eq!(summary.jankurai_decision.as_deref(), Some("tool-failed"));

    // Garbage and unknown repos are rejected cleanly.
    let bad = repo_jankurai_scores_ingest(
        State(state.clone()),
        authenticated_admin_account("jeryu-admin"),
        AxumPath(id),
        axum::body::Bytes::from_static(br#"{"branch":"main"}"#),
    )
    .await
    .into_response();
    assert_eq!(bad.status(), axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    let missing = repo_jankurai_scores_ingest(
        State(state),
        authenticated_admin_account("jeryu-admin"),
        AxumPath("jeryu/missing".to_string()),
        axum::body::Bytes::from_static(
            br#"{"branch":"main","commit_sha":"a","decision":"scored"}"#,
        ),
    )
    .await
    .into_response();
    assert_eq!(missing.status(), axum::http::StatusCode::NOT_FOUND);
}

/// GET /api/v1/repos must apply the SPA's filters SERVER-SIDE — the family
/// drill-down page is nothing but `?family=`, and it shipped against a
/// handler that ignored every query parameter (the e2e mock honoured the
/// filter, masking the gap). This drives the real handler through Query
/// extraction so a mock can never hide it again.
#[tokio::test]
async fn repo_list_filters_apply_server_side() {
    let core = ForgeCore::new();
    for (name, family) in [
        ("jmcp-core", Some("jmcp-split")),
        ("jmcp-web", Some("jmcp-split")),
        ("veox-nht", Some("veox-split")),
        ("openQG", None),
    ] {
        core.create_repository(
            "jeryu",
            CreateRepositoryRequest {
                name: name.to_string(),
                private: true,
                description: Some(format!("{name} repository")),
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
        if let Some(family) = family {
            core.set_repository_family("jeryu", name, Some(family.to_string()))
                .unwrap();
        }
    }
    let state = Arc::new(WebState::new(core));
    let account = Extension(AccountSummary {
        login: "jeryu-admin".to_string(),
        display_name: "Jeryu Admin".to_string(),
        role: jeryu_core::UserRole::Admin,
        status: AccountStatus::Active,
        auth_epoch: 0,
        must_change_password: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    });

    let family_only = repo_page(
        repos(
            State(state.clone()),
            account.clone(),
            Query(crate::web::repositories::RepoListQuery {
                family: Some("jmcp-split".to_string()),
                ..Default::default()
            }),
        )
        .await,
    )
    .await;
    let names: Vec<&str> = family_only
        .repositories
        .iter()
        .map(|repo| repo.id.name.as_str())
        .collect();
    assert_eq!(
        family_only.total, 2,
        "only the family members may be listed"
    );
    assert!(names.contains(&"jmcp-core") && names.contains(&"jmcp-web"));
    // Facets keep the full picture so the filter chips stay populated.
    assert_eq!(
        family_only.facets.families,
        vec!["jmcp".to_string(), "veox".to_string()]
    );

    let searched = repo_page(
        repos(
            State(state.clone()),
            account.clone(),
            Query(crate::web::repositories::RepoListQuery {
                q: Some("veox".to_string()),
                ..Default::default()
            }),
        )
        .await,
    )
    .await;
    assert_eq!(searched.total, 1);
    assert_eq!(searched.repositories[0].id.name, "veox-nht");

    let sorted = repo_page(
        repos(
            State(state.clone()),
            account.clone(),
            Query(crate::web::repositories::RepoListQuery {
                sort: Some("name".to_string()),
                ..Default::default()
            }),
        )
        .await,
    )
    .await;
    let sorted_names: Vec<&str> = sorted
        .repositories
        .iter()
        .map(|repo| repo.id.name.as_str())
        .collect();
    assert_eq!(
        sorted_names,
        vec!["jmcp-core", "jmcp-web", "openQG", "veox-nht"]
    );

    // Archived repos are excluded by default and exclusive under ?archived=1.
    let archived = repo_page(
        repos(
            State(state),
            account,
            Query(crate::web::repositories::RepoListQuery {
                archived: Some("1".to_string()),
                ..Default::default()
            }),
        )
        .await,
    )
    .await;
    assert_eq!(archived.total, 0, "no archived repos exist in this fixture");
}

/// The Repos table shows the last push when there is one, so the summary
/// must carry `pushed_at` and the default ("recent_activity") sort must rank
/// by it; a repo whose metadata was touched later but never pushed still
/// falls back to `updated_at`.
#[tokio::test]
async fn repo_list_reports_pushed_at_and_sorts_activity_by_it() {
    let core = ForgeCore::new();
    for name in ["pushed-old", "pushed-new", "never-pushed"] {
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
    let old_push = chrono::Utc::now() - chrono::Duration::days(3);
    let new_push = chrono::Utc::now() - chrono::Duration::days(1);
    core.record_repository_push("jeryu", "pushed-old", old_push)
        .unwrap();
    core.record_repository_push("jeryu", "pushed-new", new_push)
        .unwrap();

    let state = Arc::new(WebState::new(core));
    let account = Extension(AccountSummary {
        login: "jeryu-admin".to_string(),
        display_name: "Jeryu Admin".to_string(),
        role: jeryu_core::UserRole::Admin,
        status: AccountStatus::Active,
        auth_epoch: 0,
        must_change_password: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    });

    let listed = repo_page(
        repos(
            State(state),
            account,
            Query(crate::web::repositories::RepoListQuery::default()),
        )
        .await,
    )
    .await;
    let by_name = |name: &str| {
        listed
            .repositories
            .iter()
            .find(|repo| repo.id.name == name)
            .unwrap_or_else(|| panic!("{name} must be listed"))
            .clone()
    };
    assert_eq!(
        by_name("pushed-new").pushed_at,
        Some(new_push.to_rfc3339()),
        "the recorded push time must reach the SPA"
    );
    assert_eq!(
        by_name("never-pushed").pushed_at,
        None,
        "a repo with no push keeps pushed_at null"
    );

    let order: Vec<&str> = listed
        .repositories
        .iter()
        .map(|repo| repo.id.name.as_str())
        .collect();
    // never-pushed was created last, so its updated_at is the newest key.
    assert_eq!(order, vec!["never-pushed", "pushed-new", "pushed-old"]);
}

/// A red main is the repository's own breakage: the summary the repository
/// page and the listing read says `failing` even though no check run on the
/// head does, and the next green head clears it.
#[test]
fn a_red_main_makes_the_repository_summary_failing() {
    let core = ForgeCore::new();
    core.create_repository(
        "acme",
        CreateRepositoryRequest {
            name: "widget".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let state = WebState::new(core);
    assert_eq!(repo_list_response(&state).repositories[0].health, "healthy");

    let verdict = |sha: &str, outcome: &str| {
        crate::web::pipeline::record(
            &state,
            "forge",
            crate::web::pipeline::NewEvent {
                repo: Some("acme/widget".to_string()),
                sha: Some(sha.to_string()),
                outcome: Some(outcome.to_string()),
                ..crate::web::pipeline::NewEvent::forge("main.status", "gate verdict on main")
            },
        )
        .unwrap();
    };

    verdict("bbb2222", "red");
    let listed = &repo_list_response(&state).repositories[0];
    assert_eq!(listed.health, "failing");
    assert_eq!(
        listed.failing_checks, 0,
        "the gate reports through events, not a check run on the head"
    );

    verdict("ccc3333", "green");
    assert_eq!(
        repo_list_response(&state).repositories[0].health,
        "healthy",
        "the next green head ends the red interval"
    );
}

/// A fast-forward merge moves the default branch onto a commit that was
/// already scored under the pull request's topic branch, and the push audit
/// does not re-score a commit it already covered. The list badge must follow
/// the default branch's HEAD commit, not the branch name the score was filed
/// under, or every fast-forward merge leaves a stale score on /repos.
#[test]
fn repo_list_score_follows_the_default_branch_head_after_a_fast_forward() {
    let core = ForgeCore::new();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let storage = tempdir().expect("git storage dir");
    let bare = storage.path().join("jeryu").join("jeryu.git");
    std::fs::create_dir_all(&bare).expect("create bare dir");
    let git = |args: &[&str], cwd: &Path| git_in(cwd, args);
    git(&["init", "--quiet", "--bare", "-b", "main"], &bare);
    let work = storage.path().join("ff-work");
    std::fs::create_dir_all(&work).expect("create work dir");
    git(&["init", "--quiet", "-b", "topic"], &work);
    std::fs::write(work.join("README.md"), "ff\n").expect("write file");
    git(&["add", "README.md"], &work);
    git(&["commit", "--quiet", "-m", "topic work"], &work);
    let head = git(&["rev-parse", "HEAD"], &work);
    git(
        &[
            "push",
            "--quiet",
            bare.to_str().expect("bare utf8"),
            "topic",
        ],
        &work,
    );

    // An older audit of main, then the topic-branch audit of the head that is
    // about to become main.
    core.record_jankurai_score(
        "jeryu",
        "jeryu",
        jeryu_core::RecordJankuraiScoreRequest {
            branch: "main".to_string(),
            commit_sha: "older-main-sha".to_string(),
            score: Some(76),
            decision: "scored".to_string(),
            ..jeryu_core::RecordJankuraiScoreRequest::default()
        },
    )
    .unwrap();
    core.record_jankurai_score(
        "jeryu",
        "jeryu",
        jeryu_core::RecordJankuraiScoreRequest {
            branch: "topic".to_string(),
            commit_sha: head.clone(),
            score: Some(88),
            decision: "scored".to_string(),
            ..jeryu_core::RecordJankuraiScoreRequest::default()
        },
    )
    .unwrap();

    let state = WebState::new_with_git_storage(core.clone(), storage.path().to_path_buf());
    // Before the merge main still points nowhere: the default-branch score wins.
    assert_eq!(
        repo_list_response(&state).repositories[0].jankurai_score,
        Some(76),
        "with no main head the newest default-branch score is the fallback"
    );

    // Fast-forward main onto the scored head. No second audit is recorded.
    git(&["update-ref", "refs/heads/main", &head], &bare);
    let summary = &repo_list_response(&state).repositories[0];
    assert_eq!(summary.jankurai_score, Some(88));
    assert_eq!(summary.jankurai_decision.as_deref(), Some("scored"));
    assert_eq!(
        core.list_jankurai_scores("jeryu", "jeryu", None, Some(&head))
            .unwrap()
            .len(),
        1,
        "the covered head keeps its single audit record"
    );
}

/// With a protection rule to read, only the contexts it requires count as
/// failing; every other current check is reported apart as advisory, failing
/// and passing side by side.
#[test]
fn repo_summary_counts_required_checks_and_reports_advisory_apart() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.set_branch_protection(
        "alice",
        "jeryu",
        "main",
        SetBranchProtectionRequest {
            required_status_checks: vec!["jeryu/ci".to_string()],
            ..SetBranchProtectionRequest::default()
        },
    )
    .unwrap();
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "feature".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("live-sha".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    for (name, conclusion) in [
        ("jeryu/ci", CheckConclusion::Failure),
        ("jeryu/spell", CheckConclusion::Failure),
        ("jeryu/typos", CheckConclusion::Failure),
        ("jeryu/docs", CheckConclusion::Success),
        // Mirror bookkeeping is on neither side of the count.
        ("jeryu/github-mirror", CheckConclusion::Failure),
    ] {
        core.create_check_run(
            "alice",
            "jeryu",
            CreateCheckRunRequest {
                name: name.to_string(),
                head_sha: "live-sha".to_string(),
                conclusion: Some(conclusion),
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap();
    }

    let state = WebState::new(core);
    let summary = &repo_list_response(&state).repositories[0];
    assert_eq!(
        summary.failing_checks, 1,
        "only the required jeryu/ci failure counts as failing"
    );
    let advisory = summary
        .advisory_checks
        .as_ref()
        .expect("a readable rule tells advisory from required");
    assert_eq!(advisory.failing, 2, "jeryu/spell and jeryu/typos");
    assert_eq!(advisory.passing, 1, "jeryu/docs");
    assert_eq!(summary.health, "warning");
}

/// A repository whose required checks all pass is healthy even while advisory
/// checks fail: the advisory failures are still reported, apart.
#[test]
fn repo_summary_stays_healthy_when_only_advisory_checks_fail() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.set_branch_protection(
        "alice",
        "jeryu",
        "main",
        SetBranchProtectionRequest {
            required_status_checks: vec!["jeryu/ci".to_string()],
            ..SetBranchProtectionRequest::default()
        },
    )
    .unwrap();
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "feature".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("live-sha".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    for (name, conclusion) in [
        ("jeryu/ci", CheckConclusion::Success),
        ("jeryu/spell", CheckConclusion::Failure),
    ] {
        core.create_check_run(
            "alice",
            "jeryu",
            CreateCheckRunRequest {
                name: name.to_string(),
                head_sha: "live-sha".to_string(),
                conclusion: Some(conclusion),
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap();
    }

    let state = WebState::new(core);
    let summary = &repo_list_response(&state).repositories[0];
    assert_eq!(summary.failing_checks, 0);
    assert_eq!(summary.health, "healthy");
    let advisory = summary.advisory_checks.as_ref().expect("advisory reported");
    assert_eq!((advisory.failing, advisory.passing), (1, 0));
}

/// `jankurai/proof` is required when the rule insists on it even though it is
/// not listed as a status-check context.
#[test]
fn repo_summary_treats_required_jankurai_proof_as_required() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.set_branch_protection(
        "alice",
        "jeryu",
        "main",
        SetBranchProtectionRequest {
            require_jankurai_proof: true,
            ..SetBranchProtectionRequest::default()
        },
    )
    .unwrap();
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "feature".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("live-sha".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: "jankurai/proof".to_string(),
            head_sha: "live-sha".to_string(),
            conclusion: Some(CheckConclusion::Failure),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();

    let state = WebState::new(core);
    let summary = &repo_list_response(&state).repositories[0];
    assert_eq!(summary.failing_checks, 1);
    let advisory = summary.advisory_checks.as_ref().expect("advisory reported");
    assert_eq!((advisory.failing, advisory.passing), (0, 0));
}

/// No protection rule means nothing tells required from advisory: every
/// current check keeps counting as failing and nothing is reported apart.
#[test]
fn repo_summary_counts_every_check_without_a_protection_rule() {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "feature".to_string(),
            head: "feature".to_string(),
            base: "main".to_string(),
            head_sha: Some("live-sha".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    for name in ["jeryu/ci", "jeryu/spell"] {
        core.create_check_run(
            "alice",
            "jeryu",
            CreateCheckRunRequest {
                name: name.to_string(),
                head_sha: "live-sha".to_string(),
                conclusion: Some(CheckConclusion::Failure),
                ..CreateCheckRunRequest::default()
            },
        )
        .unwrap();
    }

    let state = WebState::new(core);
    let summary = &repo_list_response(&state).repositories[0];
    assert_eq!(summary.failing_checks, 2);
    assert_eq!(summary.advisory_checks, None);
    assert_eq!(summary.health, "warning");
}

/// A repository with one open pull request on `base` at `live-sha`, `main` as
/// its default branch, and `rule_contexts` required on `base`.
fn repo_with_pull_on(base: &str, rule_contexts: &[&str]) -> ForgeCore {
    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: true,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    core.set_branch_protection(
        "alice",
        "jeryu",
        base,
        SetBranchProtectionRequest {
            required_status_checks: rule_contexts.iter().map(|name| name.to_string()).collect(),
            ..SetBranchProtectionRequest::default()
        },
    )
    .unwrap();
    core.create_pull_request(
        "alice",
        "jeryu",
        "alice",
        CreatePullRequestRequest {
            title: "feature".to_string(),
            head: "feature".to_string(),
            base: base.to_string(),
            head_sha: Some("live-sha".to_string()),
            ..CreatePullRequestRequest::default()
        },
    )
    .unwrap();
    core
}

fn status(core: &ForgeCore, sha: &str, context: &str, state: CommitStatusState) {
    core.create_commit_status(
        "alice",
        "jeryu",
        sha,
        "gate-runner",
        CreateCommitStatusRequest {
            base: None,
            state,
            context: context.to_string(),
            description: None,
            target_url: None,
        },
    )
    .unwrap();
}

fn check(core: &ForgeCore, sha: &str, name: &str, conclusion: CheckConclusion) {
    core.create_check_run(
        "alice",
        "jeryu",
        CreateCheckRunRequest {
            name: name.to_string(),
            head_sha: sha.to_string(),
            conclusion: Some(conclusion),
            ..CreateCheckRunRequest::default()
        },
    )
    .unwrap();
}

/// Required contexts arrive as commit statuses in this deployment
/// (`<repo>/required` from the gate runner). They resolve the way merge
/// posture resolves them: the newest status first, else a check run.
#[test]
fn repo_summary_counts_required_contexts_delivered_as_commit_statuses() {
    let core = repo_with_pull_on("main", &["jeryu/required"]);
    status(
        &core,
        "live-sha",
        "jeryu/required",
        CommitStatusState::Failure,
    );
    check(&core, "live-sha", "jeryu/lint", CheckConclusion::Failure);
    let state = WebState::new(core);
    let summary = &repo_list_response(&state).repositories[0];
    assert_eq!(
        summary.failing_checks, 1,
        "the failing required status counts"
    );
    assert_eq!(summary.health, "warning");
    let advisory = summary.advisory_checks.as_ref().expect("advisory reported");
    assert_eq!((advisory.failing, advisory.passing), (1, 0), "jeryu/lint");

    // The newest status decides: a later green clears it.
    let core = repo_with_pull_on("main", &["jeryu/required"]);
    status(
        &core,
        "live-sha",
        "jeryu/required",
        CommitStatusState::Error,
    );
    status(
        &core,
        "live-sha",
        "jeryu/required",
        CommitStatusState::Success,
    );
    let state = WebState::new(core);
    let summary = &repo_list_response(&state).repositories[0];
    assert_eq!(summary.failing_checks, 0);
    assert_eq!(summary.health, "healthy");

    // A status wins over a check run of the same name, as in posture.
    let core = repo_with_pull_on("main", &["jeryu/required"]);
    check(
        &core,
        "live-sha",
        "jeryu/required",
        CheckConclusion::Failure,
    );
    status(
        &core,
        "live-sha",
        "jeryu/required",
        CommitStatusState::Success,
    );
    let state = WebState::new(core);
    assert_eq!(repo_list_response(&state).repositories[0].failing_checks, 0);

    // With no status, the check run of that name decides.
    let core = repo_with_pull_on("main", &["jeryu/required"]);
    check(
        &core,
        "live-sha",
        "jeryu/required",
        CheckConclusion::Failure,
    );
    let state = WebState::new(core);
    assert_eq!(repo_list_response(&state).repositories[0].failing_checks, 1);
}

/// A pull request is judged by its own base's rule, not the default branch's.
#[test]
fn repo_summary_judges_each_pull_request_by_its_own_base_rule() {
    let core = repo_with_pull_on("release", &["jeryu/release-gate"]);
    core.set_branch_protection(
        "alice",
        "jeryu",
        "main",
        SetBranchProtectionRequest {
            required_status_checks: vec!["jeryu/required".to_string()],
            ..SetBranchProtectionRequest::default()
        },
    )
    .unwrap();
    // Failing on the release PR: main's required context is advisory there,
    // the release rule's own context is required.
    check(
        &core,
        "live-sha",
        "jeryu/required",
        CheckConclusion::Failure,
    );
    status(
        &core,
        "live-sha",
        "jeryu/release-gate",
        CommitStatusState::Failure,
    );
    let state = WebState::new(core);
    let summary = &repo_list_response(&state).repositories[0];
    assert_eq!(
        summary.failing_checks, 1,
        "only jeryu/release-gate is required here"
    );
    let advisory = summary.advisory_checks.as_ref().expect("advisory reported");
    assert_eq!(
        (advisory.failing, advisory.passing),
        (1, 0),
        "jeryu/required"
    );
}

/// Silence is the rule's own: the site-wide audit gate adds `jankurai/proof`
/// to a rule that requires something, and never turns a silent rule into one
/// that hides every other failure as advisory.
#[test]
fn the_site_audit_gate_does_not_make_a_silent_rule_speak() {
    let core = repo_with_pull_on("main", &[]);
    let silent = core
        .get_branch_protection("alice", "jeryu", "main")
        .unwrap();
    assert_eq!(
        crate::web::repositories::required_from_rule(&silent, true),
        None
    );
    assert_eq!(
        crate::web::repositories::required_from_rule(&silent, false),
        None
    );

    let core = repo_with_pull_on("main", &["jeryu/required", "jeryu/github-mirror"]);
    let rule = core
        .get_branch_protection("alice", "jeryu", "main")
        .unwrap();
    let gated = crate::web::repositories::required_from_rule(&rule, true).unwrap();
    assert_eq!(
        gated.into_iter().collect::<Vec<_>>(),
        vec!["jankurai/proof".to_string(), "jeryu/required".to_string()]
    );
    let plain = crate::web::repositories::required_from_rule(&rule, false).unwrap();
    assert_eq!(
        plain.into_iter().collect::<Vec<_>>(),
        vec!["jeryu/required".to_string()]
    );

    // A silent rule falls back to counting every failing current check.
    let core = repo_with_pull_on("main", &[]);
    check(&core, "live-sha", "jeryu/lint", CheckConclusion::Failure);
    check(
        &core,
        "live-sha",
        "jankurai/proof",
        CheckConclusion::Failure,
    );
    let state = WebState::new(core);
    let summary = &repo_list_response(&state).repositories[0];
    assert_eq!(summary.failing_checks, 2);
    assert_eq!(summary.advisory_checks, None);
}
