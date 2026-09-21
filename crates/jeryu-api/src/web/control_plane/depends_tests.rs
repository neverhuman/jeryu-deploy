//! `depends_on`: the tag-series rule, then the edges over real bare repos.

use std::path::Path;

use axum::http::Method as HttpMethod;
use jeryu_core::{CreateRepositoryRequest, ForgeCore, UserRole};
use serde_json::Value;
use tower::ServiceExt;

use super::depends::tag_series;
use crate::web::pipeline::tests::{body_json, request};
use crate::web::shift::tests::run_git;
use crate::web::{WebState, app};

#[test]
fn a_tag_series_is_everything_before_its_first_digit() {
    assert_eq!(tag_series("jeryu-core-v5.0.0-split.7"), "jeryu-core-v");
    assert_eq!(tag_series("jeryu-core-v4.2.0"), "jeryu-core-v");
    assert_eq!(
        tag_series("web-dist-2026.09.19"),
        "web-dist-",
        "an unrelated tag line is its own series, so it never shadows a crate pin"
    );
    assert_eq!(tag_series("stable"), "stable");
    assert_eq!(tag_series("9"), "");
}

fn commit_file(work: &Path, path: &str, text: &str, message: &str) {
    if let Some(parent) = work.join(path).parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(work.join(path), text).unwrap();
    run_git(work, &["add", "."]);
    run_git(work, &["commit", "-q", "-m", message]);
}

fn host(core: &ForgeCore, root: &Path, owner: &str, name: &str, work: &Path, refs: &[&str]) {
    std::fs::create_dir_all(root.join(owner)).unwrap();
    run_git(
        &root.join(owner),
        &["init", "-q", "--bare", &format!("{name}.git")],
    );
    let bare = root.join(owner).join(format!("{name}.git"));
    let mut push = vec!["push", "-q", bare.to_str().unwrap()];
    push.extend(refs);
    run_git(work, &push);
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

fn work_repo(root: &Path, name: &str) -> std::path::PathBuf {
    let work = root.join(name);
    std::fs::create_dir_all(&work).unwrap();
    run_git(&work, &["init", "-q"]);
    work
}

fn edges_of(graph: &Value, kind: &str) -> Vec<Value> {
    graph["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["kind"] == kind)
        .cloned()
        .collect()
}

fn node_of<'a>(graph: &'a Value, id: &str) -> &'a Value {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == id)
        .unwrap_or_else(|| panic!("no node {id}: {graph:?}"))
}

