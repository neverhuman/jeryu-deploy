use super::*;

/// A forge with two bare repos in one family, the first carrying the manifest
/// that names them both. Returns the state and the storage tempdir (which must
/// outlive it).
fn hosted_family_state(private: bool) -> (Arc<WebState>, tempfile::TempDir) {
    let core = ForgeCore::new();
    for name in ["jeryu-deploy", "jeryu-cache"] {
        core.create_repository(
            "jeryu",
            CreateRepositoryRequest {
                name: name.to_string(),
                private,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    }
    let storage = tempdir().expect("git storage dir");
    build_bare_repo_with_files(
        storage.path(),
        "jeryu",
        "jeryu-deploy",
        &[
            ("repos.manifest.toml", HOSTED_FAMILY_AUTHORITY),
            ("src/lib.rs", SHARED_TOOL_FIXTURE),
        ],
    );
    build_bare_repo_with_files(
        storage.path(),
        "jeryu",
        "jeryu-cache",
        &[("src/lib.rs", SHARED_TOOL_FIXTURE)],
    );
    let state = Arc::new(WebState::with_repo_manager(
        core,
        Arc::new(RepoManager::new(GitdConfig::new(
            storage.path().to_path_buf(),
        ))),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/web/dist"),
        std::env::temp_dir(),
        SplitCatalog::load(&[]),
    ));
    (state, storage)
}

/// The authority manifest the first fixture repo carries, in the shape the
/// family control plane publishes: `required_repos` names every member, the
/// control plane sits under `[control_plane]`, and every other member is one
/// `[[repo]]` row.
const HOSTED_FAMILY_AUTHORITY: &str = r#"
schema_version = "1"
repo_family = "jeryu-split"
required_repos = ["jeryu-cache", "jeryu-deploy"]

[control_plane]
name = "jeryu-deploy"
default_branch = "main"
identity_status = "bound"

[[repo]]
name = "jeryu-cache"
jeryu_slug = "jeryu/jeryu-cache"
default_branch = "main"
identity_status = "pending"
"#;

/// The duplicated body both fixture repos carry, so a cross-repo cluster is
/// there to be found once the bare repos are materialized.
const SHARED_TOOL_FIXTURE: &str = r#"
pub fn retry_remote_call(input: &str) -> Result<String, String> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let response = call_remote(input);
        if response.is_ok() {
            return response;
        }
        if attempts > 3 {
            return Err("failed".to_string());
        }
    }
}
"#;

/// A bare repository holding one commit of `files` on `main`, laid out where
/// the forge's `RepoManager` resolves it.
fn build_bare_repo_with_files(
    storage_root: &std::path::Path,
    owner: &str,
    repo: &str,
    files: &[(&str, &str)],
) -> String {
    let bare = storage_root.join(owner).join(format!("{repo}.git"));
    std::fs::create_dir_all(bare.parent().expect("bare parent")).expect("create owner dir");
    let work = storage_root.join(format!("{owner}-{repo}-source-work"));
    std::fs::create_dir_all(&work).expect("create work dir");

    let git = |args: &[&str], cwd: &std::path::Path| {
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
    };

    git(&["init", "--quiet", "-b", "main"], &work);
    for (rel, contents) in files {
        write_file(&work, rel, contents);
        git(&["add", rel], &work);
    }
    git(&["commit", "--quiet", "-m", "seed"], &work);
    let head = git(&["rev-parse", "HEAD"], &work);
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
    head
}

