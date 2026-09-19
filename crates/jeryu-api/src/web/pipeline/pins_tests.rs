//! Pins: the pure parsers and rules, then the route over real bare repos.

use std::path::Path;

use axum::http::{Method as HttpMethod, StatusCode};
use jeryu_core::{CreatePullRequestRequest, CreateRepositoryRequest, ForgeCore, UserRole};
use tower::ServiceExt;

use super::attention::{Severity, pin_items};
use super::pins::{BumpPr, Consumer, Pin, RawPin, Unreleased, classify, lock_pins, manifest_pins};
use super::tests::{body_json, request};
use crate::web::shift::tests::run_git;
use crate::web::{WebState, app};

const WEB_PIN: &str = "8afe03c49bbdf1ad26d1282095561b50c840bf0e";
const DIST: &str = "2ed65a68fb058a59fc3f9543d9337c1fa6d0903c91b293d4af635c8a1f575ba7";

fn lock(commit: &str) -> String {
    format!(
        r#"web_artifact = "pinned"

[[repo]]
name = "jeryu-core"
tag = "jeryu-core-v5.0.0-split.1"
commit = "4e00f9b076ffbb0fd02fc7dcda4e64c2fd57bf2f"

[[repo]]
name = "jeryu-web"
commit = "{commit}"
web_dist_sha256 = "{DIST}"

[[repo]]
name = "jeryu-deploy"
commit = "PENDING_SELF"
"#
    )
}

#[test]
fn lock_reports_only_the_entry_the_release_build_uses() {
    assert_eq!(
        lock_pins(&lock(WEB_PIN)),
        [RawPin {
            name: "jeryu-web".to_string(),
            kind: "commit",
            pinned_ref: WEB_PIN.to_string(),
        }],
        "the stale crate tag and the PENDING self entry are not pins"
    );
    // A jain-style lock (`repo = "..."`, `commit = "PENDING"`), a lock with a
    // short hash, and a file that is not TOML: skipped, never an error.
    let jain = "[[repo]]\nrepo = \"jain\"\ntag = \"jain-v8\"\ncommit = \"PENDING\"\n";
    assert_eq!(lock_pins(jain), []);
    assert_eq!(lock_pins(&lock("8afe03c")), []);
    assert_eq!(lock_pins("not [ toml"), []);
}

#[test]
fn manifests_report_git_tags_and_revs_whatever_host_the_url_names() {
    let manifest = r#"
[package]
name = "jeryu-api"

[dependencies]
serde = "1"
jeryu-obs = { git = "https://github.com/neverhuman/jeryu-release-ops.git", tag = "ops-v1", package = "jeryu-obs" }
jeryu-bench = { git = "https://github.com/neverhuman/jeryu-release-ops.git", tag = "ops-v1" }
jeryu-enterprise = { git = "http://127.0.0.1:8787/git/jeryu/jeryu-core.git", tag = "core-v6" }
local = { path = "../local" }
floating = { git = "https://git.neverhuman.org/git/jeryu/jeryu-cache.git", branch = "main" }

[dev-dependencies]
jeryu-tool = { git = "https://git.neverhuman.org/git/jeryu/jeryu-tool", rev = "0123456789abcdef0123456789abcdef01234567" }

[target.'cfg(unix)'.dependencies]
jeryu-jira = { git = "https://git.neverhuman.org/git/jeryu/jeryu-jira.git/", tag = "jira-v2" }

[workspace.dependencies]
jeryu-core = { git = "https://git.neverhuman.org/git/jeryu/jeryu-core.git", tag = "core-v6" }
"#;
    let found: Vec<(String, &str, String)> = manifest_pins(manifest)
        .into_iter()
        .map(|pin| (pin.name, pin.kind, pin.pinned_ref))
        .collect();
    assert_eq!(
        found,
        [
            ("jeryu-core".to_string(), "tag", "core-v6".to_string()),
            ("jeryu-jira".to_string(), "tag", "jira-v2".to_string()),
            ("jeryu-release-ops".to_string(), "tag", "ops-v1".to_string()),
            (
                "jeryu-tool".to_string(),
                "commit",
                "0123456789abcdef0123456789abcdef01234567".to_string()
            ),
        ],
        "deduped by dependency and ref; path, registry and branch deps are not pins"
    );
    assert!(manifest_pins("not [ toml").is_empty());
}

