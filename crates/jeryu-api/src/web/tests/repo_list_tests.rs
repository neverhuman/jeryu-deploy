use super::*;

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
    assert_eq!(portal.family.as_deref(), Some("jeryu-split"));
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
    assert_eq!(core_repo.family.as_deref(), Some("jeryu-split"));
    assert_eq!(core_repo.repo_role, Some(RepositoryRole::SplitMember));

    let unrelated = repos
        .repositories
        .iter()
        .find(|repo| repo.id.owner == "alice" && repo.id.name == "unrelated")
        .expect("unrelated repo");
    assert_eq!(unrelated.family, None);
    assert_eq!(unrelated.repo_role, None);
    assert_eq!(repos.facets.families, vec!["jeryu-split".to_string()]);
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
        vec!["jekko-split".to_string(), "jeryu-split".to_string()]
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

    let family_only = repos(
        State(state.clone()),
        account.clone(),
        Query(crate::web::repositories::RepoListQuery {
            family: Some("jmcp-split".to_string()),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .0
    .list;
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
        vec!["jmcp-split".to_string(), "veox-split".to_string()]
    );

    let searched = repos(
        State(state.clone()),
        account.clone(),
        Query(crate::web::repositories::RepoListQuery {
            q: Some("veox".to_string()),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .0
    .list;
    assert_eq!(searched.total, 1);
    assert_eq!(searched.repositories[0].id.name, "veox-nht");

    let sorted = repos(
        State(state.clone()),
        account.clone(),
        Query(crate::web::repositories::RepoListQuery {
            sort: Some("name".to_string()),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .0
    .list;
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
    let archived = repos(
        State(state),
        account,
        Query(crate::web::repositories::RepoListQuery {
            archived: Some("1".to_string()),
            ..Default::default()
        }),
    )
    .await
    .unwrap()
    .0
    .list;
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

    let listed = repos(
        State(state),
        account,
        Query(crate::web::repositories::RepoListQuery::default()),
    )
    .await
    .unwrap()
    .0
    .list;
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