#[tokio::test]
async fn codegraph_query_route_returns_impact_pack() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    let state = Arc::new(WebState::new(core));
    let snapshot = GraphSnapshot {
        symbols: vec![SymbolRow {
            crate_name: "jeryu-codegraph".to_string(),
            file: "crates/jeryu-codegraph/src/lib.rs".to_string(),
            symbol: "CodeGraph".to_string(),
            kind: "public".to_string(),
            is_public: true,
            line: 7,
        }],
        crate_deps: vec![CrateDepRow {
            crate_name: "jeryu-mcp".to_string(),
            depends_on: "jeryu-codegraph".to_string(),
        }],
        symbol_refs: vec![SymbolRefRow {
            crate_name: "jeryu-codegraph".to_string(),
            file: "crates/jeryu-codegraph/src/lib.rs".to_string(),
            symbol: "CodeGraph".to_string(),
            ref_file: "crates/jeryu-mcp/src/backend/memory.rs".to_string(),
            ref_line: 12,
            ref_kind: "type".to_string(),
        }],
        ..Default::default()
    };
    state.codegraph_store.persist(&snapshot).unwrap();

    let response = crate::web::codegraph::query(
        State(state),
        AxumPath(repo.id.to_string()),
        axum::body::Bytes::from(
            serde_json::json!({
                "changed_paths": ["crates/jeryu-codegraph/src/lib.rs"],
                "symbol": "CodeGraph",
                "crate_name": "jeryu-codegraph"
            })
            .to_string(),
        ),
    )
    .await;
    let pack = response_json(response).await;
    assert_eq!(pack["schema_version"], "codegraph.query/v1");
    assert_eq!(pack["provenance"]["storage_schema"], "5");
    assert_eq!(pack["definition"]["symbol"], "CodeGraph");
    assert_eq!(
        pack["references"][0]["ref_file"],
        "crates/jeryu-mcp/src/backend/memory.rs"
    );
    assert_eq!(pack["reverse_deps"], serde_json::json!(["jeryu-mcp"]));
    assert!(
        pack["proof_lanes"][0]
            .as_str()
            .unwrap()
            .contains("jeryu-codegraph")
    );
}

#[tokio::test]
async fn codegraph_query_route_errors_are_typed() {
    let core = ForgeCore::new();
    let repo = core
        .create_repository(
            "alice",
            CreateRepositoryRequest {
                name: "jeryu".to_string(),
                private: false,
                description: None,
                default_branch: Some("main".to_string()),
            },
        )
        .unwrap();
    let state = Arc::new(WebState::new(core));

    let missing = response_json(
        crate::web::codegraph::query(
            State(state.clone()),
            AxumPath("repo-missing".to_string()),
            axum::body::Bytes::from("{}"),
        )
        .await,
    )
    .await;
    assert_eq!(missing["code"], "not_found");
    assert_eq!(missing["purpose"], "query repository codegraph");
    assert!(
        missing["common_fixes"]
            .as_array()
            .expect("common fixes")
            .len()
            >= 2
    );

    let invalid = response_json(
        crate::web::codegraph::query(
            State(state),
            AxumPath(repo.id.to_string()),
            axum::body::Bytes::from("{"),
        )
        .await,
    )
    .await;
    assert_eq!(invalid["code"], "codegraph_invalid_request");
    assert_eq!(invalid["docs_url"], "docs/errors.md#not-found");
    assert!(
        invalid["repair_hint"]
            .as_str()
            .expect("repair hint")
            .contains("codegraph API proof lane")
    );
}