#[test]
fn a_pin_is_behind_only_when_what_it_misses_may_ship() {
    assert_eq!(classify(false, false, 0, None), "unknown");
    assert_eq!(classify(true, true, 0, Some(true)), "diverged");
    assert_eq!(classify(true, false, 0, Some(false)), "current");
    assert_eq!(classify(true, false, 3, Some(false)), "behind_not_green");
    assert_eq!(classify(true, false, 3, Some(true)), "behind");
    assert_eq!(
        classify(true, false, 3, None),
        "behind",
        "no gate posts there"
    );
}

fn pin(kind: &'static str, state: &'static str, bump: Option<u64>) -> Pin {
    Pin {
        dependency: "jeryu/jeryu-web".to_string(),
        kind,
        source: match kind {
            "tag" => "crates/jeryu-api/Cargo.toml".to_string(),
            _ => "jeryu-split.lock.toml".to_string(),
        },
        pinned_ref: match kind {
            "tag" => "jeryu-web-v1".to_string(),
            _ => WEB_PIN.to_string(),
        },
        pinned_sha: Some(WEB_PIN.to_string()),
        latest_sha: Some("427bebecb848d7b7bb37ecc71521d7461072694d".to_string()),
        latest_at: Some("2026-09-19T14:40:00+00:00".to_string()),
        behind: 9,
        latest_green: Some(state == "behind"),
        state,
        bump_pr: bump.map(|number| BumpPr {
            number,
            state: "open".to_string(),
            url: format!("/repos/jeryu/jeryu/jeryu-deploy/pulls/{number}"),
        }),
        unreleased: vec![Unreleased {
            sha: "427bebecb848d7b7bb37ecc71521d7461072694d".to_string(),
            subject: "test: the dock test brings its own Storage".to_string(),
        }],
    }
}

fn consumer(pins: Vec<Pin>) -> Vec<Consumer> {
    vec![Consumer {
        repo: "jeryu/jeryu-deploy".to_string(),
        family: Some("jeryu".to_string()),
        branch: "main".to_string(),
        pins,
    }]
}

#[test]
fn commit_pin_behind_with_no_bump_open_asks_for_the_bump() {
    let items = pin_items(&consumer(vec![pin("commit", "behind", None)]));
    let [item] = items.as_slice() else {
        panic!("one item: {items:?}")
    };
    assert_eq!((item.kind, item.severity), ("pin_behind", Severity::Action));
    assert_eq!(item.id, "pin-behind:jeryu/jeryu-deploy:jeryu/jeryu-web");
    assert_eq!(
        item.title,
        "9 merged commits of jeryu-web are not in jeryu-deploy's pin"
    );
    assert!(item.reason.contains("would not include them"), "{item:?}");
    assert!(item.reason.contains("the dock test brings its own Storage"));
    assert_eq!(item.href, "/unreleased");
    assert_eq!(
        item.action.command.as_deref(),
        Some(
            "scripts/release/build-web-dist.sh --commit \
             427bebecb848d7b7bb37ecc71521d7461072694d /tmp/web-dist"
        )
    );
    assert!(item.next_step.contains("build-web-dist.sh"), "{item:?}");
    assert_eq!(item.repo.as_deref(), Some("jeryu/jeryu-deploy"));
    assert_eq!(item.family.as_deref(), Some("jeryu"));
    assert_eq!(item.sha.as_deref(), Some("427bebe"));
    assert_eq!(item.since.as_deref(), Some("2026-09-19T14:40:00+00:00"));
}