#[tokio::test]
async fn depends_on_edges_are_opt_in_and_carry_the_pin_and_its_staleness() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();

    // The dependency: two tags in one series, plus an unrelated tag line that
    // is newer than either and must not be mistaken for the newest pin.
    let lib = work_repo(root, "work-core");
    commit_file(&lib, "lib.rs", "1", "core one");
    run_git(&lib, &["tag", "jeryu-core-v5.0.0-split.6"]);
    commit_file(&lib, "lib.rs", "2", "core two");
    run_git(&lib, &["tag", "jeryu-core-v5.0.0-split.7"]);
    run_git(&lib, &["tag", "web-dist-2026.09.19"]);
    host(
        &core,
        root,
        "jeryu",
        "jeryu-core",
        &lib,
        &[
            "main",
            "jeryu-core-v5.0.0-split.6",
            "jeryu-core-v5.0.0-split.7",
            "web-dist-2026.09.19",
        ],
    );

    // A second dependency, pinned by rev: nothing to compare a tag against.
    let web = work_repo(root, "work-web");
    commit_file(&web, "index.html", "1", "first page");
    let web_head = run_git(&web, &["rev-parse", "HEAD"]);
    host(&core, root, "jeryu", "jeryu-web", &web, &["main"]);

    // The consumer: a stale core tag, a web rev, a dependency this forge does
    // not host, and a path dependency on itself.
    let deploy = work_repo(root, "work-deploy");
    commit_file(
        &deploy,
        "crates/jeryu-api/Cargo.toml",
        &format!(
            "[dependencies]\n\
             jeryu-core = {{ git = \"http://127.0.0.1:8787/git/jeryu/jeryu-core.git\", tag = \"jeryu-core-v5.0.0-split.6\" }}\n\
             jeryu-web = {{ git = \"http://127.0.0.1:8787/git/jeryu/jeryu-web.git\", rev = \"{web_head}\" }}\n\
             elsewhere = {{ git = \"https://github.com/someone/elsewhere.git\", tag = \"v1\" }}\n\
             local = {{ path = \"../local\" }}\n"
        ),
        "manifest",
    );
    host(&core, root, "jeryu", "jeryu-deploy", &deploy, &["main"]);
    // A repository outside the release manifest: a node, but not a member.
    let other = work_repo(root, "work-other");
    commit_file(&other, "README.md", "1", "readme");
    host(&core, root, "veox", "jain-tui", &other, &["main"]);

    let admin = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new_with_git_storage(core.clone(), root.to_path_buf())
            .with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let call = |uri: &'static str| {
        router
            .clone()
            .oneshot(request(HttpMethod::GET, uri, &admin, None))
    };

    let plain = body_json(call("/api/v1/control-plane/repo-graph").await.unwrap()).await;
    assert_eq!(plain["schemaVersion"], "jeryu.repo_graph/v2");
    assert_eq!(
        edges_of(&plain, "depends_on"),
        [] as [Value; 0],
        "the dependency tree is opt-in, so the Intelligence page is untouched"
    );

    let graph = body_json(
        call("/api/v1/control-plane/repo-graph?include=depends_on")
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(graph["schemaVersion"], "jeryu.repo_graph/v2");
    let edges = edges_of(&graph, "depends_on");
    let named: Vec<(&str, &str)> = edges
        .iter()
        .map(|edge| {
            (
                edge["source"].as_str().unwrap(),
                edge["target"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        named,
        [
            ("repo:jeryu/jeryu-deploy", "repo:jeryu/jeryu-core"),
            ("repo:jeryu/jeryu-deploy", "repo:jeryu/jeryu-web"),
        ],
        "the unhosted dependency and the path dependency are not edges"
    );

    let core_edge = &edges[0];
    assert_eq!(
        core_edge["state"], "queued",
        "the pin is not the newest tag"
    );
    assert_eq!(
        core_edge["metadata"],
        serde_json::json!({
            "pinKind": "tag",
            "pinnedRef": "jeryu-core-v5.0.0-split.6",
            "newestRef": "jeryu-core-v5.0.0-split.7",
            "pinIsNewest": "false",
            "pinState": "behind",
            "behind": "1",
            "source": "crates/jeryu-api/Cargo.toml",
        }),
        "the newer unrelated tag line is not the newest of this series"
    );

    let web_edge = &edges[1];
    assert_eq!(
        (&web_edge["state"], &web_edge["metadata"]["pinKind"]),
        (&"unknown".into(), &"commit".into()),
        "a rev pin has no tag to be newest of"
    );
    assert_eq!(web_edge["metadata"]["pinnedRef"], web_head.as_str());
    assert_eq!(web_edge["metadata"]["pinIsNewest"], Value::Null);
    assert_eq!(
        (
            &web_edge["metadata"]["pinState"],
            &web_edge["metadata"]["behind"]
        ),
        (&"current".into(), &"0".into()),
        "a rev pin is still compared, by the resolver /api/v1/pins uses"
    );

    // Containment edges keep the shape they always had.
    let has_pr_or_check = graph["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["kind"] != "depends_on");
    assert!(
        has_pr_or_check
            .clone()
            .all(|edge| edge["metadata"].is_null()),
        "containment edges carry no metadata on the wire"
    );

    assert_eq!(
        node_of(&graph, "repo:jeryu/jeryu-core")["metadata"]["releaseMember"],
        "true"
    );
    assert_eq!(
        node_of(&graph, "repo:veox/jain-tui")["metadata"]["releaseMember"],
        "false",
        "the release manifest says who ships together, nothing more"
    );
}