#[tokio::test]
async fn tool_build_routes_return_clusters_and_record_feedback() {
    let core = ForgeCore::new();
    let state = Arc::new(WebState::new(core));
    let root = tempdir().expect("tool-build fixture");
    let repeated = r#"
pub fn alpha(input: &str) -> Result<String, String> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let response = call_remote(input);
        if response.is_ok() {
            return response;
        }
        if attempts > 3 {
            return Err("failed".to_string());
        }
    }
}
"#;
    write_file(root.path(), "crates/a/src/lib.rs", repeated);
    write_file(
        root.path(),
        "crates/b/src/lib.rs",
        &repeated.replace("alpha", "beta"),
    );
    let report = scan_tool_build_clusters(
        root.path(),
        "alice/jeryu",
        "commit-a",
        ToolBuildScanConfig {
            window_lines: 5,
            min_normalized_tokens: 12,
            min_occurrences: 2,
            max_file_bytes: 64 * 1024,
            max_clusters: 10,
            min_repo_count: 1,
        },
    )
    .unwrap();
    assert!(!report.clusters.is_empty());
    state
        .codegraph_store
        .persist_tool_build_report(&report)
        .unwrap();

    let status = response_json(
        crate::web::tool_build::status(
            State(state.clone()),
            Query(crate::web::tool_build::ToolBuildQuery {
                repo: Some("alice/jeryu".to_string()),
                limit: None,
                include_ignored: false,
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status["schema_version"], "codegraph.tool_build/v1");
    assert!(status["cluster_count"].as_u64().unwrap() > 0);

    let clusters = response_json(
        crate::web::tool_build::clusters(
            State(state.clone()),
            Query(crate::web::tool_build::ToolBuildQuery {
                repo: Some("alice/jeryu".to_string()),
                limit: Some(10),
                include_ignored: false,
            }),
        )
        .await,
    )
    .await;
    let cluster_id = clusters["clusters"][0]["cluster_id"]
        .as_str()
        .expect("cluster id")
        .to_string();
    assert!(
        clusters["clusters"][0]["insight"]
            .as_str()
            .unwrap()
            .contains("normalized window")
    );

    let invalid = response_json(
        crate::web::tool_build::feedback(
            State(state.clone()),
            AxumPath(cluster_id.clone()),
            axum::body::Bytes::from(r#"{"reason":""}"#),
        )
        .await,
    )
    .await;
    assert_eq!(invalid["code"], "tool_build_feedback_reason_required");
    assert_eq!(invalid["docs_url"], "docs/codegraph-tool-build.md");

    let feedback = response_json(
        crate::web::tool_build::feedback(
            State(state.clone()),
            AxumPath(cluster_id.clone()),
            axum::body::Bytes::from(r#"{"reason":"fixture boilerplate","ignored_by":"test"}"#),
        )
        .await,
    )
    .await;
    assert_eq!(feedback["cluster_id"], cluster_id);
    assert_eq!(feedback["reason"], "fixture boilerplate");

    let suppressed = response_json(
        crate::web::tool_build::clusters(
            State(state),
            Query(crate::web::tool_build::ToolBuildQuery {
                repo: Some("alice/jeryu".to_string()),
                limit: Some(10),
                include_ignored: false,
            }),
        )
        .await,
    )
    .await;
    assert!(
        suppressed["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .all(|cluster| cluster["cluster_id"] != cluster_id)
    );
}

/// The live `/api/v1/ecosystem` route returns the camelCase tool-graph with
/// real catalog data through the mounted router.
#[tokio::test]
async fn ecosystem_route_serves_live_tool_graph() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let core = ForgeCore::new();
    core.create_repository(
        "alice",
        CreateRepositoryRequest {
            name: "jeryu".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let response = app(
        WebState::new(core),
        std::path::Path::new("/tmp/jeryu-no-spa"),
    )
    .oneshot(
        Request::builder()
            .uri("/api/v1/ecosystem")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let parsed = response_json(response).await;
    assert_eq!(parsed["live"], true);
    assert_eq!(parsed["degradedReason"], "");
    let tools = parsed["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), jeryu_mcp::tool_manifest().len());
    // The first node carries the exact camelCase contract keys + live repo.
    let node = &tools[0];
    for key in [
        "name",
        "className",
        "conformance",
        "sideEffects",
        "dataClasses",
        "dependsOn",
    ] {
        assert!(node.get(key).is_some(), "missing contract key: {key}");
    }
    assert_eq!(node["provider"], "jeryu");
    assert_eq!(node["repo"], "alice/jeryu");
}

#[tokio::test]
async fn tool_finder_scan_status_idle_busy_guard_and_snapshot_arm() {
    let core = ForgeCore::new();
    let mut raw_state = WebState::new(core);
    // Neither manifests nor hosted repositories: a scan cannot start, with a
    // typed repair path.
    let state = Arc::new(raw_state.clone());
    let idle = response_json(
        crate::web::tool_finder::scan_status(State(state.clone()))
            .await
            .into_response(),
    )
    .await;
    assert_eq!(idle["phase"], "idle");
    assert_eq!(idle["running"], false);
    let not_configured =
        response_json(crate::web::tool_finder::scan_start(State(state.clone())).await).await;
    assert_eq!(not_configured["code"], "tool_finder_not_configured");

    // With manifests wired and the single flight already claimed, POST is a
    // 409 with the running snapshot's repair hints.
    raw_state.split_manifests = vec![PathBuf::from("/tmp/fixture-split/repos.manifest.toml")];
    let state = Arc::new(raw_state);
    state
        .tool_finder_scan
        .try_begin()
        .expect("claim the single flight");
    let busy = response_json(crate::web::tool_finder::scan_start(State(state.clone())).await).await;
    assert_eq!(busy["code"], "tool_finder_scan_running");

    // The websocket snapshot arm serves the live status for the scope.
    let event = snapshot_event(&state, "tool_finder.scan").expect("tool_finder.scan snapshot");
    assert_eq!(event.kind, "tool_finder.scan.snapshot");
    assert_eq!(event.entity, "system/host");
    assert_eq!(event.payload["running"], true);
    assert_eq!(event.payload["phase"], "discover");
}

/// The authority manifest names its members in `required_repos` and its
/// control plane in `[control_plane]`, never in a `[[repo]]` row of its own; a
/// release lock names them in `[[repo]]` rows only. Both are read.
#[test]
fn family_member_names_come_from_the_authority_or_the_lock() {
    assert_eq!(
        crate::web::tool_finder::hosted::member_names(HOSTED_FAMILY_AUTHORITY),
        BTreeSet::from(["jeryu-cache".to_string(), "jeryu-deploy".to_string()])
    );
    assert_eq!(
        crate::web::tool_finder::hosted::member_names(
            r#"
[[repo]]
name = "pelago"
github_slug = "pelago-oss/pelago"

[[repo]]
jeryu_slug = "pelago/pelago-core"
"#
        ),
        BTreeSet::from(["pelago".to_string(), "pelago-core".to_string()])
    );
    assert!(crate::web::tool_finder::hosted::member_names("not = [toml").is_empty());
}

/// Discovery reads the family out of the bare repos themselves, materializes
/// both default branches, and the scanner finds the duplicated code in them —
/// the whole point: no working-tree checkout exists anywhere here.
#[tokio::test]
async fn tool_finder_scans_hosted_bare_repos_without_any_checkout() {
    let (state, _storage) = hosted_family_state(false);

    // The source report names the hosted family and calls itself configured.
    let source = response_json(crate::web::tool_finder::source(State(state.clone())).await).await;
    assert_eq!(source["kind"], "hosted");
    assert_eq!(source["configured"], true);
    assert_eq!(source["schema_version"], "jeryu.tool_finder.source/v1");
    let mut named: Vec<&str> = source["repos"]
        .as_array()
        .expect("repos")
        .iter()
        .map(|repo| repo["repo"].as_str().expect("repo id"))
        .collect();
    named.sort_unstable();
    assert_eq!(named, vec!["jeryu/jeryu-cache", "jeryu/jeryu-deploy"]);
    assert_eq!(source["repos"][0]["family"], "jeryu-split");
    assert_eq!(source["repos"][0]["branch"], "main");

    // Materializing writes both trees out of the bare repos.
    let worker_state = state.clone();
    let scan_source = tokio::task::spawn_blocking(move || {
        crate::web::tool_finder::resolve_scan_source(&worker_state)
            .map_err(|error| error.to_string())
            .expect("hosted scan source")
    })
    .await
    .expect("materialize");
    let roots = scan_source.roots().to_vec();
    assert_eq!(roots.len(), 2);
    for (repo, root) in &roots {
        assert!(
            root.join("src/lib.rs").is_file(),
            "{repo} must have its sources on disk"
        );
    }
    let workspace_root = roots[0]
        .1
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();

    // The scanner finds the duplicated body across the two materialized trees.
    let report = jeryu_codegraph::scan_tool_build_family(
        &roots,
        "system/host",
        "hosted-default-branch",
        ToolBuildScanConfig {
            window_lines: 5,
            min_normalized_tokens: 12,
            min_occurrences: 2,
            max_file_bytes: 64 * 1024,
            max_clusters: 10,
            min_repo_count: 2,
        },
    )
    .expect("scan the materialized trees");
    assert!(
        !report.clusters.is_empty(),
        "the duplicated body must cluster across both bare repos"
    );
    let repos: BTreeSet<&str> = report.clusters[0]
        .occurrences
        .iter()
        .map(|occurrence| occurrence.repo_id.as_str())
        .collect();
    assert_eq!(
        repos,
        BTreeSet::from(["jeryu/jeryu-cache", "jeryu/jeryu-deploy"])
    );

    // Dropping the source removes the scratch tree: no disk left behind.
    drop(scan_source);
    assert!(!workspace_root.exists(), "the scratch tree must be removed");
}

/// Private repos are materialized, because findings name their files. That is
/// only sound while every tool-finder route is global-admin-only.
#[tokio::test]
async fn tool_finder_hosted_sources_include_private_repos_behind_admin_only_routes() {
    let (state, _storage) = hosted_family_state(true);
    let source = crate::web::tool_finder::source_payload(&state);
    assert_eq!(source.kind, "hosted");
    assert_eq!(source.repos.len(), 2);
    assert!(
        source.repos.iter().all(|repo| repo.private),
        "the fixture family is private"
    );
    for path in [
        "/api/v1/tool-finder/source",
        "/api/v1/tool-finder/dashboard",
        "/api/v1/tool-finder/scan",
    ] {
        assert!(
            crate::web::auth::admin_only_request(&axum::http::Method::GET, path),
            "{path} must stay admin-only while private sources are scanned"
        );
    }
}

/// A forge with repositories but no family file anywhere says so, instead of
/// failing a scan with a bare "no split-family repos discovered".
#[tokio::test]
async fn tool_finder_source_reports_not_configured_without_a_family_file() {
    let core = ForgeCore::new();
    core.create_repository(
        "jeryu",
        CreateRepositoryRequest {
            name: "standalone".to_string(),
            private: false,
            description: None,
            default_branch: Some("main".to_string()),
        },
    )
    .unwrap();
    let storage = tempdir().expect("git storage dir");
    build_bare_repo_with_files(
        storage.path(),
        "jeryu",
        "standalone",
        &[("src/lib.rs", "pub fn solo() {}\n")],
    );
    let state = Arc::new(WebState::with_repo_manager(
        core,
        Arc::new(RepoManager::new(GitdConfig::new(
            storage.path().to_path_buf(),
        ))),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/web/dist"),
        std::env::temp_dir(),
        SplitCatalog::load(&[]),
    ));

    let source = response_json(crate::web::tool_finder::source(State(state.clone())).await).await;
    assert_eq!(source["kind"], "none");
    assert_eq!(source["configured"], false);
    assert!(
        source["detail"]
            .as_str()
            .expect("detail")
            .contains("no split family is configured on this server"),
        "{source:?}"
    );
    assert!(
        source["skipped"].as_array().expect("skipped").is_empty(),
        "nothing was skipped: nothing named a family"
    );

    // A repository named by a family file but not hosted here is reported as
    // skipped rather than silently dropped.
    let (family_state, _family_storage) = hosted_family_state(false);
    family_state
        .core
        .set_repository_archived("admin", "jeryu", "jeryu-cache", true)
        .expect("archive the member");
    let source = crate::web::tool_finder::source_payload(&family_state);
    assert_eq!(source.repos.len(), 1);
    assert_eq!(source.repos[0].repo, "jeryu/jeryu-deploy");
    assert_eq!(source.skipped.len(), 1);
    assert_eq!(source.skipped[0].repo, "jeryu/jeryu-cache");
    assert_eq!(source.skipped[0].reason, "unreadable");
}

#[tokio::test]
async fn tool_finder_dashboard_and_propose_round_trip() {
    let core = ForgeCore::new();
    let mut raw_state = WebState::new(core);

    // A writable fixture registry so propose can append.
    let registry_dir = tempdir().expect("registry fixture");
    write_file(
        registry_dir.path(),
        "tools-registry.toml",
        "schema_version = \"1\"\n\n[[tool]]\nid = \"existing\"\nname = \"Existing\"\nkind = \"rust-crate\"\nstatus = \"published\"\nadopting_repos = []\ncandidate_repos = []\nloc_saved = 10\nloc_saved_estimate = 0\n",
    );
    raw_state.tool_registry_path = Some(registry_dir.path().join("tools-registry.toml"));
    let state = Arc::new(raw_state);

    // Persist a cross-repo fixture scan under the system repo id.
    let repo_a = tempdir().expect("repo-a");
    let repo_b = tempdir().expect("repo-b");
    let repeated = r#"
pub fn alpha(input: &str) -> Result<String, String> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let response = call_remote(input);
        if response.is_ok() {
            return response;
        }
        if attempts > 3 {
            return Err("failed".to_string());
        }
    }
}
"#;
    write_file(repo_a.path(), "src/lib.rs", repeated);
    write_file(
        repo_b.path(),
        "src/lib.rs",
        &repeated.replace("alpha", "beta"),
    );
    let report = jeryu_codegraph::scan_tool_build_family(
        &[
            ("repo-a".to_string(), repo_a.path().to_path_buf()),
            ("repo-b".to_string(), repo_b.path().to_path_buf()),
        ],
        "system/host",
        "working-tree",
        ToolBuildScanConfig {
            window_lines: 5,
            min_normalized_tokens: 12,
            min_occurrences: 2,
            max_file_bytes: 64 * 1024,
            max_clusters: 10,
            min_repo_count: 2,
        },
    )
    .unwrap();
    assert!(!report.clusters.is_empty());
    state
        .codegraph_store
        .persist_tool_build_report(&report)
        .unwrap();

    // Dashboard: families over the persisted scan, enriched per cluster.
    let dashboard = response_json(
        crate::web::tool_finder::dashboard(
            State(state.clone()),
            Query(crate::web::tool_finder::DashboardQuery {
                limit: Some(50),
                include_ignored: false,
            }),
        )
        .await,
    )
    .await;
    assert!(dashboard["family_count"].as_u64().unwrap() >= 1);
    assert!(dashboard["cluster_count"].as_u64().unwrap() >= 1);
    let family = &dashboard["families"][0];
    let cluster = &family["clusters"][0];
    let cluster_id = cluster["cluster_id"]
        .as_str()
        .expect("cluster id")
        .to_string();
    assert!(cluster["suggested_kind"].as_str().is_some());
    assert!(cluster["anticipated_loc_saved"].as_u64().is_some());
    assert_eq!(cluster["occurrences"][0]["repo_id"], "repo-a");
    let scanned_at = dashboard["scan"]["scanned_at"]
        .as_str()
        .expect("scanned_at");
    assert!(
        chrono::DateTime::parse_from_rfc3339(scanned_at).is_ok(),
        "scanned_at must be RFC 3339, got {scanned_at}"
    );

    // Propose: files a registry entry + build task, idempotent on re-post.
    let receipt = response_json(
        crate::web::tool_finder::propose(
            State(state.clone()),
            AxumPath(cluster_id.clone()),
            axum::body::Bytes::new(),
        )
        .await,
    )
    .await;
    assert_eq!(receipt["created"], true);
    let tool_id = receipt["tool_id"].as_str().expect("tool id").to_string();
    let task_id = receipt["task_id"].as_str().expect("task id").to_string();
    let registry_text =
        std::fs::read_to_string(registry_dir.path().join("tools-registry.toml")).unwrap();
    assert!(registry_text.contains(&format!("origin_cluster = \"{cluster_id}\"")));
    assert!(registry_text.contains("status = \"proposed\""));
    assert!(
        registry_text.starts_with("schema_version"),
        "append preserves header"
    );
    assert!(
        registry_dir
            .path()
            .join("tasks")
            .join(format!("{task_id}-{tool_id}.toml"))
            .is_file()
    );

    let replay = response_json(
        crate::web::tool_finder::propose(
            State(state.clone()),
            AxumPath(cluster_id.clone()),
            axum::body::Bytes::new(),
        )
        .await,
    )
    .await;
    assert_eq!(replay["created"], false);
    assert_eq!(replay["tool_id"], tool_id);

    // Unknown cluster ids get a typed not-found.
    let missing = response_json(
        crate::web::tool_finder::propose(
            State(state.clone()),
            AxumPath("toolbuild-doesnotexist".to_string()),
            axum::body::Bytes::new(),
        )
        .await,
    )
    .await;
    assert_eq!(missing["code"], "tool_finder_cluster_not_found");
}

#[test]
fn scanned_at_is_rfc3339_utc_not_epoch_millis() {
    use crate::web::tool_finder::scanned_at_rfc3339;
    assert_eq!(
        scanned_at_rfc3339("1758412800123").as_deref(),
        Some("2025-09-21T00:00:00.123Z")
    );
    assert_eq!(scanned_at_rfc3339("2026-09-18"), None);
}