#[test]
fn commit_pin_with_a_bump_open_only_points_at_that_pull_request() {
    let items = pin_items(&consumer(vec![pin("commit", "behind", Some(53))]));
    let [item] = items.as_slice() else {
        panic!("one item: {items:?}")
    };
    assert_eq!(item.severity, Severity::Watch);
    assert_eq!(item.href, "/repos/jeryu/jeryu/jeryu-deploy/pulls/53");
    assert_eq!(item.pr, Some(53));
    assert_eq!(item.action.command, None);
    assert!(item.next_step.contains(&item.href), "{item:?}");
}

#[test]
fn tag_pin_behind_is_watched_because_nothing_cuts_tags() {
    let items = pin_items(&consumer(vec![pin("tag", "behind", None)]));
    let [item] = items.as_slice() else {
        panic!("one item: {items:?}")
    };
    assert_eq!(item.severity, Severity::Watch);
    assert_eq!(
        item.title,
        "jeryu-web has 9 merged commits since tag jeryu-web-v1"
    );
    assert!(item.reason.contains("crates/jeryu-api/Cargo.toml"));
    assert_eq!(
        (item.href.as_str(), &item.action.command),
        ("/unreleased", &None)
    );
}

#[test]
fn a_current_red_diverged_or_unknown_pin_asks_nothing() {
    let quiet = ["current", "behind_not_green", "diverged", "unknown"]
        .map(|state| pin("commit", state, None));
    assert_eq!(pin_items(&consumer(quiet.to_vec())), []);
}

fn commit_file(work: &Path, path: &str, text: &str, message: &str) -> String {
    if let Some(parent) = work.join(path).parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(work.join(path), text).unwrap();
    run_git(work, &["add", "."]);
    run_git(work, &["commit", "-q", "-m", message]);
    run_git(work, &["rev-parse", "HEAD"])
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

#[tokio::test]
async fn pins_route_is_admin_only_and_reads_the_hosted_repositories() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let core = ForgeCore::new();
    core.create_account("alice", "alice-password", UserRole::Admin)
        .unwrap();
    core.create_account("bob", "bob-password", UserRole::User)
        .unwrap();

    // jeryu-web: pinned at its first commit, two more merged since.
    let web = root.join("work-web");
    std::fs::create_dir_all(&web).unwrap();
    run_git(&web, &["init", "-q"]);
    let web_pin = commit_file(&web, "index.html", "1", "first page");
    commit_file(&web, "index.html", "2", "Add the Needs you page");
    let web_head = commit_file(&web, "index.html", "3", "Add the Activity feed");
    host(&core, root, "jeryu", "jeryu-web", &web, &["main"]);

    // jeryu-core lives under another owner; tagged at its first commit, one since.
    let lib = root.join("work-core");
    std::fs::create_dir_all(&lib).unwrap();
    run_git(&lib, &["init", "-q"]);
    commit_file(&lib, "lib.rs", "1", "core one");
    run_git(&lib, &["tag", "core-v6"]);
    commit_file(&lib, "lib.rs", "2", "Add Repository.pushed_at");
    host(
        &core,
        root,
        "veox",
        "jeryu-core",
        &lib,
        &["main", "core-v6"],
    );

    // The consumer: a lock with the web pin, a manifest with the core tag and
    // a dependency this forge does not host.
    let deploy = root.join("work-deploy");
    std::fs::create_dir_all(&deploy).unwrap();
    run_git(&deploy, &["init", "-q"]);
    commit_file(&deploy, "jeryu-split.lock.toml", &lock(&web_pin), "lock");
    commit_file(
        &deploy,
        "crates/jeryu-api/Cargo.toml",
        "[dependencies]\n\
         jeryu-enterprise = { git = \"http://127.0.0.1:8787/git/jeryu/jeryu-core.git\", tag = \"core-v6\" }\n\
         elsewhere = { git = \"https://github.com/someone/elsewhere.git\", tag = \"v1\" }\n",
        "manifest",
    );
    host(&core, root, "jeryu", "jeryu-deploy", &deploy, &["main"]);
    // A jain-style lock makes a consumer with nothing to report, not an error.
    let jain = root.join("work-jain");
    std::fs::create_dir_all(&jain).unwrap();
    run_git(&jain, &["init", "-q"]);
    commit_file(
        &jain,
        "jain-split.lock.toml",
        "[[repo]]\nrepo = \"jain\"\ncommit = \"PENDING\"\n",
        "lock",
    );
    host(&core, root, "veox", "jain-deploy", &jain, &["main"]);

    let admin = core
        .create_personal_access_token("alice", "t", None)
        .unwrap()
        .secret;
    let user = core
        .create_personal_access_token("bob", "t", None)
        .unwrap()
        .secret;
    let router = app(
        WebState::new_with_git_storage(core.clone(), root.to_path_buf())
            .with_auth(true, false, false),
        Path::new("/tmp/jeryu-no-spa"),
    );
    let call = |token: &str| {
        router
            .clone()
            .oneshot(request(HttpMethod::GET, "/api/v1/pins", token, None))
    };

    let refused = call(&user).await.unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(refused).await["code"], "permission_denied");
    let anonymous = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/v1/pins")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let response = call(&admin).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["schema_version"], "jeryu.pins/v1");
    let consumers = body["consumers"].as_array().unwrap();
    let names: Vec<&str> = consumers
        .iter()
        .map(|c| c["repo"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["jeryu/jeryu-deploy", "veox/jain-deploy"]);
    assert_eq!(consumers[1]["pins"], serde_json::json!([]));
    let pins = consumers[0]["pins"].as_array().unwrap();
    assert_eq!(
        pins.len(),
        2,
        "the unhosted dependency is skipped: {pins:?}"
    );

    let web_pin_row = &pins[0];
    assert_eq!(web_pin_row["dependency"], "jeryu/jeryu-web");
    assert_eq!(web_pin_row["kind"], "commit");
    assert_eq!(web_pin_row["source"], "jeryu-split.lock.toml");
    assert_eq!(web_pin_row["pinned_sha"], web_pin.as_str());
    assert_eq!(web_pin_row["latest_sha"], web_head.as_str());
    assert_eq!(web_pin_row["behind"], 2);
    assert_eq!(web_pin_row["latest_green"], serde_json::Value::Null);
    assert_eq!(web_pin_row["state"], "behind");
    assert_eq!(web_pin_row["bump_pr"], serde_json::Value::Null);
    let subjects: Vec<&str> = web_pin_row["unreleased"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["subject"].as_str().unwrap())
        .collect();
    assert_eq!(
        subjects,
        ["Add the Activity feed", "Add the Needs you page"]
    );

    let core_pin_row = &pins[1];
    assert_eq!(
        core_pin_row["dependency"], "veox/jeryu-core",
        "found under another owner"
    );
    assert_eq!(core_pin_row["kind"], "tag");
    assert_eq!(core_pin_row["source"], "crates/jeryu-api/Cargo.toml");
    assert_eq!(core_pin_row["pinned_ref"], "core-v6");
    assert_eq!(
        (&core_pin_row["behind"], &core_pin_row["state"]),
        (&1.into(), &"behind".into())
    );

    // An open bump shows on the pin, and the inbox then only watches it.
    core.create_pull_request(
        "jeryu",
        "jeryu-deploy",
        "alice",
        CreatePullRequestRequest {
            title: "release: pin jeryu-web 427bebe".to_string(),
            body: None,
            head: "auto/pin-web-427bebecb848".to_string(),
            base: "main".to_string(),
            head_sha: Some(web_head.clone()),
            base_sha: None,
            source_repository: None,
            draft: false,
            commits: Vec::new(),
            changed_files: Vec::new(),
        },
    )
    .unwrap();
    let state = WebState::new_with_git_storage(core, root.to_path_buf());
    let fresh = super::pins::collect(&state);
    let bump = fresh.consumers[0].pins[0].bump_pr.clone().expect("bump pr");
    assert_eq!(bump.url, "/repos/jeryu/jeryu/jeryu-deploy/pulls/1");
    let items = pin_items(&fresh.consumers);
    let severities: Vec<Severity> = items.iter().map(|item| item.severity).collect();
    assert_eq!(severities, [Severity::Watch, Severity::Watch]);
}
